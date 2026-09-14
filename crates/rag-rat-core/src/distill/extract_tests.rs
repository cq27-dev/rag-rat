use rag_rat_db::schema;
use rusqlite::{Connection, params};

use super::{ExtractOptions, SourceKind, SourcePart, SourceRole, enqueue_eligible, extract};

/// A fully-migrated in-memory DB scoped to `repo`, via the same `temp.connection_context` write
/// `install_scope_view` uses (the multi-repo-scope test convention).
fn scoped_conn(repo: &str) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
        [repo],
    )
    .unwrap();
    conn
}

fn seed_item(
    conn: &Connection,
    repo: &str,
    kind: &str,
    key: &str,
    body: &str,
    state_normalized: &str,
    merge_commit_sha: Option<&str>,
) {
    conn.execute(
        "INSERT INTO papertrail_items
                 (tracker, project, item_kind, item_key, url, state, title, body, synced_at_ms,
                  repo_id, state_normalized, merge_commit_sha)
             VALUES ('github','o/r',?1,?2,'u','closed','t',?3,1,?4,?5,?6)",
        params![kind, key, body, repo, state_normalized, merge_commit_sha],
    )
    .unwrap();
}

fn seed_comment(conn: &Connection, repo: &str, kind: &str, key: &str, id: &str, review: bool) {
    conn.execute(
        "INSERT INTO papertrail_comments
                 (tracker, project, item_kind, item_key, comment_id, body, synced_at_ms, repo_id,
                  review_state, created_at)
             VALUES ('github','o/r',?1,?2,?3,'a comment',1,?4,?5,'2026-01-01')",
        params![kind, key, id, repo, if review { Some("approved") } else { None }],
    )
    .unwrap();
}

/// Project-parameterized item seed, for multi-binding isolation tests.
fn seed_item_in(
    conn: &Connection,
    repo: &str,
    project: &str,
    kind: &str,
    key: &str,
    body: &str,
    state: &str,
) {
    conn.execute(
        "INSERT INTO papertrail_items
                 (tracker, project, item_kind, item_key, url, state, title, body, synced_at_ms,
                  repo_id, state_normalized, merge_commit_sha)
             VALUES ('github',?1,?2,?3,'u','closed','t',?4,1,?5,?6,NULL)",
        params![project, kind, key, body, repo, state],
    )
    .unwrap();
}

fn seed_closing_edge(
    conn: &Connection,
    repo: &str,
    issue: &str,
    closer_key: &str,
    commit: Option<&str>,
    source: &str,
) {
    conn.execute(
        "INSERT INTO papertrail_closing_edges
                 (tracker, project, issue_kind, issue_key, closer_kind, closer_key, closer_commit,
                  source, synced_at_ms, repo_id)
             VALUES ('github','o/r','issue',?1,'change_request',?2,?3,?4,1,?5)",
        params![issue, closer_key, commit, source, repo],
    )
    .unwrap();
}

fn seed_commit(conn: &Connection, repo: &str, sha: &str, subject: &str) {
    seed_commit_with_body(conn, repo, sha, subject, "");
}

fn seed_commit_with_body(conn: &Connection, repo: &str, sha: &str, subject: &str, body: &str) {
    conn.execute(
        "INSERT INTO git_commits
                 (hash, author_name, author_email, authored_at_s, committed_at_s, subject, body, \
         repo_id)
             VALUES (?1,'a','a@e',1,1,?2,?4,?3)",
        params![sha, subject, repo, body],
    )
    .unwrap();
}

fn seed_changed_file(conn: &Connection, repo: &str, sha: &str, path: &str) {
    conn.execute(
        "INSERT INTO git_file_changes (commit_hash, path, change_kind, repo_id)
             VALUES (?1, ?2, 'modified', ?3)",
        params![sha, path, repo],
    )
    .unwrap();
    seed_indexed_source(conn, repo, path);
}

/// The indexed `files` row + one logical/un-logical symbol at `path`, WITHOUT any
/// `git_file_changes` row — so anchor mining only resolves it via a live gix diff (the
/// merge-commit fallback path), not the `git_file_changes` lookup.
fn seed_indexed_source(conn: &Connection, repo: &str, path: &str) {
    // A matching indexed file + one logical + one un-logical symbol, so anchor mining resolves.
    conn.execute(
        "INSERT INTO files (path, language, kind, sha256, modified_at_ms, indexed_at_ms, repo_id, \
         generation)
             VALUES (?1,'rust','source','s',1,1,?2,0)",
        params![path, repo],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO symbols (file_id, language, name, kind, start_byte, end_byte)
             VALUES (?1,'rust','render_widget','function',0,10)",
        [file_id],
    )
    .unwrap();
    let symbol_id = conn.last_insert_rowid();
    // The logical parent (FK target); id 999 is what the anchor's `sym_<hex>` encodes.
    conn.execute(
        "INSERT OR IGNORE INTO logical_symbols
                 (id, language, path, logical_name, kind, variant_count, group_reason)
             VALUES (999, 'rust', ?1, 'render_widget', 'function', 1, 'test')",
        [path],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO logical_symbol_members (logical_symbol_id, symbol_id, start_line, end_line)
             VALUES (999, ?1, 1, 2)",
        [symbol_id],
    )
    .unwrap();
}

fn distill_count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn input_hash(conn: &Connection, key: &str) -> String {
    conn.query_row(
        "SELECT distill_input_hash FROM papertrail_distill WHERE item_key = ?1",
        [key],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn full_flow_extracts_a_coalesced_record_with_floors_anchors_and_queue() {
    let conn = scoped_conn("repoA");
    // A closed issue #5 fixed by a merged PR #6, linked by a TEXT-tier closing edge (the
    // closer-minting parser matched a closing keyword); the PR has a review comment.
    seed_item(&conn, "repoA", "issue", "5", "The widget crashes on load.", "closed", None);
    seed_item(
        &conn,
        "repoA",
        "change_request",
        "6",
        "## Summary\nFixes it.",
        "merged",
        Some("deadbeef"),
    );
    seed_comment(&conn, "repoA", "issue", "5", "c1", false);
    seed_comment(&conn, "repoA", "change_request", "6", "c2", true);
    seed_closing_edge(&conn, "repoA", "5", "6", Some("deadbeef"), "text");
    seed_commit(&conn, "repoA", "deadbeef", "fix: widget crash (fixes #5)");
    seed_changed_file(&conn, "repoA", "deadbeef", "crates/core/src/widget.rs");

    let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(report.eligible, 1, "issue #5 is the record; PR #6 coalesces into it");
    assert_eq!(report.records_written, 1);
    assert_eq!(report.coalesced_pairs, 1);
    assert_eq!(report.fix_edge_text, 1);
    assert_eq!(report.mechanical_landed, 1, "the text closing edge floors it to landed");

    // The skeleton row: mechanical columns populated, model columns NULL.
    let (fix_src, kw, revert, shape, qualified, model_status): (
        String,
        Option<String>,
        i64,
        String,
        i64,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT fix_edge_source, closing_keyword_floor, revert_override, thread_shape,
                        anchors_qualified_count, outcome_status_model
                 FROM papertrail_distill WHERE item_kind='issue' AND item_key='5'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(fix_src, "text");
    assert_eq!(kw.as_deref(), Some("closing"), "text closing edge sets the keyword floor");
    assert_eq!(revert, 0);
    assert!(!shape.is_empty());
    assert_eq!(qualified, 1, "one resolved symbol anchor (render_widget)");
    assert_eq!(model_status, None, "model column is an honest null in Phase 1");

    // Mechanical junctions.
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_record_commits WHERE commit_sha='deadbeef'"
        ),
        1,
    );
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_edges WHERE edge_kind='coalesced' AND \
             src_item_key='5' AND dst_item_key='6'"
        ),
        1,
    );
    // File anchor + symbol anchor.
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE anchor_kind='file'"
        ),
        1
    );
    let sym: (String, i64, i64, i64) = conn
        .query_row(
            "SELECT logical_symbol_id, resolved, candidate_ordinal, selected
                 FROM papertrail_distill_anchors
                  WHERE anchor_kind='symbol' AND name='render_widget'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(sym.0, "sym_3e7", "999 == 0x3e7"); // format_sym_handle(999)
    assert_eq!(sym.1, 1);
    assert_eq!(sym.2, 1, "file A0 is followed deterministically by symbol A1");
    assert_eq!(sym.3, 0, "extraction mines candidates; only the model selects them");
    // Queue holds the issue record (the coalesced PR is not its own record).
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"), 1);
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_queue WHERE item_kind='issue' AND \
             item_key='5'"
        ),
        1,
    );
}

