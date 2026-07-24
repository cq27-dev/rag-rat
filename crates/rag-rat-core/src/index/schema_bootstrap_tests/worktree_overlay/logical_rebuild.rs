use super::*;

/// Whether the repo's `logical_symbols` grouping has a row named `name` — the table
/// symbol_lookup / graph nav resolve through, i.e. the direct observable for "the repo-global
/// rebuild ran (or didn't) since these symbols were indexed".
pub(super) fn logical_symbol_named(db: &IndexDatabase, name: &str) -> bool {
    db.storage
        .connection()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM main.logical_symbols WHERE repo_id = ?1 AND logical_name \
             = ?2)",
            rusqlite::params![db.active_repo_id, name],
            |row| row.get(0),
        )
        .unwrap()
}

/// The #819 pending-rebuild marker, so tests can assert a batch commits it with the overlay rows
/// and the batch tail consumes it.
pub(super) fn logical_rebuild_pending(db: &IndexDatabase) -> bool {
    db.repo_meta("overlay_logical_rebuild_pending").unwrap().is_some()
}

#[test]
fn overlay_batch_pass_rebuilds_logical_symbols_once() {
    // #819: `logical_symbols` is repo-scoped but scope-INDEPENDENT, so a pass refreshing K
    // changed worktrees needs exactly ONE repo-global rebuild — per-worktree inline rebuilds
    // are K−1 redundant DELETE-all + re-derive passes (only the last one's output survives).
    // The counter is the cardinality observable: the old inline behavior paid one rebuild per
    // changed worktree and fails this assertion.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let wt_one = unique_temp_root();
    let _ = fs::remove_dir_all(&wt_one);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-one", wt_one.to_str().unwrap()]);
    fs::write(wt_one.join("src/one.rs"), "pub fn one_fn() {}\n").unwrap();
    run_git(&wt_one, &["add", "."]);
    run_git(&wt_one, &["commit", "-q", "-m", "one"]);
    let wt_two = unique_temp_root();
    let _ = fs::remove_dir_all(&wt_two);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-two", wt_two.to_str().unwrap()]);
    fs::write(wt_two.join("src/two.rs"), "pub fn two_fn() {}\n").unwrap();
    run_git(&wt_two, &["add", "."]);
    run_git(&wt_two, &["commit", "-q", "-m", "two"]);

    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    let after = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(after - before, 1, "two changed worktrees, ONE repo-global rebuild");
    assert!(!logical_rebuild_pending(&db), "the batch tail consumed the pending marker");
    // The deferred rebuild still lands every branch's new symbols in the grouping — the #219
    // field bug (a newly added overlay file's symbols unresolvable) stays fixed.
    assert!(logical_symbol_named(&db, "one_fn"), "first worktree's new symbol is grouped");
    assert!(logical_symbol_named(&db, "two_fn"), "second worktree's new symbol is grouped");

    // An idle follow-up sweep neither rebuilds nor marks anything pending (#63 idle backstop).
    let idle_before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        idle_before,
        "an unchanged fleet rebuilds nothing"
    );
    assert!(!logical_rebuild_pending(&db));

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&wt_one);
    let _ = fs::remove_dir_all(&wt_two);
}

