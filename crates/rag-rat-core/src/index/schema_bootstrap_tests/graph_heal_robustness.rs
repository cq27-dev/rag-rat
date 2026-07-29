//! `ensure_graph_index_current` runs on the OPEN path, so anything it treats as fatal wedges the
//! database for every later open too — the heal is retried and re-fails forever, and no CLI or MCP
//! call can get in to repair it. These tests pin the states that must NOT be fatal.
//!
//! All three arise on ordinary indexes: a deletion tombstone (every repo that ever removed a
//! file), a row whose path is absent from this checkout (the heal repopulates the repo's whole
//! generation, including sibling commits/worktrees), and a Rust file above the parse limit.

use super::*;

/// Stage an index that owes BOTH heals: the graph-version bump (which drives the reindex loop)
/// and the logical-key bump (which turns on the Rust scope refresh).
fn owe_both_heals(db: &IndexDatabase) {
    db.set_repo_meta("graph_index_version", "0").unwrap();
    db.storage
        .connection()
        .execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", [])
        .unwrap();
    // These tests rewind the version meta behind a connection that has already run passes, so a
    // drift snapshot memoized while the version still read CURRENT (i.e. `None`, nothing to heal)
    // can still be pending. A genuine pre-upgrade open carries no memo; drop it so the heal
    // captures the snapshot itself, which is also what decides whether it may stamp.
    *db.drift_snapshot.lock().expect("drift snapshot lock") = None;
}

fn indexed_root(files: &[(&str, &str)]) -> (ScratchRoot, rag_rat_base::config::Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    for (name, body) in files {
        fs::write(root.join("src").join(name), body).unwrap();
    }
    let config = source_config(root.to_path_buf(), Language::Rust);
    (root, config)
}

/// A `kind='deleted'` tombstone carries `language='unknown'` (`mark_file_deleted`). Neither token
/// parses into `Language`/`TargetKind`, so a heal that walks raw `main.files` rows without
/// excluding tombstones fails at the FIRST one — on every open, permanently. Any repo that has
/// ever deleted an indexed file holds these rows.
#[test]
fn a_deletion_tombstone_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Delete an indexed file and let the incremental pass write its tombstone, exactly as a
    // normal edit-and-reindex cycle does.
    fs::write(root.join("src/gone.rs"), "pub fn vanishing() {}\n").unwrap();
    let db = {
        drop(db);
        IndexDatabase::index_discover(&config).unwrap()
    };
    fs::remove_file(root.join("src/gone.rs")).unwrap();
    drop(db);
    let db = IndexDatabase::index_discover(&config).unwrap();
    let tombstones: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE kind = 'deleted'", [], |row| row.get(0))
        .unwrap();
    assert!(tombstones > 0, "the delete-and-reindex cycle must leave a tombstone to test against");

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("a tombstone row must not fail the heal");
    assert_eq!(
        db.repo_meta("graph_index_version").unwrap().as_deref(),
        Some(GRAPH_INDEX_VERSION),
        "the heal ran to completion and stamped its version"
    );
    // Tombstones are filtered before the loop, so every row this heal walked was covered — the
    // one shape that may stamp the key version and let later passes take the scoped re-derive.
    assert_eq!(
        db.repo_meta("logical_key_version").unwrap().as_deref(),
        Some(LOGICAL_KEY_VERSION),
        "full coverage stamps the key version"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The heal repopulates the repo's whole generation, so it walks rows for paths that need not
/// exist under THIS root (a sibling commit/worktree scope) — and a row can simply outlive its file
/// between an edit and the next index pass. An unreadable path is an expected outcome of that
/// repopulation, not a failure.
#[test]
fn a_file_row_without_a_file_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A live row (not a tombstone) whose path has no file behind it.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
                 indexed_at_ms, indexed_revision, commit_sha, worktree_id, repo_id, generation,
                 has_test_code)
             SELECT 'src/absent.rs', 'rust', 'source', '', 0, 0, 0, '', commit_sha, worktree_id,
                    repo_id, generation, 0
             FROM main.files LIMIT 1",
            [],
        )
        .unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("an unreadable path must not fail the heal");

    let _ = fs::remove_dir_all(&root);
}