/// Build a throwaway git repo whose HEAD is a real MERGE commit, and return `(root, merge_sha,
/// changed_path)`. The merge's first-parent diff touches `changed_path` (added on the merged
/// branch), while a real merge carries NO per-file numstat — exactly the shape the history
/// index stores no `git_file_changes` rows for.
fn build_merge_repo() -> (rag_rat_base::test_scratch::ScratchDir, String, String) {
    let root = rag_rat_base::test_scratch::ScratchDir::new("distill-merge");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let git = |args: &[&str]| {
        rag_rat_base::test_git::run(&root, args);
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Rag Rat"]);
    git(&["config", "user.email", "rag@example.com"]);
    std::fs::write(root.join("README.md"), "base\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "base"]);
    git(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(root.join("src/widget.rs"), "fn render_widget() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "feat: add widget"]);
    git(&["checkout", "-q", "-"]);
    // `--no-ff` forces a real merge commit (a fast-forward would carry no merge node).
    git(&["merge", "--no-ff", "-q", "-m", "Merge feature", "feature"]);
    let merge_sha = rag_rat_base::test_git::output(&root, &["rev-parse", "HEAD"]);
    (root, merge_sha, "src/widget.rs".to_string())
}

#[test]
fn merge_commit_fix_mines_anchors_via_gix_first_parent_diff() {
    // A merge commit is the usual fixing SHA for a merged PR, but the history index records NO
    // `git_file_changes` rows for real merges — so anchor mining must fall back to a live gix
    // first-parent diff. This test seeds ONLY the indexed `files` row (no `git_file_changes`),
    // so it passes ONLY through the fallback: drop the fallback and the anchor count goes to 0.
    let (root, merge_sha, path) = build_merge_repo();
    let conn = scoped_conn("repoA");
    // A standalone merged PR, closed by its own merge commit (its own provider record).
    seed_item(&conn, "repoA", "change_request", "9", "A refactor PR.", "merged", Some(&merge_sha));
    seed_commit(&conn, "repoA", &merge_sha, "Merge feature");
    seed_indexed_source(&conn, "repoA", &path);

    let report = extract(&conn, Some(&root), &ExtractOptions::default()).unwrap();
    assert_eq!(report.records_written, 1);

    // The file anchor + the resolved symbol anchor both come from the gix-recovered path.
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE anchor_kind='file'"
        ),
        1,
        "the merge's first-parent diff surfaces the changed source file"
    );
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE anchor_kind='symbol' AND \
             name='render_widget' AND resolved=1"
        ),
        1,
        "and its symbol resolves"
    );

    // Control: with no repo handle the fallback cannot fire, so the same seed yields no anchors
    // — proving the anchors above came from the gix diff, not a stray
    // `git_file_changes` row.
    let conn2 = scoped_conn("repoB");
    seed_item(&conn2, "repoB", "change_request", "9", "A refactor PR.", "merged", Some(&merge_sha));
    seed_commit(&conn2, "repoB", &merge_sha, "Merge feature");
    seed_indexed_source(&conn2, "repoB", &path);
    extract(&conn2, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn2, "SELECT COUNT(*) FROM papertrail_distill_anchors"),
        0,
        "no repo handle → no fallback → no anchors from a merge with no git_file_changes rows"
    );
}

#[test]
fn fix_diff_is_snapshotted_only_for_symbol_candidate_files() {
    // The merge repo's first-parent diff adds `src/widget.rs`; the seeded index resolves its
    // symbol, so the patch is snapshotted. The same commit's `README.md` change (no symbol
    // candidate) must NOT appear — the cap is by symbol-candidate files, not by changed files.
    let (root, merge_sha, path) = build_merge_repo();
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A refactor PR.", "merged", Some(&merge_sha));
    seed_commit(&conn, "repoA", &merge_sha, "Merge feature");
    seed_indexed_source(&conn, "repoA", &path);

    extract(&conn, Some(&root), &ExtractOptions::default()).unwrap();

    let patches: Vec<(String, String)> = conn
        .prepare("SELECT path, patch FROM papertrail_distill_fix_diffs ORDER BY path")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(patches.len(), 1, "only the symbol-candidate file's patch is snapshotted");
    let (patch_path, patch) = &patches[0];
    assert_eq!(patch_path, &path);
    assert!(patch.contains("diff --git a/src/widget.rs b/src/widget.rs"), "{patch}");
    assert!(patch.contains("--- /dev/null"), "an added file diffs from /dev/null: {patch}");
    assert!(patch.contains("+++ b/src/widget.rs"), "{patch}");
    assert!(patch.contains("+fn render_widget() {}"), "the hunk content renders: {patch}");

    // Control: no repo handle → no diff rows (the drain never sees git state).
    let conn2 = scoped_conn("repoB");
    seed_item(&conn2, "repoB", "change_request", "9", "A refactor PR.", "merged", Some(&merge_sha));
    seed_commit(&conn2, "repoB", &merge_sha, "Merge feature");
    seed_indexed_source(&conn2, "repoB", &path);
    extract(&conn2, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn2, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"),
        0,
        "no repo handle → best-effort empty diff snapshot"
    );
}

