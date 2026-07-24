use super::*;

/// How many `logical_symbol_members` rows for `logical_name`'s logical rows resolve to a LIVE
/// symbol row — the #820 relink observable: a member pointing at a deleted symbol id (or a
/// replacement symbol missing its member row) shows up here as a wrong count, which is exactly
/// the breakage a false key-stable verdict would cause.
fn live_member_count(db: &IndexDatabase, logical_name: &str) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.logical_symbol_members m
             JOIN main.logical_symbols ls ON ls.id = m.logical_symbol_id
             JOIN main.symbols s ON s.id = m.symbol_id
             WHERE ls.repo_id = ?1 AND ls.logical_name = ?2",
            rusqlite::params![db.active_repo_id, logical_name],
            |row| row.get(0),
        )
        .unwrap()
}

/// Every grouped row + member of the repo, rendered to comparable strings — so a test can assert
/// the #820 relink path's output is the FIXED POINT of `rebuild_logical_symbols` (running the
/// wholesale rebuild on top of a relinked state changes nothing, byte for byte).
pub(super) fn logical_grouping_snapshot(db: &IndexDatabase) -> Vec<String> {
    let conn = db.storage.connection();
    let mut snapshot = Vec::new();
    let mut rows = conn
        .prepare(
            "SELECT ls.id, ls.language, ls.path, ls.logical_name,
                    COALESCE((SELECT value FROM name_strings WHERE id = ls.qualified_name_id), ''),
                    ls.kind, ls.variant_count, ls.group_reason
             FROM main.logical_symbols ls WHERE ls.repo_id = ?1 ORDER BY ls.id",
        )
        .unwrap()
        .query_map(rusqlite::params![db.active_repo_id], |row| {
            Ok(format!(
                "row {}|{}|{}|{}|{}|{}|{}|{}",
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    snapshot.append(&mut rows);
    let mut members = conn
        .prepare(
            "SELECT m.logical_symbol_id, m.symbol_id, COALESCE(m.cfg_expr, ''),
                    COALESCE(m.signature_hash, ''), m.start_line, m.end_line
             FROM main.logical_symbol_members m
             JOIN main.logical_symbols ls ON ls.id = m.logical_symbol_id
             WHERE ls.repo_id = ?1 ORDER BY m.logical_symbol_id, m.symbol_id",
        )
        .unwrap()
        .query_map(rusqlite::params![db.active_repo_id], |row| {
            Ok(format!(
                "member {}|{}|{}|{}|{}|{}",
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    snapshot.append(&mut members);
    snapshot
}

#[test]
fn a_body_only_overlay_edit_relinks_members_without_a_rebuild() {
    // #820: a body-only edit re-inserts the file's symbols under NEW row ids, but the logical
    // KEY multiset (language, path, name, qualified name, kind, signature) is unchanged — the
    // rebuilt `logical_symbols` table would be identical, so the refresh re-points the members
    // at the replacement symbol ids instead of paying the whole-repo DELETE-all + re-derive.
    // The counter is the observable: pre-#820 this pass rebuilt (delta 1) and fails the zero
    // assertion below.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // MULTI-line fn: the stored signature is the declaration's first line, so a body edit below
    // it keeps all six key columns. (A one-liner's body is part of its signature line — that
    // edit is a key change and correctly keeps the rebuild.)
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    1\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds one_fn"]);

    // First refresh: the overlay file is ADDED — a key-set change, so the batch pays its one
    // rebuild and the branch symbol becomes resolvable.
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert!(logical_symbol_named(&db, "one_fn"), "the added overlay file is grouped");

    // Body-only edit: same declaration line, different body — the key multiset holds.
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    2\n}\n").unwrap();
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before,
        0,
        "a key-stable batch pays ZERO repo-global rebuilds"
    );
    assert!(!logical_rebuild_pending(&db), "a key-stable batch creates no rebuild obligation");
    // The replacement symbols are grouped and their members resolve to LIVE symbol rows — the
    // exact property a false key-stable verdict would break.
    assert!(logical_symbol_named(&db, "one_fn"));
    assert_eq!(live_member_count(&db, "one_fn"), 1, "the member points at the live symbol");

    // The relinked state is the wholesale rebuild's FIXED POINT: forcing the rebuild changes
    // nothing, byte for byte — grouped rows, ids, variant counts, member spans and hashes.
    let relinked = logical_grouping_snapshot(&db);
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(
        logical_grouping_snapshot(&db),
        relinked,
        "relink output must equal the rebuild's output"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn key_altering_edits_force_the_full_rebuild() {
    // #820 adversarial matrix: every change that alters a file's logical-key multiset must keep
    // today's rebuild — a signature change, a rename, an added file, a tombstoned base file,
    // and a pruned branch-only file. A false key-stable verdict on any of these would leave
    // members pointing at deleted symbol ids or stale rows orphaned in the grouping.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    1\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds one_fn"]);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert!(logical_symbol_named(&db, "one_fn"));
    let rebuilds =
        |db: &IndexDatabase| db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);

    // A SIGNATURE change (same name) alters the key.
    fs::write(linked.join("src/one.rs"), "pub fn one_fn(bump: i32) -> i32 {\n    bump\n}\n")
        .unwrap();
    let before = rebuilds(&db);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(rebuilds(&db) - before, 1, "a signature change pays the rebuild");
    assert_eq!(live_member_count(&db, "one_fn"), 1, "the re-derived group is live");

    // A RENAME drops one key and adds another; the stale logical row must be GONE afterwards —
    // exactly the orphan a falsely-engaged relink would leave behind.
    fs::write(linked.join("src/one.rs"), "pub fn one_fn_renamed() -> i32 {\n    3\n}\n").unwrap();
    let before = rebuilds(&db);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(rebuilds(&db) - before, 1, "a rename pays the rebuild");
    assert!(logical_symbol_named(&db, "one_fn_renamed"));
    assert!(!logical_symbol_named(&db, "one_fn"), "the old key's row is dropped, not orphaned");

    // An ADDED file introduces new keys.
    fs::write(linked.join("src/two.rs"), "pub fn two_fn() -> i32 {\n    4\n}\n").unwrap();
    let before = rebuilds(&db);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(rebuilds(&db) - before, 1, "an added file pays the rebuild");
    assert!(logical_symbol_named(&db, "two_fn"));

    // A TOMBSTONED base file (deleted in the worktree, present in the base tree) removes keys
    // from the branch's view.
    fs::remove_file(linked.join("src/a.rs")).unwrap();
    let before = rebuilds(&db);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(rebuilds(&db) - before, 1, "a tombstoned file pays the rebuild");

    // A PRUNED branch-only file (deleted from the worktree with no base row to shadow) removes
    // its keys entirely.
    fs::remove_file(linked.join("src/one.rs")).unwrap();
    let before = rebuilds(&db);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(rebuilds(&db) - before, 1, "a pruned overlay row pays the rebuild");
    assert!(!logical_symbol_named(&db, "one_fn_renamed"), "the pruned file's keys are dropped");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn a_key_stable_relink_leaves_a_pending_obligation_for_the_tail() {
    // #820 × #819: the relink path must NEVER clear (or mask) a pre-existing pending rebuild
    // obligation — only `rebuild_logical_symbols` clears the marker. An interrupted Deferred
    // batch leaves the marker committed and an UNGROUPED overlay file behind; a body-only
    // follow-up edit then takes the relink path (its own file's grouping is intact), and the
    // marker must survive that refresh for the batch tail to settle.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    1\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds one_fn"]);
    // A complete first refresh grounds one_fn's grouping.
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert!(logical_symbol_named(&db, "one_fn"));

    // Interrupted Deferred batch (#819's scenario): a NEW branch file's rows + the marker are
    // committed, the batch tail never runs — its symbols stay ungrouped.
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the interrupted batch left its obligation");
    assert!(!logical_symbol_named(&db, "branch_only_fn"), "the grouping is stale");

    // Body-only follow-up edit, refreshed Deferred WITHOUT a tail (the next interrupted-batch
    // window): the edited file is key-stable, so the refresh takes the relink path — and the
    // outstanding obligation must be untouched.
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    2\n}\n").unwrap();
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "the key-stable refresh itself rebuilds nothing"
    );
    assert!(
        logical_rebuild_pending(&db),
        "the relink path must not clear a pre-existing obligation"
    );
    assert_eq!(live_member_count(&db, "one_fn"), 1, "the relink still landed in the refresh txn");

    // The batch tail settles the surviving obligation: ONE rebuild, marker consumed, the
    // interrupted file's symbols finally grouped.
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    assert!(db.apply_pending_logical_rebuild().unwrap(), "the tail found the obligation");
    assert_eq!(db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before, 1);
    assert!(!logical_rebuild_pending(&db));
    assert!(logical_symbol_named(&db, "branch_only_fn"), "the settle grouped the stale file");
    assert_eq!(live_member_count(&db, "one_fn"), 1, "the rebuild agrees with the relink");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn a_body_only_base_edit_pass_relinks_without_a_rebuild() {
    // #820 on the BASE incremental writer: the second body-only edit of an already-dirty file
    // replaces the same scope row under an identical key multiset, so the pass's tail re-links
    // members instead of rebuilding. (The FIRST dirty edit moves the file into the worktree scope —
    // a key-set change for that scope — which #826 now serves with a PATH-SCOPED re-derive rather
    // than the whole-repo rebuild; either way the second edit relinks, and both outputs are the
    // wholesale rebuild's fixed point.)
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() -> i32 {\n    1\n}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    // First dirty edit: the file enters the worktree overlay scope (an ADD there — a key-set
    // change). #826 serves it with the path-scoped re-derive, so the pass runs ZERO whole-repo
    // rebuilds; the resulting grouping is still correct (the relink fixed point below builds on
    // it).
    fs::write(main.join("src/a.rs"), "pub fn base_fn() -> i32 {\n    2\n}\n").unwrap();
    let first = IndexDatabase::index_paths(&config, &[main.join("src/a.rs")]).unwrap();
    assert_eq!(
        first.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "#826: the scope move is a key-set change served by the path-scoped re-derive, not a \
         whole-repo rebuild"
    );
    assert!(
        logical_symbol_named(&first, "base_fn"),
        "the scoped re-derive grouped the overlay add"
    );
    drop(first);

    // Second body-only edit: same scope row replaced, identical key multiset — zero rebuilds
    // on the whole pass (pre-#820 this was 1).
    fs::write(main.join("src/a.rs"), "pub fn base_fn() -> i32 {\n    3\n}\n").unwrap();
    let second = IndexDatabase::index_paths(&config, &[main.join("src/a.rs")]).unwrap();
    assert_eq!(
        second.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a key-stable base pass pays ZERO repo-global rebuilds"
    );
    assert!(logical_symbol_named(&second, "base_fn"));
    // Committed row + dirty overlay row both carry the key → two live members, the overlay one
    // pointing at the replacement symbol id.
    assert_eq!(live_member_count(&second, "base_fn"), 2);

    // Fixed-point check on the base route too.
    let relinked = logical_grouping_snapshot(&second);
    second.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(logical_grouping_snapshot(&second), relinked);

    let _ = fs::remove_dir_all(&main);
}