/// Above `MAX_GRAPH_PARSE_BYTES` the indexer persists no symbols at all (the chunker declines at
/// the same bound), so there is nothing for the scope refresh to certify. Treating the file as an
/// un-certifiable heal blocker would fail the open over a file the index never parsed.
#[test]
fn an_oversized_rust_file_does_not_wedge_the_graph_heal() {
    let filler = "pub fn pad() {}\n".repeat(40_000);
    assert!(filler.len() > edges::MAX_GRAPH_PARSE_BYTES, "the fixture must exceed the parse limit");
    let (root, config) = indexed_root(&[
        ("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n"),
        ("huge.rs", filler.as_str()),
    ]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("an oversized Rust file must not fail the heal");

    let _ = fs::remove_dir_all(&root);
}

/// Every `to_name` on `file_id`'s edges — the file's outgoing graph as EXTRACTED, independent of
/// which scope is active (edge rows are keyed by `source_file_id`, and base and overlay hold
/// separate rows for a shadowed path).
fn edge_target_names(db: &IndexDatabase, file_id: i64) -> Vec<String> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT n.value FROM edges_data d
             JOIN name_strings n ON n.id = d.to_name_id
             WHERE d.source_file_id = ?1
             ORDER BY n.value",
        )
        .unwrap();
    let names = stmt.query_map([file_id], |row| row.get::<_, String>(0)).unwrap();
    names.map(Result::unwrap).collect()
}