#[test]
fn losing_the_repo_handle_preserves_diff_snapshots_and_the_identity() {
    // The diff is a pure function of already-hashed inputs, so git AVAILABILITY must not be
    // part of the record identity: extract with a repo, then again with `None` (the documented
    // bare/copied-index path) — the hash holds, the good snapshot rows survive, nothing
    // re-enqueues. The changed paths come from `git_file_changes` rows here (not the live-gix
    // merge fallback), so the anchors and changed-path selection are repo-independent and ONLY
    // the diff snapshot differs between the two passes.
    let (root, merge_sha, path) = build_merge_repo();
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A refactor PR.", "merged", Some(&merge_sha));
    seed_commit(&conn, "repoA", &merge_sha, "Merge feature");
    seed_changed_file(&conn, "repoA", &merge_sha, &path);

    extract(&conn, Some(&root), &ExtractOptions::default()).unwrap();
    let hash_with_repo: String = conn
        .query_row("SELECT distill_input_hash FROM papertrail_distill", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"),
        1,
        "the repo-backed pass snapshots the diff"
    );

    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let hash_without_repo: String = conn
        .query_row("SELECT distill_input_hash FROM papertrail_distill", [], |row| row.get(0))
        .unwrap();
    assert_eq!(hash_with_repo, hash_without_repo, "repo availability is not identity");
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"),
        1,
        "a repo-less rerun preserves the snapshot"
    );

    // Self-heal: the mirror is reset to the repo-less state (record + no diff rows), then a
    // repo-backed pass fills the missing rows WITHOUT any identity change.
    let conn2 = scoped_conn("repoB");
    seed_item(&conn2, "repoB", "change_request", "9", "A refactor PR.", "merged", Some(&merge_sha));
    seed_commit(&conn2, "repoB", &merge_sha, "Merge feature");
    seed_changed_file(&conn2, "repoB", &merge_sha, &path);
    extract(&conn2, None, &ExtractOptions::default()).unwrap();
    assert_eq!(distill_count(&conn2, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"), 0);
    extract(&conn2, Some(&root), &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn2, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"),
        1,
        "a later repo-backed pass heals the missing diff rows"
    );
}

#[test]
fn a_shallow_clone_missing_the_parent_yields_no_bogus_full_tree_diff() {
    // At a shallow boundary the parent id is recorded but the object is absent; that is NOT a
    // root commit. Diffing against the empty tree would snapshot every repo file as added.
    let (root, merge_sha, path) = build_merge_repo();
    // `git clone` into the guard's fresh, empty directory.
    let shallow = rag_rat_base::test_scratch::ScratchDir::new("distill-shallow");
    rag_rat_base::test_git::run(&root, &[
        "clone",
        "-q",
        "--depth",
        "1",
        &format!("file://{}", root.display()),
        shallow.to_str().unwrap(),
    ]);

    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A refactor PR.", "merged", Some(&merge_sha));
    seed_commit(&conn, "repoA", &merge_sha, "Merge feature");
    // `git_file_changes` rows supply the anchors, so the symbol-candidate filter is non-empty
    // even though the shallow repo cannot produce a first-parent diff.
    seed_changed_file(&conn, "repoA", &merge_sha, &path);

    extract(&conn, Some(&shallow), &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"),
        0,
        "a missing parent skips the commit — never a full-tree 'everything added' patch"
    );
}

/// Build a throwaway repo whose HEAD fix commit modifies `src/widget.rs` and adds a >1MiB
/// `src/big.rs`, returning `(root, fix_sha)`.
fn build_big_blob_repo() -> (rag_rat_base::test_scratch::ScratchDir, String) {
    let root = rag_rat_base::test_scratch::ScratchDir::new("distill-big");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let git = |args: &[&str]| {
        rag_rat_base::test_git::run(&root, args);
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Rag Rat"]);
    git(&["config", "user.email", "rag@example.com"]);
    std::fs::write(root.join("src/widget.rs"), "fn render_widget() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "base"]);
    std::fs::write(root.join("src/widget.rs"), "fn render_widget() { todo!() }\n").unwrap();
    std::fs::write(root.join("src/big.rs"), "x".repeat(1_200_000)).unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "fix: widget and a giant file"]);
    let fix_sha = rag_rat_base::test_git::output(&root, &["rev-parse", "HEAD"]);
    (root, fix_sha)
}

#[test]
fn a_blob_over_the_size_cap_is_skipped_before_rendering() {
    let (root, fix_sha) = build_big_blob_repo();
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "The fix.", "merged", Some(&fix_sha));
    seed_commit(&conn, "repoA", &fix_sha, "fix: widget and a giant file");
    seed_changed_file(&conn, "repoA", &fix_sha, "src/widget.rs");
    seed_changed_file(&conn, "repoA", &fix_sha, "src/big.rs");

    extract(&conn, Some(&root), &ExtractOptions::default()).unwrap();
    let paths: Vec<String> = conn
        .prepare("SELECT path FROM papertrail_distill_fix_diffs ORDER BY path")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        paths,
        vec!["src/widget.rs".to_string()],
        "the small patch renders; the >1MiB blob is skipped before any full diff"
    );
}

/// Seed one outbound ref row the mirror sync would have mined from `source_text`'s item body.
fn seed_outbound_ref(
    conn: &Connection,
    repo: &str,
    source_text: &str,
    target_key: &str,
    ref_kind: &str,
) {
    conn.execute(
        "INSERT INTO papertrail_refs
                 (tracker, project, item_key, item_kind, ref_kind, source_kind, source_text,
                  discovered_at_ms, repo_id)
             VALUES ('github','o/r',?1,'issue',?2,'item',?3,1,?4)",
        params![target_key, ref_kind, source_text, repo],
    )
    .unwrap();
}

#[test]
fn outbound_refs_snapshot_the_referenced_title_and_opening() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "A bug. See #9.", "closed", None);
    seed_item(&conn, "repoA", "issue", "9", "Opening line.\n\nSecond paragraph.", "closed", None);
    seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5", "9", "reference");
    // A comment-sourced ref to the same target dedupes; a ref to an unmirrored item drops.
    seed_comment(&conn, "repoA", "issue", "5", "c1", false);
    seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5:c1", "9", "reference");
    seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5", "404", "reference");
    // A giant single-paragraph body: the opening snapshot is capped to EXACTLY the prompt's
    // render width, so a multi-MB paragraph neither inflates the row nor hashes text the model
    // never sees.
    seed_item(&conn, "repoA", "issue", "10", &"x".repeat(5_000), "closed", None);
    seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5", "10", "reference");

    extract(&conn, None, &ExtractOptions::default()).unwrap();

    let rows: Vec<(String, String, String, String)> = conn
        .prepare(
            "SELECT target_item_key, ref_kind, title, opening FROM papertrail_distill_xrefs
                 WHERE item_kind='issue' AND item_key='5' ORDER BY xref_ordinal",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2, "comment-source dup and unmirrored target contribute nothing");
    assert_eq!(rows[0].0, "9");
    assert_eq!(rows[0].1, "reference");
    assert_eq!(rows[0].2, "t", "seeded title is frozen");
    assert_eq!(rows[0].3, "Opening line.", "the opening paragraph only, not the body");
    assert_eq!(rows[1].0, "10");
    assert_eq!(
        rows[1].3.chars().count(),
        crate::distill::prompts::XREF_TEXT_RENDER_CHARS + 1,
        "a giant paragraph's opening is capped to the render width plus the ellipsis",
    );
    assert!(rows[1].3.ends_with('…'), "the truncated opening keeps the ellipsis marker");

    // The referenced records are eligible too, but their own records have no outbound refs.
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_xrefs"), 2);
}

#[test]
fn xref_snapshot_cap_matches_the_prompt_xref_budget() {
    // Rows beyond the snapshot cap are invisible to the prompt; a budget below the cap would
    // hash rows the model never sees (spurious regeneration on their edits). Keep the two
    // equal.
    assert_eq!(
        super::XREF_SNAPSHOT_CAP,
        crate::distill::prompts::PromptBudget::default().max_xrefs,
        "the snapshot cap and the render budget must move together"
    );
}