#[test]
fn pending_logical_rebuild_marker_heals_an_interrupted_batch() {
    // #819 crash-window backstop: a Deferred overlay refresh commits its rows and the pending
    // marker in ONE transaction; if the process dies before the batch tail, the next pass
    // idle-skips the (now unchanged) overlay rows — only the persisted marker can force the
    // owed rebuild then. Without it, a newly added branch file's symbols would stay
    // unresolvable until an unrelated change triggered a rebuild.
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
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    // Simulate the interrupted batch: the refresh commits (rows + marker); the tail never runs.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    let report = db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "the branch file landed as an overlay row");
    assert!(logical_rebuild_pending(&db), "the obligation is committed with the rows");
    assert!(
        !logical_symbol_named(&db, "branch_only_fn"),
        "committed rows without the batch tail leave the grouping stale — the state the persisted \
         marker exists to heal"
    );

    // The next maintenance pass finds every overlay row unchanged (identity-skip) but still
    // runs the owed rebuild off the marker.
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before,
        1,
        "the write-idle pass still runs the owed rebuild"
    );
    assert!(!logical_rebuild_pending(&db), "the healing pass consumed the marker");
    assert!(logical_symbol_named(&db, "branch_only_fn"), "the branch symbol is now grouped");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn inline_rebuild_satisfies_a_pending_deferred_obligation() {
    // #819 review: a stale pending marker (left by an interrupted deferred batch) must be
    // consumed by ANY successful rebuild — here a standalone INLINE overlay refresh — not only
    // by the batch tail. Left set, the next maintenance pass would pay a second wholesale
    // rebuild the inline one already performed.
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
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    // Interrupted deferred batch: rows + marker committed, the tail never runs.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the interrupted batch left its obligation");

    // A later dirty edit in the same worktree, refreshed via the STANDALONE (inline) route.
    fs::write(linked.join("src/more.rs"), "pub fn later_fn() {}\n").unwrap();
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "the dirty edit landed as an overlay row");
    assert!(!logical_rebuild_pending(&db), "the inline rebuild consumed the stale obligation");
    // The rebuild is repo-global, so it grouped the interrupted batch's earlier symbols too.
    assert!(logical_symbol_named(&db, "branch_only_fn"));
    assert!(logical_symbol_named(&db, "later_fn"));

    // The next sweep owes nothing — no second wholesale rebuild.
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "an already-satisfied obligation triggers nothing"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn standalone_unchanged_refresh_settles_a_pending_deferred_obligation() {
    // #819 review P2: an interrupted Deferred batch leaves the pending marker committed. A
    // standalone `index --worktree` (Inline) over the UNCHANGED checkout then has an EMPTY
    // delta — `finalize_overlay_refresh` (and with it the inline rebuild, the sole marker
    // clearer) is skipped entirely — so without the entry-point tail settle the run would
    // exit leaving BOTH the marker and the stale `logical_symbols` in place: branch-only
    // symbols invisible to symbol/graph queries until some unrelated pass consumed the
    // marker. The invariant: EVERY indexing entry point settles a pending obligation before
    // exiting, even when its own delta is empty.
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
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    // Interrupted deferred batch: rows + marker committed, the batch tail never runs.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the interrupted batch left its obligation");
    assert!(!logical_symbol_named(&db, "branch_only_fn"), "the grouping is stale");

    // The standalone (Inline) refresh over the SAME, unchanged worktree: zero row changes.
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(report.indexed, 0, "unchanged checkout: the refresh itself writes no rows");
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before,
        1,
        "the entry-point tail ran the ONE owed rebuild"
    );
    assert!(!logical_rebuild_pending(&db), "the standalone exit consumed the obligation");
    assert!(logical_symbol_named(&db, "branch_only_fn"), "branch symbols are grouped again");

    // Settled means settled: a second unchanged standalone run owes (and pays) nothing.
    let idle = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        idle,
        "no obligation pending: the tail settle is a single meta read"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn idle_base_incremental_pass_settles_a_pending_deferred_obligation() {
    // Same class as the standalone-worktree exit (#819 review): the base incremental writer's
    // logical rebuild is gated on its OWN row changes (indexed/healed/carried/roots_changed),
    // so an IDLE `rag-rat index` pass over an unchanged base tree closes its empty transaction
    // and would exit past a committed obligation. The pass settles at its tail instead — one
    // meta read when nothing is pending.
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
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    // Interrupted deferred batch: rows + marker committed, the batch tail never runs.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the interrupted batch left its obligation");

    // An idle base incremental pass (its own connection, like the CLI `index`): the base tree
    // is unchanged, so the pass's gated rebuild never fires — only the tail settle can.
    let refreshed = IndexDatabase::index_changed(&config).unwrap();
    drop(refreshed);
    assert!(!logical_rebuild_pending(&db), "the idle incremental pass settled the obligation");
    assert!(logical_symbol_named(&db, "branch_only_fn"), "branch symbols are grouped again");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn path_scoped_inline_refresh_settles_a_pending_deferred_obligation() {
    // The path-scoped overlay twin (#679) has the same empty-delta exit as the whole-delta
    // route: an unchanged supplied path identity-skips, `finalize_overlay_refresh` never runs,
    // and a stale pending obligation (#819 review) must still be settled at the entry point's
    // tail. The non-sibling no-op exit is an entry-point exit too.
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
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    // Interrupted deferred batch: rows + marker committed, the batch tail never runs.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the interrupted batch left its obligation");

    // Path-scoped INLINE refresh of the UNCHANGED path: identity-skip, zero row changes.
    let report = db
        .index_worktree_overlay_paths(
            &config,
            &linked,
            &[linked.join("src/new.rs")],
            crate::index::OverlayLogicalRebuild::Inline,
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(report.indexed, 0, "unchanged path: the refresh itself writes no rows");
    assert!(!logical_rebuild_pending(&db), "the path-scoped exit consumed the obligation");
    assert!(logical_symbol_named(&db, "branch_only_fn"), "branch symbols are grouped again");

    // Re-arm the obligation with a second interrupted Deferred refresh...
    fs::write(linked.join("src/more.rs"), "pub fn second_branch_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "second branch file"]);
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the second interrupted batch re-armed it");
    // ...then exit through the NON-SIBLING no-op arm (the base root is not a linked sibling):
    // still an Inline entry-point exit, so it settles too.
    let report = db.index_worktree_overlay(&config, &main, &mut |_| {}).unwrap();
    assert!(report.worktree_id.is_empty(), "base root refresh is the no-op arm");
    assert!(!logical_rebuild_pending(&db), "every Inline entry-point exit settles");
    assert!(logical_symbol_named(&db, "second_branch_fn"), "the settle grouped the new symbol");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_basis_writes_ride_the_refresh_transaction() {
    // #824 (#577 semantics preserved): the skip-proof basis is maintained INSIDE the overlay's
    // own BEGIN IMMEDIATE — record the caller's pair on a COMPLETE refresh (even a no-change
    // one: the proof "this pair is current" is the point), clear it on a PARTIAL one, leave it
    // untouched when no basis is maintained — instead of a separate autocommit per worktree
    // per pass.
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

    // COMPLETE refresh with a maintained basis: the CALLER's pair (read around the refresh,
    // like the watcher's) is recorded in the refresh transaction.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: Some(crate::index::OverlayBasisUpdate {
            base_sha: "base-head-1",
            linked_head_sha: "linked-head-1",
        }),
    };
    let report = db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(report.status_complete, "an undisturbed delta walk completes");
    assert_eq!(
        db.worktree_overlay_basis(&report.worktree_id).unwrap(),
        Some(("base-head-1".to_string(), "linked-head-1".to_string())),
        "a complete refresh records the maintained basis"
    );
    assert!(!logical_rebuild_pending(&db), "a no-change refresh defers no rebuild");

    // PARTIAL refresh clears the recorded proof: a dirty edit moves no HEAD, so a stale pair
    // would keep matching and scoped passes would skip the stale overlay until an `All` sweep
    // (#577 review). Exercised at the tail seam — a real mid-walk gix status failure can't be
    // provoked deterministically.
    db.apply_overlay_basis_tail(
        &report.worktree_id,
        false,
        Some(crate::index::OverlayBasisUpdate {
            base_sha: "base-head-2",
            linked_head_sha: "linked-head-2",
        }),
    )
    .unwrap();
    assert_eq!(
        db.worktree_overlay_basis(&report.worktree_id).unwrap(),
        None,
        "a partial refresh drops the stale skip proof"
    );

    // The standalone shape maintains no basis: whatever is recorded stays untouched.
    db.apply_overlay_basis_tail(
        &report.worktree_id,
        true,
        Some(crate::index::OverlayBasisUpdate {
            base_sha: "base-head-3",
            linked_head_sha: "linked-head-3",
        }),
    )
    .unwrap();
    let standalone = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        db.worktree_overlay_basis(&standalone.worktree_id).unwrap(),
        Some(("base-head-3".to_string(), "linked-head-3".to_string())),
        "a refresh that maintains no basis leaves the recorded pair alone"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}
