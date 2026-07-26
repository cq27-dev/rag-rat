use super::*;

/// One committed base repo + one linked worktree, the shared #822/#825 shape: `(main, linked,
/// config, db)` with `src/a.rs` committed on both sides and every overlay basis unrecorded.
fn quiet_window_fixture() -> (ScratchRoot, ScratchRoot, Config, IndexDatabase) {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_a() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    (main, linked, config, db)
}

#[test]
fn overlay_quiet_window_skips_a_dirty_only_listed_worktree_inside_the_window() {
    // #822: dirty working-tree churn fires events that LIST the worktree, and pre-#822 every such
    // pass re-paid the full per-worktree probe (tree diff + status walk + ignore compile). While
    // both heads still equal the recorded basis AND the last complete refresh is younger than the
    // quiet window, the scoped pass skips the worktree outright; the window elapsing re-arms the
    // refresh.
    let (main, linked, mut config, mut db) = quiet_window_fixture();
    config.watch.overlay_quiet_secs = 300;

    // The All pass records the basis WITH its fresh timestamp (the window anchor).
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    // Dirty-only churn: no HEAD moves, the event scope lists the worktree.
    fs::write(linked.join("src/a.rs"), "pub fn dirty_fn() {}\n").unwrap();
    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([linked.clone()]));
    assert!(
        !crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "inside the window a dirty-only listed worktree is skipped"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["base_a".to_string()],
        "the dirty edit stays un-overlaid while the quiet window holds"
    );

    // The window elapsing re-arms the refresh: backdate the recorded timestamp past the window.
    set_base_scope(&mut db, &main);
    let worktree_id = crate::index::worktree_id_of(&linked);
    let (base_sha, linked_head) = db.worktree_overlay_basis(&worktree_id).unwrap().unwrap();
    db.record_worktree_overlay_basis(
        &worktree_id,
        &base_sha,
        &linked_head,
        rag_rat_base::time::now_ms() - 301_000,
    )
    .unwrap();
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "an elapsed window refreshes the dirty-only worktree"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["dirty_fn".to_string()],
        "the deferred dirty edit lands once the window elapses"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_quiet_window_never_defers_a_head_move() {
    // #822: committed freshness stays immediate — a commit moves a HEAD, the recorded basis
    // mismatches, and the refresh runs no matter how young the window is. Both directions: a
    // linked commit, then a base commit.
    let (main, linked, mut config, mut db) = quiet_window_fixture();
    config.watch.overlay_quiet_secs = 300;
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    // Linked commit, seconds into the window, on a pass that lists the worktree.
    fs::write(linked.join("src/new.rs"), "pub fn committed_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);
    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([linked.clone()]));
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "a linked HEAD move refreshes immediately inside the window"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/new.rs"), vec!["committed_fn".to_string()]);

    // Base commit: every worktree's diff basis moves; even an EMPTY event scope refreshes.
    fs::write(main.join("src/a.rs"), "pub fn base_v2() {}\n").unwrap();
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base v2"]);
    set_base_scope(&mut db, &main);
    let empty = crate::watch::OverlayScope::Linked(std::collections::BTreeSet::new());
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &empty),
        "a base HEAD move refreshes immediately inside the window"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_quiet_window_zero_disables_and_sweeps_ignore_it() {
    // #822: `overlay_quiet_secs = 0` restores per-pass refresh for listed worktrees, and an `All`
    // sweep never consults the window even when it is armed — the sweep is one of the backstops
    // the window's staleness bound leans on.
    let (main, linked, mut config, mut db) = quiet_window_fixture();
    config.watch.overlay_quiet_secs = 0;
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    fs::write(linked.join("src/a.rs"), "pub fn dirty_v1() {}\n").unwrap();
    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([linked.clone()]));
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "0 disables the window: a dirty-only listed worktree refreshes every pass"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["dirty_v1".to_string()]);

    // Arm the window; the All sweep refreshes the fresh dirty edit regardless.
    set_base_scope(&mut db, &main);
    config.watch.overlay_quiet_secs = 300;
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    fs::write(linked.join("src/a.rs"), "pub fn dirty_v2() {}\n").unwrap();
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["dirty_v2".to_string()],
        "an All sweep is never held back by the quiet window"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_quiet_window_is_ignored_when_the_periodic_sweep_is_disabled() {
    // #822: the window defers work it never schedules — a deferred edit is delivered by the next
    // post-window event pass or by the periodic sweep. With `periodic_sweep_secs = 0` a one-off
    // dirty edit skipped inside the window could have neither (no follow-up event, no sweep) and
    // stay invisible indefinitely, so the gate must disable itself.
    let (main, linked, mut config, mut db) = quiet_window_fixture();
    config.watch.overlay_quiet_secs = 300;
    config.watch.periodic_sweep_secs = 0;
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    fs::write(linked.join("src/a.rs"), "pub fn dirty_fn() {}\n").unwrap();
    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([linked.clone()]));
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "no sweep backstop → the window is ignored and the pass refreshes"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["dirty_fn".to_string()]);

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn a_cleared_basis_never_quiet_skips() {
    // #822: the timestamp rides the basis value, so the #577 clear paths (partial refresh in-txn,
    // failed refresh caller-side) drop the window anchor with the pair — after a clear, a scoped
    // pass must refresh even though the window would otherwise still hold.
    let (main, linked, mut config, mut db) = quiet_window_fixture();
    config.watch.overlay_quiet_secs = 300;
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    fs::write(linked.join("src/a.rs"), "pub fn dirty_fn() {}\n").unwrap();
    // The failure path's clear (the watcher's Err arm calls exactly this).
    let worktree_id = crate::index::worktree_id_of(&linked);
    db.clear_worktree_overlay_basis(&worktree_id).unwrap();
    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([linked.clone()]));
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "no recorded basis, no quiet skip — the pass refreshes"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["dirty_fn".to_string()]);

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_basis_value_parses_stamped_and_legacy_shapes() {
    // #822: the basis meta value grew a third line (the last-complete-refresh timestamp). A
    // pre-#822 two-line value must still yield the #577 pair — only the quiet window stays
    // disarmed — and a malformed timestamp degrades the same way instead of poisoning the pair.
    let (main, linked, _config, db) = quiet_window_fixture();

    db.record_worktree_overlay_basis("wt-stamped", "base-1", "linked-1", 42).unwrap();
    assert_eq!(
        db.worktree_overlay_basis("wt-stamped").unwrap(),
        Some(("base-1".to_string(), "linked-1".to_string()))
    );
    assert_eq!(db.worktree_overlay_basis_refreshed_at_ms("wt-stamped").unwrap(), Some(42));

    // A legacy value written by a pre-#822 build.
    db.set_repo_meta_if_changed("worktree_overlay_basis:wt-legacy", "base-2\nlinked-2").unwrap();
    assert_eq!(
        db.worktree_overlay_basis("wt-legacy").unwrap(),
        Some(("base-2".to_string(), "linked-2".to_string())),
        "the pair survives a pre-#822 value"
    );
    assert_eq!(
        db.worktree_overlay_basis_refreshed_at_ms("wt-legacy").unwrap(),
        None,
        "no timestamp on a legacy value — the quiet window never holds"
    );

    db.set_repo_meta_if_changed("worktree_overlay_basis:wt-garbage", "base-3\nlinked-3\nnot-ms")
        .unwrap();
    assert_eq!(
        db.worktree_overlay_basis("wt-garbage").unwrap(),
        Some(("base-3".to_string(), "linked-3".to_string()))
    );
    assert_eq!(db.worktree_overlay_basis_refreshed_at_ms("wt-garbage").unwrap(), None);

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}