#[test]
fn an_xref_edit_beyond_the_render_width_does_not_regenerate() {
    // The length-dimension partner of `xref_snapshot_cap_matches_the_prompt_xref_budget`: a
    // referenced item's title/opening are hashed at EXACTLY the width the prompt renders, so an
    // edit past that width is invisible to the model and must NOT regenerate the record
    // (re-paying the model with identical visible input). An edit WITHIN the width still does.
    let width = crate::distill::prompts::XREF_TEXT_RENDER_CHARS;
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "A bug. See #9.", "closed", None);
    seed_item(&conn, "repoA", "issue", "9", "Body.", "closed", None);
    seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5", "9", "reference");
    let head = "a".repeat(width);
    conn.execute("UPDATE papertrail_items SET title = ?1 WHERE item_key = '9'", [format!(
        "{head}X"
    )])
    .unwrap();

    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let baseline = input_hash(&conn, "5");
    let stored: String = conn
        .query_row("SELECT title FROM papertrail_distill_xrefs WHERE item_key='5'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        stored.chars().count(),
        width + 1,
        "the stored title is capped to the render width (plus the ellipsis), not the source",
    );

    // Edit only the TAIL, past the rendered width — the first `width` chars are unchanged.
    conn.execute("UPDATE papertrail_items SET title = ?1 WHERE item_key = '9'", [format!(
        "{head}YYYY"
    )])
    .unwrap();
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        input_hash(&conn, "5"),
        baseline,
        "a tail-only edit past the render width leaves the identity unchanged",
    );

    // Edit WITHIN the rendered width — now the model-visible text changes, so regenerate.
    conn.execute("UPDATE papertrail_items SET title = ?1 WHERE item_key = '9'", [format!(
        "z{head}"
    )])
    .unwrap();
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_ne!(
        input_hash(&conn, "5"),
        baseline,
        "an edit within the render width regenerates the record",
    );
}

#[test]
fn a_bare_ref_resolves_the_target_kind_by_fallback() {
    // A kindless bare ref (`#N`, `papertrail_refs.item_kind` NULL) resolves down the fallback
    // ladder: the source item's OWN kind first (parser namespace inheritance), then the
    // deterministic kind order. A PR whose `#9` names an ISSUE resolves the issue even though
    // the syntax could not disambiguate; a ref that resolves as NEITHER kind drops entirely.
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "5", "A PR. See #9 and #404.", "merged", None);
    seed_item(&conn, "repoA", "issue", "9", "The referenced issue body.", "closed", None);
    let kindless = |target: &str| {
        conn.execute(
            "INSERT INTO papertrail_refs
                     (tracker, project, item_key, item_kind, ref_kind, source_kind, source_text,
                      discovered_at_ms, repo_id)
                 VALUES ('github','o/r',?1,NULL,'reference','item',
                         'github:o/r:change_request:5',1,'repoA')",
            [target],
        )
        .unwrap();
    };
    kindless("9"); // no change_request #9 exists, but issue #9 does → resolves via fallback
    kindless("404"); // resolves as neither kind → dropped

    extract(&conn, None, &ExtractOptions::default()).unwrap();

    let rows: Vec<(String, String)> = conn
        .prepare(
            "SELECT target_item_key, target_item_kind FROM papertrail_distill_xrefs
                 WHERE item_kind='change_request' AND item_key='5' ORDER BY xref_ordinal",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![("9".to_string(), "issue".to_string())],
        "the kindless ref resolves to the issue by fallback; the unresolvable ref drops",
    );
}

/// Build a throwaway repo whose HEAD fix commit DELETES `src/gone.rs` (added in the base
/// commit), returning `(root, fix_sha, path)`.
fn build_delete_repo() -> (rag_rat_base::test_scratch::ScratchDir, String, String) {
    let root = rag_rat_base::test_scratch::ScratchDir::new("distill-del");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let git = |args: &[&str]| {
        rag_rat_base::test_git::run(&root, args);
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Rag Rat"]);
    git(&["config", "user.email", "rag@example.com"]);
    std::fs::write(root.join("src/gone.rs"), "fn gone() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "base"]);
    std::fs::remove_file(root.join("src/gone.rs")).unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "fix: remove gone"]);
    let fix_sha = rag_rat_base::test_git::output(&root, &["rev-parse", "HEAD"]);
    (root, fix_sha, "src/gone.rs".to_string())
}

#[test]
fn a_deleted_symbol_file_snapshots_a_deletion_patch() {
    // The deletion arm of the file-patch header: a fixing commit that removes a
    // symbol-candidate file diffs the old path TO /dev/null, not a bogus addition.
    let (root, fix_sha, path) = build_delete_repo();
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "Remove it.", "merged", Some(&fix_sha));
    seed_commit(&conn, "repoA", &fix_sha, "fix: remove gone");
    seed_changed_file(&conn, "repoA", &fix_sha, &path);

    extract(&conn, Some(&root), &ExtractOptions::default()).unwrap();

    let patch: String = conn
        .query_row(
            "SELECT patch FROM papertrail_distill_fix_diffs WHERE path = ?1",
            [&path],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        patch.contains(&format!("--- a/{path}")),
        "a deletion diffs from the old path: {patch}"
    );
    assert!(patch.contains("+++ /dev/null"), "a deleted file diffs TO /dev/null: {patch}");
    assert!(patch.contains("-fn gone() {}"), "the removed line renders: {patch}");
}

#[test]
fn an_xref_title_edit_regenerates_the_record() {
    // The referenced item's title is MUTABLE mirror state folded into the prompt, so an edit
    // must regenerate the record exactly like a primary-source edit.
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "A bug. See #9.", "closed", None);
    seed_item(&conn, "repoA", "issue", "9", "Opening line.", "closed", None);
    seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5", "9", "reference");

    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let hash_before: String = conn
        .query_row(
            "SELECT distill_input_hash FROM papertrail_distill WHERE item_key='5'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE papertrail_items SET title='Renamed referenced item' WHERE item_key='9'",
        [],
    )
    .unwrap();
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let hash_after: String = conn
        .query_row(
            "SELECT distill_input_hash FROM papertrail_distill WHERE item_key='5'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(hash_before, hash_after, "a referenced title edit changes the input identity");
    let title: String = conn
        .query_row("SELECT title FROM papertrail_distill_xrefs WHERE item_key='5'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(title, "Renamed referenced item", "the snapshot re-froze the edited title");

    // An identical rerun is stable: no new queue rows for either record (the still-queued
    // NULL-stamped rows keep their place — draining, not extraction, retires them).
    let queue_before = distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"),
        queue_before,
        "unchanged input does not re-enqueue"
    );
    let hash_stable: String = conn
        .query_row(
            "SELECT distill_input_hash FROM papertrail_distill WHERE item_key='5'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(hash_after, hash_stable, "an identical rerun recomputes the same identity");
}

#[test]
fn a_provider_only_closing_edge_does_not_set_the_keyword_floor() {
    // The closing-keyword floor is derived from a TEXT-tier closing edge (the parser matched a
    // closing keyword), NOT a re-scan of commit text — so a same-numbered cross-project or MR
    // URL in a commit can never force `landed`. A provider-attested closure carries no keyword,
    // so the floor stays NULL and the effective status defers to the model (unclear here).
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "A bug.", "closed", None);
    seed_item(&conn, "repoA", "change_request", "6", "The PR.", "merged", Some("m6"));
    seed_closing_edge(&conn, "repoA", "5", "6", Some("m6"), "provider");
    seed_commit(&conn, "repoA", "m6", "fix: crash. Fixes other/repo#5"); // cross-project ref

    let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
    let kw: Option<String> = conn
        .query_row(
            "SELECT closing_keyword_floor FROM papertrail_distill WHERE item_kind='issue' AND \
             item_key='5'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kw, None, "a provider-only closure sets no keyword floor");
    assert_eq!(report.fix_edge_provider, 1);
    assert_eq!(report.mechanical_landed, 0, "no keyword floor → not force-landed");
}

#[test]
fn standalone_merged_pr_is_its_own_provider_record() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A refactor PR.", "merged", Some("cafe"));
    seed_commit(&conn, "repoA", "cafe", "refactor: tidy up");
    let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(report.records_written, 1);
    assert_eq!(report.fix_edge_provider, 1, "a merged PR is its own provider closure");
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill WHERE item_kind='change_request' AND \
             item_key='9'"
        ),
        1,
    );
}