/// The `main.files` row id for `path` in the scope identified by `worktree_id` (`''` = base).
fn scoped_file_id(db: &IndexDatabase, path: &str, worktree_id: &str) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT id FROM main.files WHERE path = ?1 AND worktree_id = ?2 AND repo_id = ?3",
            params![path, worktree_id, &db.active_repo_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// The heal walks the repo's WHOLE generation — every commit/worktree scope — but reads from ONE
/// checkout. A linked worktree's row for a path that also exists in the active checkout must
/// therefore keep ITS OWN graph: re-deriving it from the active root would stamp the base
/// checkout's calls onto the branch's rows, silently corrupting every graph answer served from
/// that worktree. The row's `sha256` is what decides — bytes that do not hash to the row are not
/// that row's source.
#[test]
fn a_graph_heal_does_not_rewrite_a_sibling_worktrees_edges() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(
        main.join("src/target.rs"),
        "pub fn base_helper() {}\npub fn entry() { base_helper(); }\n",
    )
    .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // The branch rewrites the file's body, so base and overlay hold DIFFERENT content for the
    // same path — the one shape a single-root heal cannot serve both of.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(
        linked.join("src/target.rs"),
        "pub fn overlay_helper() {}\npub fn entry() { overlay_helper(); }\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch body"]);
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "target.rs indexed as an overlay row");

    set_base_scope(&mut db, &main);
    let base_id = scoped_file_id(&db, "src/target.rs", "");
    let overlay_id = scoped_file_id(&db, "src/target.rs", &worktree_id_of(&linked));
    assert_ne!(base_id, overlay_id, "base and overlay must hold separate rows for the path");
    let overlay_before = edge_target_names(&db, overlay_id);
    assert!(
        overlay_before.iter().any(|name| name == "overlay_helper"),
        "the overlay row's graph is derived from the BRANCH body: {overlay_before:?}"
    );

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("the heal runs from the base checkout");

    assert_eq!(
        edge_target_names(&db, overlay_id),
        overlay_before,
        "the heal read only the base checkout, so the overlay row's graph must be untouched"
    );
    let base_after = edge_target_names(&db, base_id);
    assert!(
        base_after.iter().any(|name| name == "base_helper"),
        "the active checkout's own row IS re-derived: {base_after:?}"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// A row the digest gate skips keeps its OLD `scope_path`, and `regroup_logical_symbols` reads
/// raw `main.files` across every scope — so that stale scope does not merely lag, it changes
/// IDENTITY: the cross-scope group splits, the stale row keeps the original `stable_id`, and the
/// refreshed row gets a new one. Stamping the key version over that would also disarm the two
/// mechanisms built to recover from it (the drift snapshot, and the scoped-re-derive gate, which
/// by construction never revisits untouched rows). So partial coverage must DEFER the stamp.
#[test]
fn partial_coverage_defers_the_logical_key_stamp() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(
        main.join("src/shared.rs"),
        "pub struct S;\nimpl Greet for S { fn hello(&self) {} }\n",
    )
    .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // A branch whose copy of the path differs, so its row can never be vouched for from here.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(
        linked.join("src/shared.rs"),
        "pub struct S;\nimpl Greet for S { fn hello(&self) {} }\npub fn only_here() {}\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    set_base_scope(&mut db, &main);
    owe_both_heals(&db);
    db.ensure_graph_index_current().unwrap();

    assert_eq!(
        db.repo_meta("graph_index_version").unwrap().as_deref(),
        Some(GRAPH_INDEX_VERSION),
        "the graph pass itself ran to completion"
    );
    assert_ne!(
        db.repo_meta("logical_key_version").unwrap().as_deref(),
        Some(LOGICAL_KEY_VERSION),
        "a row whose scope could not be refreshed must leave the key version unstamped, so the \
         drift heal and the full-rederive gate stay armed"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// The flip side of the digest gate: it must not turn the heal into a no-op. With a linked
/// worktree's rows in the same database, the ACTIVE checkout's own rows still hash to their
/// source, so the heal re-derives them — a gate that read the wrong column, or compared against
/// a normalized form of the text, would quietly skip everything and leave the graph unbuilt.
/// Full coverage must also STAMP the key version, or every later pass pays a whole-corpus regroup.
#[test]
fn a_graph_heal_still_repopulates_the_active_checkouts_rows() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/shared.rs"), "pub fn helper() {}\npub fn entry() { helper(); }\n")
        .unwrap();
    fs::write(main.join("src/touched.rs"), "pub fn only_on_main() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // The branch touches a DIFFERENT file, so `shared.rs` keeps identical content in both scopes.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/touched.rs"), "pub fn only_on_branch() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    set_base_scope(&mut db, &main);
    let shared_id = scoped_file_id(&db, "src/shared.rs", "");
    db.storage
        .connection()
        .execute("DELETE FROM edges_data WHERE source_file_id = ?1", [shared_id])
        .unwrap();
    assert!(edge_target_names(&db, shared_id).is_empty(), "the row starts the heal with no graph");

    owe_both_heals(&db);
    db.ensure_graph_index_current().unwrap();

    let after = edge_target_names(&db, shared_id);
    assert!(
        after.iter().any(|name| name == "helper"),
        "a content-identical row is re-derived, not skipped: {after:?}"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// A file edited since it was indexed no longer hashes to its row, so the digest gate rejects it
/// before the scope refresh or the edge re-derivation ever run. That is the ordinary state between
/// an edit and the next watcher pass, and it must not fail the open — the row keeps the graph it
/// has and the next index of that file lands the current shape.
#[test]
fn a_file_edited_since_indexing_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let file_id = scoped_file_id(&db, "src/lib.rs", &db.active_worktree_id.clone());
    let before = edge_target_names(&db, file_id);
    assert!(!before.is_empty(), "the row starts with a graph to preserve");

    // Prepend a symbol WITHOUT reindexing: every stored span is now stale by the same offset.
    fs::write(
        root.join("src/lib.rs"),
        "pub fn inserted_ahead_of_everything() {}\npub struct Alpha;\nimpl Alpha { pub fn \
         run(&self) {} }\n",
    )
    .unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("a file edited since indexing must not fail the heal");

    assert_eq!(
        edge_target_names(&db, file_id),
        before,
        "a row the gate rejects keeps its edges rather than losing or re-deriving them"
    );

    let _ = fs::remove_dir_all(&root);
}

/// `refresh_symbol_scopes` reports a shortfall when it cannot reach every persisted symbol of a
/// file — the `unrefreshed` half of the heal's bookkeeping. The digest gate now short-circuits the
/// obvious way in (an edited file), so this reaches it the only remaining way: a persisted symbol
/// row the current parse does not produce. The heal must count it and carry on, NOT fail the open
/// and NOT skip the file's edge re-derivation.
#[test]
fn a_symbol_row_the_parser_cannot_match_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let file_id = scoped_file_id(&db, "src/lib.rs", &db.active_worktree_id.clone());

    // A phantom symbol at a span the file does not contain: the span-matched UPDATE can never
    // reach it, so the refresh covers fewer rows than the file holds.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.symbols(file_id, name, kind, language, qualified_name_id, \
             scope_path,
                 signature, start_line, end_line, start_byte, end_byte)
             SELECT file_id, 'phantom', 'function', language, qualified_name_id, 'phantom',
                    signature, 900, 901, 90000, 90010
             FROM main.symbols WHERE file_id = ?1 LIMIT 1",
            [file_id],
        )
        .unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("an unmatchable symbol row must not fail the heal");

    assert_eq!(
        db.repo_meta("graph_index_version").unwrap().as_deref(),
        Some(GRAPH_INDEX_VERSION),
        "the heal ran to completion and stamped its version"
    );
    assert!(
        !edge_target_names(&db, file_id).is_empty(),
        "the file's edges are still re-derived — the shortfall is reported, not fatal"
    );

    let _ = fs::remove_dir_all(&root);
}
