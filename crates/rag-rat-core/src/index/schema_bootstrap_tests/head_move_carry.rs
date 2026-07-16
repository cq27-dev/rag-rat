//! HEAD-move carry (#502): a `git pull` / branch checkout re-keys the base commit scope, but the
//! content of nearly every file is byte-identical with the previous scope. Discovery adopts the
//! retained committed rows by re-stamping `files.commit_sha` in place — the row id, and every
//! chunk/symbol/edge/embedding/memory-binding hanging off it, survives — so a HEAD move costs
//! roughly its diff, not a full reindex.

use rag_rat_base::hash::hex_sha256;

use super::*;

/// A repo with two committed rust files, returning `(root, config)`. `keep.rs` stays unchanged
/// across every HEAD move in these tests; `edit.rs` is the file the moves modify.
fn head_move_repo(tag: &str) -> (PathBuf, Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/keep.rs"), format!("pub fn {tag}_kept_anchor() -> i32 {{ 1 }}\n"))
        .unwrap();
    fs::write(root.join("src/edit.rs"), "pub fn edit_v1() -> i32 { 1 }\n").unwrap();
    // Keep the test index out of the tree: a later `git add .` would otherwise commit the
    // database file and block branch switches over it.
    fs::write(root.join(".gitignore"), ".rag-rat/\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    (root, config)
}

/// The raw `main.files` rows `(id, commit_sha, sha256)` for `path` in the ACTIVE repo's base
/// scope — bypassing the commit dimension of the scope view so retained (out-of-scope) rows are
/// observable, while staying repo-scoped so the poison sibling's same-path row is not counted.
fn committed_rows(db: &IndexDatabase, path: &str) -> Vec<(i64, String, String)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT id, commit_sha, sha256 FROM main.files
             WHERE path = ?1 AND worktree_id = '' AND kind != 'deleted'
               AND repo_id = (SELECT value FROM temp.connection_context WHERE key = 'repo_id')
             ORDER BY id",
        )
        .unwrap();
    stmt.query_map([path], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// The active-scope (view) row id for `path`, or None when the path is out of scope.
fn active_row_id(db: &IndexDatabase, path: &str) -> Option<i64> {
    db.storage
        .connection()
        .query_row("SELECT id FROM files WHERE path = ?1", [path], |row| row.get(0))
        .ok()
}

fn chunk_ids_of_file(db: &IndexDatabase, file_id: i64) -> Vec<i64> {
    let conn = db.storage.connection();
    let mut stmt = conn.prepare("SELECT id FROM chunks WHERE file_id = ?1 ORDER BY id").unwrap();
    stmt.query_map([file_id], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
}

/// #561: the incremental write phase skips overwriting a scope key whose current row already has a
/// NEWER disk mtime than the version it prepared (a concurrent lockless heal). The interleaving
/// isn't unit-testable, but `scope_row_modified_at_ms` — the value the guard compares against the
/// prepared mtime — is: it returns the row's `modified_at_ms` for the exact scope key, `None` for
/// any other.
#[test]
fn scope_row_modified_at_ms_reads_the_scoped_disk_mtime() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();
    fs::write(root.join(".gitignore"), ".rag-rat/\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // The committed row's own scope + disk mtime.
    let (commit_sha, worktree_id, modified_at_ms): (String, String, i64) = db
        .storage
        .connection()
        .query_row(
            "SELECT commit_sha, worktree_id, modified_at_ms FROM files WHERE path = 'src/lib.rs'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let path = std::path::Path::new("src/lib.rs");

    // The guard reads this exact scope's mtime — the value it compares against the prepared mtime.
    assert_eq!(
        db.scope_row_modified_at_ms(path, &commit_sha, &worktree_id).unwrap(),
        Some(modified_at_ms)
    );
    // Scope-keyed: a different commit scope or a different path is a different row → None.
    assert_eq!(
        db.scope_row_modified_at_ms(path, "deadbeefdeadbeef", &worktree_id).unwrap(),
        None,
        "a different commit_sha is a different scope key"
    );
    assert_eq!(
        db.scope_row_modified_at_ms(
            std::path::Path::new("src/other.rs"),
            &commit_sha,
            &worktree_id
        )
        .unwrap(),
        None,
        "a different path is a different scope key"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_head_move_carries_unchanged_rows_and_rederives_only_the_diff() {
    let (root, config) = head_move_repo("carry");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let old_head = head_sha(&root);
    let keep_id = active_row_id(&db, "src/keep.rs").expect("keep.rs indexed");
    let keep_chunks = chunk_ids_of_file(&db, keep_id);
    let edit_id = active_row_id(&db, "src/edit.rs").expect("edit.rs indexed");
    drop(db);

    // A commit editing ONE file moves HEAD; keep.rs is byte-identical across the move.
    fs::write(root.join("src/edit.rs"), "pub fn edit_v2() -> i32 { 2 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "edit"]);
    let new_head = head_sha(&root);
    assert_ne!(old_head, new_head);

    let (db, content_changed) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert!(content_changed, "the edited file is a real content change");

    // keep.rs: the SAME row (and its chunks) re-stamped to the new commit — no re-derive.
    let keep_rows = committed_rows(&db, "src/keep.rs");
    assert_eq!(
        keep_rows.len(),
        1,
        "an unchanged file must be carried, not duplicated across scopes: {keep_rows:?}"
    );
    assert_eq!(keep_rows[0].0, keep_id, "carry must re-stamp the row in place (same id)");
    assert_eq!(keep_rows[0].1, new_head, "the carried row lives at the new HEAD");
    assert_eq!(
        chunk_ids_of_file(&db, keep_id),
        keep_chunks,
        "carried chunks (and everything keyed by them) survive"
    );

    // edit.rs: re-derived at the new HEAD; the old row stays behind at the old sha (out of
    // scope, gc's business), so the active view serves exactly one fresh row.
    let edit_rows = committed_rows(&db, "src/edit.rs");
    assert_eq!(
        edit_rows.len(),
        2,
        "changed path: old row retained + new row derived; old={old_head} new={new_head} \
         rows={edit_rows:?}"
    );
    assert!(edit_rows.iter().any(|(id, sha, _)| *id == edit_id && sha == &old_head));
    let active_edit = active_row_id(&db, "src/edit.rs").expect("edit.rs active");
    assert_ne!(active_edit, edit_id, "the changed file is a fresh derive");
    let symbols: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM symbols s JOIN files f ON f.id = s.file_id
             WHERE s.name = 'edit_v2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(symbols, 1, "the re-derived file's new symbol is in scope");

    // A carried pass must never touch a sibling repo's rows (round-6 harness).
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_branch_switch_back_carries_everything_with_no_rederive() {
    let (root, config) = head_move_repo("swback");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let main_head = head_sha(&root);
    let keep_id = active_row_id(&db, "src/keep.rs").unwrap();
    let edit_id = active_row_id(&db, "src/edit.rs").unwrap();
    drop(db);

    // A feature branch edits one file; index it there.
    run_git(&root, &["checkout", "-q", "-b", "feat"]);
    fs::write(root.join("src/edit.rs"), "pub fn edit_branch() -> i32 { 2 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "branch edit"]);
    let (db, _) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert_eq!(
        active_row_id(&db, "src/keep.rs"),
        Some(keep_id),
        "the unchanged file is carried onto the branch head"
    );
    drop(db);

    // Switching back re-adopts BOTH rows: the carried one comes back, and the changed path's
    // old row — left behind at the main head with main's content — matches disk again. Nothing
    // re-derives, so the pass reports no content change.
    run_git(&root, &["checkout", "-q", "main"]);
    let (db, content_changed) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert!(!content_changed, "a switch back to already-derived content re-derives nothing");
    assert_eq!(active_row_id(&db, "src/keep.rs"), Some(keep_id));
    assert_eq!(
        active_row_id(&db, "src/edit.rs"),
        Some(edit_id),
        "the old-branch row still holding this content is re-adopted, not re-derived"
    );
    let keep_rows = committed_rows(&db, "src/keep.rs");
    assert_eq!(keep_rows.len(), 1);
    assert_eq!(keep_rows[0].1, main_head);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn carry_requires_matching_language_and_kind() {
    let (root, config) = head_move_repo("drift");
    let db = IndexDatabase::rebuild(&config).unwrap();
    // A forged retained row at a stale commit whose sha256 matches the file about to appear,
    // but under a different language — the target-drift twin of discovery's sha check. Carry
    // must refuse it and derive the path properly.
    let new_bytes = "pub fn freshly_added() -> i32 { 3 }\n";
    let (repo_id, generation): (String, i64) = db
        .storage
        .connection()
        .query_row(
            "SELECT repo_id, generation FROM main.files WHERE path = 'src/keep.rs' AND \
             worktree_id = ''",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
                     indexed_at_ms, indexed_revision, commit_sha, worktree_id, repo_id, generation)
             VALUES ('src/new.rs', 'markdown', 'docs', ?1, 0, 0, 0, '', 'stalecommit', '', ?2, ?3)",
            params![hex_sha256(new_bytes.as_bytes()), repo_id, generation],
        )
        .unwrap();
    let forged_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT MAX(id) FROM main.files WHERE path = 'src/new.rs'", [], |row| row.get(0))
        .unwrap();
    drop(db);

    fs::write(root.join("src/new.rs"), new_bytes).unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "add new.rs"]);

    let (db, _) = IndexDatabase::index_discover_reporting(&config).unwrap();
    let active = active_row_id(&db, "src/new.rs").expect("new.rs indexed");
    assert_ne!(active, forged_id, "a language/kind mismatch must not be carried");
    let language: String = db
        .storage
        .connection()
        .query_row("SELECT language FROM files WHERE path = 'src/new.rs'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(language, "rust");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn dirty_paths_keep_their_overlay_and_are_not_carried() {
    let (root, config) = head_move_repo("dirty");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let old_head = head_sha(&root);
    drop(db);

    // keep.rs goes dirty (overlay row); a commit to edit.rs then moves HEAD.
    fs::write(root.join("src/keep.rs"), "pub fn dirty_kept_anchor() -> i32 { 9 }\n").unwrap();
    let db = IndexDatabase::index_discover(&config).unwrap();
    let overlay: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE path = 'src/keep.rs' AND worktree_id != ''",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(overlay, 1, "dirty edit creates the overlay row");
    drop(db);

    fs::write(root.join("src/edit.rs"), "pub fn edit_v2() -> i32 { 2 }\n").unwrap();
    run_git(&root, &["add", "src/edit.rs"]);
    run_git(&root, &["commit", "-q", "-m", "edit only edit.rs"]);

    let (db, _) = IndexDatabase::index_discover_reporting(&config).unwrap();
    // The dirty path is served by its overlay (which shadows any committed row), so it is not a
    // carry candidate: its committed row may stay at the old sha without harming the view.
    let sha: String = db
        .storage
        .connection()
        .query_row("SELECT sha256 FROM files WHERE path = 'src/keep.rs'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        sha,
        hex_sha256(b"pub fn dirty_kept_anchor() -> i32 { 9 }\n"),
        "the overlay row keeps serving the dirty content"
    );
    let committed = committed_rows(&db, "src/keep.rs");
    assert_eq!(committed.len(), 1, "no duplicate committed row is derived for a dirty path");
    assert_eq!(committed[0].1, old_head, "the shadowed committed row is left in place");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_pull_after_a_branch_round_trip_still_carries_despite_stale_higher_id_rows() {
    // main → feat (edit.rs re-derived there, HIGHER row id) → main → commit on main. The pull's
    // carry must adopt the OLD-main row that matches disk, not give up because the stale feat
    // row (higher id, different sha) shadows it for the same path.
    let (root, config) = head_move_repo("multiret");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let keep_id = active_row_id(&db, "src/keep.rs").unwrap();
    let edit_id = active_row_id(&db, "src/edit.rs").unwrap();
    drop(db);

    run_git(&root, &["checkout", "-q", "-b", "feat"]);
    fs::write(root.join("src/edit.rs"), "pub fn edit_branch() -> i32 { 2 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "branch edit"]);
    let (db, _) = IndexDatabase::index_discover_reporting(&config).unwrap();
    drop(db);
    run_git(&root, &["checkout", "-q", "main"]);
    let (db, _) = IndexDatabase::index_discover_reporting(&config).unwrap();
    drop(db);

    // The "pull": a commit on main that does not touch edit.rs. edit.rs now has retained rows
    // at BOTH the feat head (higher id, branch content) and the old main head (its own row,
    // matching disk) — the matching row must win.
    fs::write(root.join("src/keep.rs"), "pub fn multiret_kept_anchor() -> i32 { 7 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "pull"]);
    let new_head = head_sha(&root);

    let (db, _) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert_eq!(
        active_row_id(&db, "src/edit.rs"),
        Some(edit_id),
        "the sha-matching retained row is carried even when a stale row has a higher id"
    );
    let edit_active = committed_rows(&db, "src/edit.rs")
        .into_iter()
        .filter(|(_, sha, _)| sha == &new_head)
        .collect::<Vec<_>>();
    assert_eq!(edit_active.len(), 1, "exactly the carried row serves the new HEAD");
    assert_ne!(
        active_row_id(&db, "src/keep.rs"),
        Some(keep_id),
        "the genuinely changed file is re-derived"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn discovery_status_surfaces_a_pending_carry_instead_of_reporting_clean() {
    // With no watcher running, `doctor`/`status` is the only signal after a HEAD move. A
    // carry-only scope must not read as "everything indexed": the scope view misses the rows
    // until a discover pass applies the re-stamps, so status reports the pending carry and
    // names the remedy.
    let (root, config) = head_move_repo("status");
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    fs::write(root.join("src/edit.rs"), "pub fn edit_v2() -> i32 { 2 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "edit"]);

    let db = IndexDatabase::open_config(&config).unwrap();
    let status = db.discovery_status(&config).unwrap();
    assert_eq!(status.carryable_files, 1, "the unchanged file awaits its carry");
    let warning = status.warning.expect("a pending carry is pending work, not a clean index");
    assert!(warning.contains("index --discover"), "the warning names the remedy: {warning}");
    drop(db);

    let (db, _) = IndexDatabase::index_discover_reporting(&config).unwrap();
    let status = db.discovery_status(&config).unwrap();
    assert_eq!(status.carryable_files, 0, "the applied carry clears the pending count");
    assert_eq!(status.warning, None, "a carried-and-derived scope is clean");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_untracked_recreation_of_old_content_is_not_carried_into_the_committed_scope() {
    // A file deleted at the new HEAD but re-created UNTRACKED with its old bytes matches a
    // retained row on sha — but the committed tree at the new HEAD does not contain it, and
    // committed rows are shared with every linked worktree's base view. It must be indexed as
    // this worktree's OVERLAY row, never re-stamped into the committed scope.
    let (root, config) = head_move_repo("untracked");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let old_head = head_sha(&root);
    drop(db);

    let old_bytes = fs::read(root.join("src/edit.rs")).unwrap();
    run_git(&root, &["rm", "-q", "src/edit.rs"]);
    run_git(&root, &["commit", "-q", "-m", "delete edit.rs"]);
    let new_head = head_sha(&root);
    fs::write(root.join("src/edit.rs"), &old_bytes).unwrap();

    let (db, _) = IndexDatabase::index_discover_reporting(&config).unwrap();
    let committed = committed_rows(&db, "src/edit.rs");
    assert!(
        committed.iter().all(|(_, sha, _)| sha == &old_head),
        "untracked working-tree content must not become a committed row at the new HEAD: \
         {committed:?}"
    );
    let overlay: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM files WHERE path = 'src/edit.rs' AND worktree_id != ''",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(overlay, 1, "the re-created file is served as this worktree's overlay");
    assert_eq!(
        committed_rows(&db, "src/keep.rs")[0].1,
        new_head,
        "the genuinely committed unchanged file is still carried"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_dirty_revert_to_old_content_is_not_carried_into_the_committed_scope() {
    // The tracked twin: the file changes at the new HEAD, and the working tree then reverts it
    // to the OLD commit's bytes without committing. The path is dirty, so its true committed
    // content at the new HEAD is the edited version — the old-content retained row must not be
    // re-stamped as committed; the dirty bytes belong to the overlay scope.
    let (root, config) = head_move_repo("dirtyrevert");
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    let old_bytes = fs::read(root.join("src/edit.rs")).unwrap();
    fs::write(root.join("src/edit.rs"), "pub fn edit_v2() -> i32 { 2 }\n").unwrap();
    run_git(&root, &["add", "src/edit.rs"]);
    run_git(&root, &["commit", "-q", "-m", "edit"]);
    let new_head = head_sha(&root);
    fs::write(root.join("src/edit.rs"), &old_bytes).unwrap();

    let (db, _) = IndexDatabase::index_discover_reporting(&config).unwrap();
    let committed = committed_rows(&db, "src/edit.rs");
    assert!(
        !committed
            .iter()
            .any(|(_, sha, bytes_sha)| sha == &new_head && bytes_sha == &hex_sha256(&old_bytes)),
        "dirty working-tree bytes must not be recorded as committed at the new HEAD: {committed:?}"
    );
    let overlay_sha: String = db
        .storage
        .connection()
        .query_row(
            "SELECT sha256 FROM files WHERE path = 'src/edit.rs' AND worktree_id != ''",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        overlay_sha,
        hex_sha256(&old_bytes),
        "the reverted bytes are served from this worktree's overlay"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_carried_callers_edge_reresolves_to_the_rederived_callee() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/callee.rs"), "pub fn target_fn() -> i32 { 1 }\n").unwrap();
    fs::write(root.join("src/caller.rs"), "pub fn caller_fn() -> i32 { target_fn() }\n").unwrap();
    fs::write(root.join(".gitignore"), ".rag-rat/\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let caller_id = active_row_id(&db, "src/caller.rs").unwrap();
    drop(db);

    // The callee is re-derived across the HEAD move (its symbols get fresh rowids); the carried
    // caller's edge must re-point at the fresh symbol, not the retired one.
    fs::write(
        root.join("src/callee.rs"),
        "pub fn target_fn() -> i32 { 2 }\npub fn second_fn() -> i32 { 3 }\n",
    )
    .unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "edit callee"]);
    let (db, _) = IndexDatabase::index_discover_reporting(&config).unwrap();

    assert_eq!(active_row_id(&db, "src/caller.rs"), Some(caller_id), "caller is carried");
    let fresh_target: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT s.id FROM symbols s JOIN files f ON f.id = s.file_id
             WHERE s.name = 'target_fn'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let resolved_to: Option<i64> = db
        .storage
        .connection()
        .query_row(
            "SELECT to_symbol_id FROM edges
             WHERE source_file_id = ?1 AND to_name = 'target_fn'",
            [caller_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        resolved_to,
        Some(fresh_target),
        "the carried caller's edge re-resolves to the re-derived callee symbol"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_carry_only_pass_still_refreshes_package_roots() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"carrypkg\"\nversion = \"0.1.0\"\n")
        .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn pkg_anchor() -> i32 { 1 }\n").unwrap();
    fs::write(root.join("README.md"), "seed\n").unwrap();
    fs::write(root.join(".gitignore"), ".rag-rat/\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    // The commit touches only a non-target file: the pass indexes nothing, carries everything —
    // and must still re-key the package map to the new scope, or import resolution falls open
    // until the next real edit.
    fs::write(root.join("README.md"), "moved\n").unwrap();
    run_git(&root, &["add", "README.md"]);
    run_git(&root, &["commit", "-q", "-m", "docs only"]);
    let new_head = head_sha(&root);

    let (db, content_changed) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert!(!content_changed, "no indexed content changed across the move");
    let package_rows: Vec<(String, String)> = {
        let conn = db.storage.connection();
        let mut stmt =
            conn.prepare("SELECT manifest_dir, commit_sha FROM packages ORDER BY id").unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert!(
        package_rows.iter().any(|(_, sha)| sha == &new_head),
        "package roots follow the carried scope to the new HEAD {new_head}: {package_rows:?}"
    );
    let _ = fs::remove_dir_all(root);
}