#[test]
fn closed_issue_with_no_closing_edge_has_no_fix_edge() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "7", "Closed as not planned.", "closed", None);
    let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(report.fix_edge_none, 1);
    let src: String = conn
        .query_row("SELECT fix_edge_source FROM papertrail_distill WHERE item_key='7'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(src, "none");
}

#[test]
fn a_closing_edge_to_an_unmerged_pr_does_not_upgrade_fix_edge_provenance() {
    // Issue #5 has a provider closing edge to PR #6, but PR #6 is CLOSED (not merged) — its
    // merge_commit_sha is a trap. The edge must not be treated as a fix edge, so the
    // no-fix-edge floor still fires (fix_edge_source = none, and no coalesce).
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "A bug.", "closed", None);
    seed_item(&conn, "repoA", "change_request", "6", "An abandoned PR.", "closed", Some("beef"));
    seed_closing_edge(&conn, "repoA", "5", "6", Some("beef"), "provider");

    let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(report.coalesced_pairs, 0, "an unmerged PR is not a coalesce partner");
    let src: String = conn
        .query_row(
            "SELECT fix_edge_source FROM papertrail_distill WHERE item_kind='issue' AND \
             item_key='5'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(src, "none", "an unmerged-PR closing edge is not a fix edge");
}

#[test]
fn sibling_repo_items_never_leak_into_the_active_repos_records() {
    let conn = scoped_conn("repoA");
    // Active repo A has a closed issue #5; a SIBLING repo B has an identically-numbered closed
    // issue #5. Extraction is scoped to A and must not produce a record for B's item.
    seed_item(&conn, "repoA", "issue", "5", "A's bug.", "closed", None);
    seed_item(&conn, "repoB", "issue", "5", "B's bug.", "closed", None);
    seed_item(&conn, "repoB", "change_request", "6", "B's PR.", "merged", Some("beef"));
    // Poison an identically-keyed sibling snapshot: A's replacement/clear paths must scope by
    // the complete record identity, not merely tracker/project/kind/key.
    conn.execute(
        "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, repo_id)
             VALUES ('github','o/r','issue','5',0,'primary',NULL,'issue','5','item','body','5',
                     'B poison','repoB')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_units
                 (tracker, project, item_kind, item_key, unit_ordinal, source_ordinal, byte_start,
                  byte_end, repo_id)
             VALUES ('github','o/r','issue','5',0,0,0,8,'repoB')",
        [],
    )
    .unwrap();

    let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(report.records_written, 1, "only repo A's single item");
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"), 1);
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill WHERE repo_id='repoB'"),
        0,
        "no sibling-repo record",
    );
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue WHERE repo_id='repoB'"),
        0,
    );
    let sibling_text: String = conn
        .query_row(
            "SELECT exact_text FROM papertrail_distill_sources WHERE repo_id='repoB'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sibling_text, "B poison", "the sibling snapshot survives A extraction");
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_units WHERE repo_id='repoB'"),
        1,
        "the sibling unit span survives A extraction",
    );
}

#[test]
fn snapshot_source_enums_round_trip_only_their_closed_tokens() {
    for role in [SourceRole::Primary, SourceRole::Partner] {
        assert_eq!(SourceRole::from_db_str(role.as_db_str()).unwrap(), role);
    }
    for kind in [SourceKind::Item, SourceKind::Comment] {
        assert_eq!(SourceKind::from_db_str(kind.as_db_str()).unwrap(), kind);
    }
    for part in [SourcePart::Title, SourcePart::Body, SourcePart::Comment] {
        assert_eq!(SourcePart::from_db_str(part.as_db_str()).unwrap(), part);
    }
    assert!(SourcePart::from_db_str("summary").is_err());
}

#[test]
fn snapshots_distinguish_identical_title_and_body_and_unicode_units_quote_exact_bytes() {
    let conn = scoped_conn("repoA");
    let text = "Intro 🦀.\n\n尾 paragraph.";
    seed_item(&conn, "repoA", "issue", "5", text, "closed", None);
    conn.execute(
        "UPDATE papertrail_items SET title=?1, author='alice', author_kind='user',
                    author_association='member', created_at='2026-01-01T00:00:00.123Z'
             WHERE repo_id='repoA' AND item_key='5'",
        [text],
    )
    .unwrap();
    extract(&conn, None, &ExtractOptions::default()).unwrap();

    let sources: Vec<(i64, String, String, String)> = conn
        .prepare(
            "SELECT source_ordinal, source_part, source_id, exact_text
                 FROM papertrail_distill_sources WHERE repo_id='repoA' ORDER BY source_ordinal",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(&sources[0], &(0, "title".into(), "5".into(), text.into()));
    assert_eq!(&sources[1], &(1, "body".into(), "5".into(), text.into()));

    let units: Vec<(i64, i64, i64)> = conn
        .prepare(
            "SELECT source_ordinal, byte_start, byte_end FROM papertrail_distill_units
                 WHERE repo_id='repoA' ORDER BY unit_ordinal",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(units.len(), 4, "two markdown blocks for each distinct source part");
    for (source_ordinal, start, end) in units {
        let source_ordinal = source_ordinal as usize;
        let start = start as usize;
        let end = end as usize;
        let exact = &sources[source_ordinal].3;
        let quote = &exact[start..end];
        assert_eq!(quote.as_bytes(), &exact.as_bytes()[start..end]);
        assert!(std::str::from_utf8(quote.as_bytes()).is_ok(), "span is on UTF-8 boundaries");
    }
}

#[test]
fn every_source_text_or_provenance_edit_changes_the_full_snapshot_hash() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "body one", "closed", None);
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let initial = input_hash(&conn, "5");

    conn.execute("UPDATE papertrail_items SET title='new title' WHERE item_key='5'", []).unwrap();
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let title_edit = input_hash(&conn, "5");
    assert_ne!(initial, title_edit, "title-only edits regenerate");

    conn.execute("UPDATE papertrail_items SET body='body two' WHERE item_key='5'", []).unwrap();
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let body_edit = input_hash(&conn, "5");
    assert_ne!(title_edit, body_edit, "body-only edits regenerate");

    seed_comment(&conn, "repoA", "issue", "5", "c1", false);
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let comment_added = input_hash(&conn, "5");
    assert_ne!(body_edit, comment_added, "comments are full snapshot inputs");

    conn.execute("UPDATE papertrail_comments SET body='edited comment' WHERE comment_id='c1'", [])
        .unwrap();
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let comment_edit = input_hash(&conn, "5");
    assert_ne!(comment_added, comment_edit, "comment text edits regenerate");

    conn.execute(
        "UPDATE papertrail_comments SET author_association='maintainer' WHERE comment_id='c1'",
        [],
    )
    .unwrap();
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let provenance_edit = input_hash(&conn, "5");
    assert_ne!(comment_edit, provenance_edit, "provenance-only edits regenerate");
}

#[test]
fn all_partners_are_snapshotted_in_deterministic_key_order() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "issue", "closed", None);
    seed_item(&conn, "repoA", "change_request", "8", "later", "merged", Some("m8"));
    seed_item(&conn, "repoA", "change_request", "6", "earlier", "merged", Some("m6"));
    seed_closing_edge(&conn, "repoA", "5", "8", Some("m8"), "provider");
    seed_closing_edge(&conn, "repoA", "5", "6", Some("m6"), "provider");
    extract(&conn, None, &ExtractOptions::default()).unwrap();

    let partners: Vec<(i64, String)> = conn
        .prepare(
            "SELECT partner_ordinal, source_item_key FROM papertrail_distill_sources
                 WHERE repo_id='repoA' AND role='partner' AND source_part='title'
                 ORDER BY source_ordinal",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(partners, vec![(0, "6".into()), (1, "8".into())]);
}

#[test]
fn regeneration_replaces_snapshots_while_unchanged_reruns_leave_them_stable() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "first\n\nsecond", "closed", None);
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let first_ids: Vec<i64> = conn
        .prepare("SELECT id FROM papertrail_distill_sources ORDER BY source_ordinal")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let first_hash = input_hash(&conn, "5");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let unchanged_ids: Vec<i64> = conn
        .prepare("SELECT id FROM papertrail_distill_sources ORDER BY source_ordinal")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(first_ids, unchanged_ids, "unchanged snapshots are left untouched");
    assert_eq!(first_hash, input_hash(&conn, "5"));

    conn.execute("UPDATE papertrail_items SET body='replacement' WHERE item_key='5'", []).unwrap();
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let body: String = conn
        .query_row(
            "SELECT exact_text FROM papertrail_distill_sources WHERE source_part='body'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(body, "replacement");
    assert_ne!(first_hash, input_hash(&conn, "5"));
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_units WHERE source_ordinal=1"
        ),
        1,
        "old body spans are replaced rather than appended",
    );
}

#[test]
fn otherwise_identical_snapshots_have_repo_isolated_hashes() {
    let conn_a = scoped_conn("repoA");
    seed_item(&conn_a, "repoA", "issue", "5", "same", "closed", None);
    extract(&conn_a, None, &ExtractOptions::default()).unwrap();
    let conn_b = scoped_conn("repoB");
    seed_item(&conn_b, "repoB", "issue", "5", "same", "closed", None);
    extract(&conn_b, None, &ExtractOptions::default()).unwrap();
    assert_ne!(input_hash(&conn_a, "5"), input_hash(&conn_b, "5"));
}

#[test]
fn cheap_enqueue_covers_every_eligible_thread_and_is_idempotent_and_scoped() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "closed issue", "closed", None);
    seed_item(&conn, "repoA", "change_request", "6", "merged pr", "merged", Some("x"));
    seed_item(&conn, "repoA", "issue", "8", "still open", "open", None); // not eligible
    seed_item(&conn, "repoB", "issue", "5", "sibling", "closed", None); // wrong repo

    let first = enqueue_eligible(&conn).unwrap();
    assert_eq!(first, 2, "the closed issue + merged PR (not the open issue, not repo B)");
    // Idempotent: a second pass adds nothing.
    assert_eq!(enqueue_eligible(&conn).unwrap(), 0);
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"), 2);
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue WHERE repo_id='repoB'"),
        0,
    );

    // Once a thread is distilled and its queue row is DRAINED, a later sync must NOT re-enqueue
    // it — otherwise every completed thread is re-processed forever.
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    conn.execute("DELETE FROM papertrail_distill_queue", []).unwrap();
    assert_eq!(enqueue_eligible(&conn).unwrap(), 0, "distilled threads are not re-enqueued");
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"), 0);
}

#[test]
fn re_running_extraction_is_idempotent() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
    seed_commit(&conn, "repoA", "cafe", "refactor: x");
    seed_changed_file(&conn, "repoA", "cafe", "crates/core/src/a.rs");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    // No duplicate rows across the two passes (natural-key upsert + junction
    // clear-and-rebuild).
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"), 1);
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_record_commits"), 1);
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE anchor_kind='file'"
        ),
        1
    );
}

#[test]
fn same_numbered_threads_in_different_projects_stay_isolated() {
    // One repo, two tracker-binding projects, each with a closed issue #5. Only o/r's issue is
    // closed by a merged PR #6. Records must not cross-contaminate on the shared number.
    let conn = scoped_conn("repoA");
    seed_item_in(&conn, "repoA", "o/r", "issue", "5", "Alpha bug body.", "closed");
    seed_item_in(&conn, "repoA", "o/r2", "issue", "5", "Beta bug body.", "closed");
    seed_item_in(&conn, "repoA", "o/r", "change_request", "6", "Alpha PR.", "merged");
    seed_closing_edge(&conn, "repoA", "5", "6", Some("deadbeef"), "provider");

    let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
    // Two issue records (one per project); o/r's PR #6 coalesced away.
    assert_eq!(report.records_written, 2);
    assert_eq!(report.coalesced_pairs, 1);

    // Each project's record has its OWN fix edge and a DISTINCT input hash (distinct bodies).
    let fix_a: String = conn
        .query_row(
            "SELECT fix_edge_source FROM papertrail_distill WHERE project='o/r' AND item_key='5'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let fix_b: String = conn
        .query_row(
            "SELECT fix_edge_source FROM papertrail_distill WHERE project='o/r2' AND item_key='5'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fix_a, "provider", "o/r#5 coalesces its merged PR closer");
    assert_eq!(fix_b, "none", "o/r2#5 has no closer of its own");
    let distinct_hashes: i64 = distill_count(
        &conn,
        "SELECT COUNT(DISTINCT distill_input_hash) FROM papertrail_distill WHERE item_key='5'",
    );
    assert_eq!(distinct_hashes, 2, "distinct bodies → distinct regeneration identities");

    // The coalesced edge belongs to o/r only — o/r2 has none.
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_edges WHERE project='o/r' AND \
             src_item_key='5'"
        ),
        1,
    );
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_edges WHERE project='o/r2'"),
        0,
    );
}

#[test]
fn a_pr_extracted_standalone_loses_its_record_once_it_becomes_coalesced() {
    let conn = scoped_conn("repoA");
    // Pass 1: PR #6 is merged with no known closing edge → a standalone record + queue entry.
    seed_item(&conn, "repoA", "change_request", "6", "The fix PR.", "merged", Some("dead"));
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill WHERE item_kind='change_request' AND \
             item_key='6'"
        ),
        1,
        "PR #6 starts as a standalone record",
    );

    // Pass 2: a closed issue #5 and a closing edge #5 -> #6 arrive; #6 now coalesces into #5.
    seed_item(&conn, "repoA", "issue", "5", "The bug.", "closed", None);
    seed_closing_edge(&conn, "repoA", "5", "6", Some("dead"), "provider");
    extract(&conn, None, &ExtractOptions::default()).unwrap();

    // The stale standalone PR record + its queue entry are gone; only the coalesced issue
    // record remains, with the coalesce edge.
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill WHERE item_kind='change_request' AND \
             item_key='6'"
        ),
        0,
        "the coalesced PR's standalone record is reconciled away",
    );
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_queue WHERE item_kind='change_request' AND \
             item_key='6'"
        ),
        0,
        "the coalesced PR's queue entry is removed",
    );
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill WHERE item_kind='issue' AND item_key='5'"
        ),
        1,
    );
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_edges WHERE edge_kind='coalesced' AND \
             dst_item_key='6'"
        ),
        1,
    );
}

#[test]
fn a_record_whose_thread_becomes_ineligible_is_reconciled_away() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "7", "A bug.", "closed", None);
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"), 1);

    // The issue is reopened → no longer eligible. The next extraction must drop its record.
    conn.execute("UPDATE papertrail_items SET state_normalized='open' WHERE item_key='7'", [])
        .unwrap();
    let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(report.records_written, 0, "the reopened issue is not eligible");
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"),
        0,
        "the ineligible record is reconciled away",
    );
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"), 0);
}

#[test]
fn a_record_whose_thread_is_absent_from_the_mirror_is_preserved_not_reconciled_away() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "7", "A bug.", "closed", None);
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"), 1);

    // The local mirror no longer carries the thread — a thin/empty-mirror device (fresh
    // enrollment, or a roster peer without tracker credentials), or an item deleted upstream.
    // Absent from the mirror is "no opinion", never "delete": once distill/1 replicates
    // records, deleting here would author a `Remove` that wipes the fleet's records
    // (#1135).
    conn.execute("DELETE FROM papertrail_items", []).unwrap();
    let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(report.records_written, 0, "an empty mirror plans nothing");
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"),
        1,
        "a record whose thread the mirror no longer carries is preserved, not reconciled away",
    );
}

#[test]
fn an_extraction_that_writes_a_record_advances_the_papertrail_lens_lane() {
    let conn = scoped_conn("repoA");
    // V108 dropped the papertrail_distill triggers, so the extract pass advances the lane
    // explicitly — gated on repo registration.
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repoA', 'repoA', 0)",
        [],
    )
    .unwrap();
    seed_item(&conn, "repoA", "issue", "7", "A bug.", "closed", None);
    let lane = || -> i64 {
        conn.query_row(
            "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM repo_meta
                     WHERE repo_id = 'repoA' AND key = 'lens_papertrail_revision'), 0)",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let before = lane();
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"), 1);
    assert!(lane() > before, "writing a distilled record advances the papertrail lane");
}

#[test]
fn changing_the_fixing_commit_regenerates_even_with_identical_text() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "A bug.", "closed", None);
    // A commit closing edge (its SHA is a fix input that never appears in the thread text).
    let seed_commit_edge = |sha: &str| {
        conn.execute(
            "INSERT OR REPLACE INTO papertrail_closing_edges
                     (tracker, project, issue_kind, issue_key, closer_kind, closer_key,
                      closer_commit, source, synced_at_ms, repo_id)
                 VALUES ('github','o/r','issue','5','commit',?1,NULL,'provider',1,'repoA')",
            [sha],
        )
        .unwrap();
    };
    seed_commit_edge("c1");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    conn.execute("UPDATE papertrail_distill SET root_cause='x' WHERE item_key='5'", []).unwrap();

    // A second fixing commit arrives (same thread text). The record must regenerate: model
    // output cleared, because the fix SHA set rides the regeneration hash.
    seed_commit_edge("c2");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let cause: Option<String> = conn
        .query_row("SELECT root_cause FROM papertrail_distill WHERE item_key='5'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cause, None, "a changed fixing commit invalidates stale model output");
}

#[test]
fn a_pr_queued_before_extraction_is_dropped_from_the_queue_once_coalesced() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "The bug.", "closed", None);
    seed_item(&conn, "repoA", "change_request", "6", "The fix PR.", "merged", Some("dead"));
    seed_closing_edge(&conn, "repoA", "5", "6", Some("dead"), "provider");

    // The cheap sync enqueue runs first (no records yet) → queues BOTH #5 and #6.
    assert_eq!(enqueue_eligible(&conn).unwrap(), 2);
    // Extraction coalesces #6 into #5. #6's queue row (never a record) must be reconciled away.
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_queue WHERE item_kind='change_request' AND \
             item_key='6'"
        ),
        0,
        "the coalesced PR's pre-extraction queue row is removed",
    );
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_queue WHERE item_kind='issue' AND \
             item_key='5'"
        ),
        1,
        "the coalesced issue record stays queued",
    );
}

#[test]
fn an_unchanged_rerun_after_a_drain_does_not_requeue() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
    seed_commit(&conn, "repoA", "cafe", "refactor: x");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    // Simulate the #704 drain removing the completed queue row.
    conn.execute(
        "UPDATE papertrail_distill SET prompt_version = ?1, model_input_hash = 'sha256:model'",
        [i64::from(crate::distill::prompts::PROMPT_VERSION)],
    )
    .unwrap();
    conn.execute("DELETE FROM papertrail_distill_queue", []).unwrap();
    // An identical re-extract must NOT re-enqueue the completed, unchanged record.
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"),
        0,
        "an unchanged record is not re-queued after a drain",
    );
}

#[test]
fn a_prompt_version_change_invalidates_model_output_and_requeues() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
    seed_commit(&conn, "repoA", "cafe", "refactor: x");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    conn.execute_batch(
        "UPDATE papertrail_distill SET
                 root_cause = 'old result', prompt_version = 0, model_input_hash = 'sha256:old';
             UPDATE papertrail_distill_anchors SET selected = 1;
             DELETE FROM papertrail_distill_queue;",
    )
    .unwrap();

    extract(&conn, None, &ExtractOptions::default()).unwrap();

    let row: (Option<String>, Option<i64>, Option<String>, i64) = conn
        .query_row(
            "SELECT root_cause, prompt_version, model_input_hash,
                        (SELECT COUNT(*) FROM papertrail_distill_queue)
                 FROM papertrail_distill WHERE repo_id = 'repoA' AND item_key = '9'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(row, (None, None, None, 1));
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE selected=1"),
        0,
    );
}

#[test]
fn only_a_landed_commit_revert_flips_status_not_a_text_claim() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
    seed_commit(&conn, "repoA", "cafe", "feat: x"); // the fix (merge commit)
    // A reverts ref, tagged by its source and (for commits) the reverting commit sha.
    let seed_reverts = |source_kind: &str, source_commit: Option<&str>| {
        conn.execute(
            "INSERT INTO papertrail_refs
                     (tracker, project, item_key, item_kind, ref_kind, source_kind, source_commit,
                      source_text, discovered_at_ms, repo_id)
                 VALUES ('github','o/r','9','change_request','reverts',?1,?2,'Reverts \
             #9',1,'repoA')",
            rusqlite::params![source_kind, source_commit],
        )
        .unwrap();
    };
    // A text-tier `reverts` ref (an open PR body / comment claim) must NOT flip status.
    seed_reverts("item", None);
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn, "SELECT revert_override FROM papertrail_distill WHERE item_key='9'"),
        0,
        "a text-claim reverts ref does not flip status",
    );

    // A reverting commit that is NOT in git history (rebased away / stale annotation) also must
    // not flip — the ref is a dangling annotation.
    seed_reverts("commit", Some("nonexistent-sha"));
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn, "SELECT revert_override FROM papertrail_distill WHERE item_key='9'"),
        0,
        "a reverts commit no longer in git history does not flip status",
    );

    // A landed revert commit that reverts a DIFFERENT (old, replaced) commit — the
    // reopen→revert→re-fix case — must not flip the re-fixed record.
    seed_commit_with_body(&conn, "repoA", "stale-rev", "Revert old", "This reverts commit oldfix.");
    seed_reverts("commit", Some("stale-rev"));
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn, "SELECT revert_override FROM papertrail_distill WHERE item_key='9'"),
        0,
        "a revert of a non-current fix commit does not flip status",
    );

    // A landed revert whose body reverts the CURRENT fix commit does.
    seed_commit_with_body(
        &conn,
        "repoA",
        "real-rev",
        "Revert the fix",
        "This reverts commit cafe.",
    );
    seed_reverts("commit", Some("real-rev"));
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn, "SELECT revert_override FROM papertrail_distill WHERE item_key='9'"),
        1,
        "a revert of the current fix commit flips status",
    );
}

#[test]
fn a_fix_that_is_itself_a_revert_landed_it_is_not_marked_reverted() {
    // A merged PR whose own fixing commit is a `Revert` (intentional revert work) LANDED — the
    // record must not be marked reverted just because its subject starts with "Revert".
    let conn = scoped_conn("repoA");
    seed_item(
        &conn,
        "repoA",
        "change_request",
        "9",
        "Revert the bad change.",
        "merged",
        Some("cafe"),
    );
    seed_commit(&conn, "repoA", "cafe", "Revert \"feat: bad change\" (#8)");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(&conn, "SELECT revert_override FROM papertrail_distill WHERE item_key='9'"),
        0,
        "intentional revert work that landed is not itself reverted",
    );
}

#[test]
fn a_revert_of_the_coalesced_partner_pr_flips_the_issue_record() {
    // Issue #5 coalesces merged PR #6; the revert commit references the PR (#6), not the issue.
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "issue", "5", "A bug.", "closed", None);
    seed_item(&conn, "repoA", "change_request", "6", "The fix PR.", "merged", Some("m6"));
    seed_closing_edge(&conn, "repoA", "5", "6", Some("m6"), "provider");
    seed_commit(&conn, "repoA", "m6", "The fix"); // the coalesced fix (merge commit)
    // the revert names the fix commit it reverts
    seed_commit_with_body(&conn, "repoA", "rev6", "Revert the fix", "This reverts commit m6.");
    conn.execute(
        "INSERT INTO papertrail_refs
                 (tracker, project, item_key, item_kind, ref_kind, source_kind, source_commit,
                  source_text, discovered_at_ms, repo_id)
             VALUES ('github','o/r','6','change_request','reverts','commit','rev6','Reverts \
         #6',1,'repoA')",
        [],
    )
    .unwrap();

    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(
            &conn,
            "SELECT revert_override FROM papertrail_distill WHERE item_kind='issue' AND \
             item_key='5'"
        ),
        1,
        "a revert of the coalesced partner PR flips the issue record",
    );
}

#[test]
fn a_rerun_preserves_non_coalesced_edges() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
    seed_commit(&conn, "repoA", "cafe", "feat: x");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    // A later model/human `supersedes` edge authored by this record (reserved to survive
    // regeneration).
    conn.execute(
        "INSERT INTO papertrail_distill_edges
                 (tracker, project, src_item_kind, src_item_key, dst_item_kind, dst_item_key,
                  edge_kind, created_at_ms, repo_id)
             VALUES \
         ('github','o/r','change_request','9','change_request','10','supersedes',1,'repoA')",
        [],
    )
    .unwrap();

    // Re-extract: the mechanical clear must NOT wipe the supersedes edge.
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        distill_count(
            &conn,
            "SELECT COUNT(*) FROM papertrail_distill_edges WHERE edge_kind='supersedes' AND \
             src_item_key='9'"
        ),
        1,
        "a rerun preserves supersedes/promoted edges",
    );
}

#[test]
fn regeneration_resets_the_queue_attempt_state() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
    seed_commit(&conn, "repoA", "cafe", "feat: x");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    // The previous input exhausted the drain's retries and left an error on the queue row.
    conn.execute(
        "UPDATE papertrail_distill_queue SET attempts=5, last_error='boom', raw_reply='junk'
             WHERE item_key='9'",
        [],
    )
    .unwrap();

    // The input changes (a new comment). The regenerated work must start with a fresh attempt.
    seed_comment(&conn, "repoA", "change_request", "9", "c-new", false);
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let (attempts, err, reply): (i64, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT attempts, last_error, raw_reply FROM papertrail_distill_queue WHERE \
             item_key='9'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(attempts, 0, "regeneration resets the attempt count");
    assert_eq!(err, None);
    assert_eq!(reply, None);
}

#[test]
fn unchanged_pending_work_preserves_failure_attempt_state() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
    seed_commit(&conn, "repoA", "cafe", "feat: x");
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    conn.execute(
        "UPDATE papertrail_distill_queue
             SET attempts = 2, last_error = 'bad reply', raw_reply = 'raw', enqueued_at_ms = 7
             WHERE item_key = '9'",
        [],
    )
    .unwrap();

    extract(&conn, None, &ExtractOptions::default()).unwrap();

    let row: (i64, Option<String>, Option<String>, i64) = conn
        .query_row(
            "SELECT attempts, last_error, raw_reply, enqueued_at_ms
                 FROM papertrail_distill_queue WHERE item_key = '9'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(row, (2, Some("bad reply".into()), Some("raw".into()), 7));
}

#[test]
fn regeneration_clears_stale_model_output_but_an_identical_rerun_preserves_it() {
    let conn = scoped_conn("repoA");
    seed_item(&conn, "repoA", "change_request", "9", "A PR body.", "merged", Some("cafe"));
    seed_commit(&conn, "repoA", "cafe", "refactor: x");
    extract(&conn, None, &ExtractOptions::default()).unwrap();

    // Simulate the #704 pass filling model columns + model junctions on this record.
    conn.execute(
        "UPDATE papertrail_distill SET root_cause='the cause', outcome_status_model='landed',
                 quotes_materialized=3, prompt_version=?1, model_input_hash='sha256:model'
             WHERE item_key='9'",
        [i64::from(crate::distill::prompts::PROMPT_VERSION)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_evidence
                 (tracker, project, item_kind, item_key, ordinal, field, source_kind, source_id,
                  byte_start, byte_end, quote, repo_id)
             VALUES \
         ('github','o/r','change_request','9',0,'root_cause','item','9',0,3,'the','repoA')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_alternatives
                 (tracker, project, item_kind, item_key, ordinal, alternative, repo_id)
             VALUES ('github','o/r','change_request','9',0,'do nothing','repoA')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, anchor_kind, file_path, name, resolved,
                  candidate_ordinal, selected, repo_id)
             VALUES ('github','o/r','change_request','9','file','src/lib.rs','src/lib.rs',1,
                     0,1,'repoA')",
        [],
    )
    .unwrap();

    // An IDENTICAL rerun preserves the model's work (same input hash + pipeline version).
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let cause: Option<String> = conn
        .query_row("SELECT root_cause FROM papertrail_distill WHERE item_key='9'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cause.as_deref(), Some("the cause"), "identical rerun keeps model output");
    let stamps: (Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT prompt_version, model_input_hash FROM papertrail_distill WHERE item_key='9'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        stamps,
        (Some(i64::from(crate::distill::prompts::PROMPT_VERSION)), Some("sha256:model".into()))
    );
    assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_evidence"), 1);
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE selected = 1"),
        1,
        "identical rerun keeps model-selected anchors",
    );

    // Changing the input (a new comment shifts the assembled units → a new hash) INVALIDATES
    // the model columns and clears the model junctions.
    seed_comment(&conn, "repoA", "change_request", "9", "c-new", false);
    extract(&conn, None, &ExtractOptions::default()).unwrap();
    let cause_after: Option<String> = conn
        .query_row("SELECT root_cause FROM papertrail_distill WHERE item_key='9'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cause_after, None, "regeneration NULLs the stale model columns");
    let stamps_after: (Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT prompt_version, model_input_hash FROM papertrail_distill WHERE item_key='9'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stamps_after, (None, None), "regeneration clears model-input stamps");
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_evidence"),
        0,
        "regeneration clears stale evidence",
    );
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_alternatives"),
        0,
        "regeneration clears stale alternatives",
    );
    assert_eq!(
        distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE selected = 1"),
        0,
        "regeneration clears stale anchor selections",
    );
}
