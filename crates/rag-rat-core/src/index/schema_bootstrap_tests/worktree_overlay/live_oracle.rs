//! The live-oracle stage against REAL linked worktrees sharing one database (#1010).
//!
//! The unit tests next to the stage exercise its bookkeeping with hand-built state. These drive
//! the production seam instead: real checkouts, real scope switching, and the real root derivation
//! — the parts that can only be wrong against an actual repository.
//!
//! No language server is assumed present, so a spawn declines and the work stays in the backlog.
//! That is exactly the state these assert on: which checkout the stage decided to act for, where
//! it would have rooted the server, and what scope the connection is left in.

use super::*;

/// A config with the live stage enabled, rooted at `root`.
fn live_source_config(root: PathBuf) -> Config {
    let mut config = source_config(root, Language::Rust);
    config.oracle.live.enabled = true;
    config
}

/// A main checkout with one committed Rust file, plus `count` linked worktrees.
///
/// Returns the scratch GUARDS, not bare paths: a `ScratchRoot` removes its directory on drop, so
/// handing back only the paths would delete every checkout the moment this returns.
fn repo_with_worktrees(count: usize, subdir: Option<&str>) -> (ScratchRoot, Vec<ScratchRoot>) {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    let src = subdir.map_or_else(|| main.join("src"), |sub| main.join(sub).join("src"));
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("base.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);

    let linked = (0..count)
        .map(|index| {
            let path = unique_temp_root();
            let _ = fs::remove_dir_all(&path);
            let branch = format!("feat{index}");
            run_git(&main, &["worktree", "add", "-q", "-b", &branch, path.to_str().unwrap()]);
            path
        })
        .collect();
    (main, linked)
}

/// The changed-set shape a pass produces when only a linked checkout moved: the base hint is
/// `Some(empty)` (a reliable superset that happens to be empty), and the overlay names the paths.
fn linked_only_change(
    worktree: &Path,
    paths: &[&str],
) -> std::collections::BTreeMap<String, crate::watch::CheckoutReindex> {
    std::collections::BTreeMap::from([(
        crate::index::worktree_id_of(worktree),
        crate::watch::CheckoutReindex {
            source_root: worktree.to_path_buf(),
            paths: paths.iter().map(PathBuf::from).collect(),
            coverage: crate::index::ChangedPathsCoverage::Complete,
        },
    )])
}

#[test]
fn a_linked_only_pass_acts_for_the_linked_checkout_and_leaves_the_base_scope_restored() {
    // The whole point of #1010: an edit that exists only in a linked worktree must reach the live
    // stage keyed to THAT checkout. It must also not leave the connection scoped there — the base
    // reconcile, gc, and memory validation that follow all assume base scope.
    let (main_dir, linked_dirs) = repo_with_worktrees(2, None);
    let main = rag_rat_base::paths::canonicalize(&main_dir).unwrap();
    let edited = rag_rat_base::paths::canonicalize(&linked_dirs[0]).unwrap();
    let quiet = rag_rat_base::paths::canonicalize(&linked_dirs[1]).unwrap();
    let (edited, quiet) = (&edited, &quiet);
    let config = live_source_config(main.clone());
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let _no_server = crate::watch::suppress_live_spawn();
    let mut tail = crate::watch::LiveOracleTail::new();
    let empty_base = std::collections::BTreeSet::new();
    let overlays = linked_only_change(edited, &["src/base.rs"]);
    tail.on_pass(&mut db, &config, &crate::watch::LiveChangedSets {
        base: Some(&empty_base),
        overlays: &overlays,
    })
    .expect("the live stage restored the base scope");

    let edited_id = crate::index::worktree_id_of(edited);
    let roots = tail.checkout_roots();
    assert!(
        roots.iter().any(|(id, _)| id == &edited_id),
        "the edited linked checkout must be the one acted for: {roots:?}",
    );
    assert!(
        !roots.iter().any(|(id, _)| id == &crate::index::worktree_id_of(quiet)),
        "an unedited sibling must not be given live state: {roots:?}",
    );
    assert!(
        !roots.iter().any(|(id, _)| id.is_empty()),
        "an empty base change set must not claim a checkout slot: {roots:?}",
    );
    assert_eq!(
        tail.backlog_for(&edited_id),
        vec!["src/base.rs".to_string()],
        "no language server is present, so the path stays retained for a later pass",
    );
    assert_eq!(
        db.active_worktree_id,
        crate::index::worktree_id_of(&config.root),
        "the connection must be back on the BASE scope, not left on the linked checkout (the base \
         scope's id is the root's own, not an empty string)",
    );
}

#[test]
fn a_linked_checkouts_server_is_rooted_at_its_own_equivalent_of_the_config_root() {
    // With a subdir-rooted config the checkout root and the server root differ (`<linked>` vs
    // `<linked>/crate`). A server rooted at the checkout would initialize a workspace that does
    // not contain the indexed sources, and the pass's own guard would then reject its verdicts.
    let (main_dir, linked_dirs) = repo_with_worktrees(1, Some("crate"));
    let main = rag_rat_base::paths::canonicalize(&main_dir).unwrap();
    let linked = rag_rat_base::paths::canonicalize(&linked_dirs[0]).unwrap();
    let config = live_source_config(main.join("crate"));
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let _no_server = crate::watch::suppress_live_spawn();
    let mut tail = crate::watch::LiveOracleTail::new();
    let empty_base = std::collections::BTreeSet::new();
    let overlays = linked_only_change(&linked, &["src/base.rs"]);
    tail.on_pass(&mut db, &config, &crate::watch::LiveChangedSets {
        base: Some(&empty_base),
        overlays: &overlays,
    })
    .unwrap();

    let edited_id = crate::index::worktree_id_of(&linked);
    let root = tail
        .checkout_roots()
        .into_iter()
        .find(|(id, _)| id == &edited_id)
        .map(|(_, root)| root)
        .expect("the linked checkout is held");
    assert_eq!(
        root,
        linked.join("crate"),
        "the server root is the checkout's own equivalent of config.root, not the checkout root",
    );
}

#[test]
fn a_linked_checkout_is_judged_against_its_own_target_bindings() {
    // A branch may ADD a target the main checkout does not have. The live stage must judge that
    // checkout against ITS bindings, exactly as the overlay refresh indexed it: with the main
    // config's targets the backend's language gate rejects the added language — and that arm has
    // already taken the backlog — so the linked edit is dropped outright rather than deferred.
    let (main_dir, linked_dirs) = repo_with_worktrees(1, None);
    let main = rag_rat_base::paths::canonicalize(&main_dir).unwrap();
    let linked = rag_rat_base::paths::canonicalize(&linked_dirs[0]).unwrap();

    // The branch adds a TypeScript target; the main config below stays Rust-only.
    fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\ntypescript = [\"web\"]\n",
    )
    .unwrap();
    fs::create_dir_all(linked.join("web")).unwrap();
    fs::write(linked.join("web/app.ts"), "export function appFn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds a typescript target"]);

    let config = live_source_config(main.clone());
    assert!(
        config.targets.iter().all(|target| target.language != Language::TypeScript),
        "the main config must NOT carry the branch's added target, or this proves nothing",
    );
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let _no_server = crate::watch::suppress_live_spawn();
    let mut tail = crate::watch::LiveOracleTail::new();
    let empty_base = std::collections::BTreeSet::new();
    let overlays = linked_only_change(&linked, &["web/app.ts"]);
    tail.on_pass(&mut db, &config, &crate::watch::LiveChangedSets {
        base: Some(&empty_base),
        overlays: &overlays,
    })
    .unwrap();

    // No language server is installed, so the spawn declines and the path rides the backlog. It
    // only gets that far if the branch's bindings were used: judged against the main config the
    // backend returns at its language gate, having already drained the backlog.
    assert_eq!(
        tail.backlog_for(&crate::index::worktree_id_of(&linked)),
        vec!["web/app.ts".to_string()],
        "the branch's own target bindings must decide what this checkout's backends serve",
    );
}

#[test]
fn a_checkout_whose_work_turns_out_not_to_be_real_releases_its_slot_to_a_sibling() {
    // The structural rule behind the whole admission story: a slot is claimed by DOING work, not
    // by being admitted. Predicting whether a checkout's paths are real means matching everything
    // the run checks — target membership, ignore rules, existence, whether the paths even have
    // oracle candidates — and every approximation leaves a gap where a checkout takes the sole
    // slot and strands a sibling. Here the gap is a stale backlog: a branch retains `.rs` work
    // from an earlier pass and then drops the target that owned it, so nothing predicts the
    // emptiness — it only shows up when the backend drains the worklist at its language gate
    // (#1010).
    let _no_server = crate::watch::suppress_live_spawn();
    let (main_dir, linked_dirs) = repo_with_worktrees(2, None);
    let main = rag_rat_base::paths::canonicalize(&main_dir).unwrap();
    let stale = rag_rat_base::paths::canonicalize(&linked_dirs[0]).unwrap();
    let sibling = rag_rat_base::paths::canonicalize(&linked_dirs[1]).unwrap();
    let config = live_source_config(main.clone());
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    let stale_id = crate::index::worktree_id_of(&stale);
    let sibling_id = crate::index::worktree_id_of(&sibling);

    // Serve the stale checkout FIRST, then the sibling, so the stale one is provably the
    // longest-unserved when both are running on backlog alone — that is what makes it take the
    // single slot in the final pass, which is the situation under test.
    let mut tail = crate::watch::LiveOracleTail::new();
    let empty_base = std::collections::BTreeSet::new();
    tail.on_pass(&mut db, &config, &crate::watch::LiveChangedSets {
        base: Some(&empty_base),
        overlays: &linked_only_change(&stale, &["src/base.rs"]),
    })
    .unwrap();
    tail.on_pass(&mut db, &config, &crate::watch::LiveChangedSets {
        base: Some(&empty_base),
        overlays: &linked_only_change(&sibling, &["src/base.rs"]),
    })
    .unwrap();
    assert_eq!(
        tail.backlog_for(&stale_id),
        vec!["src/base.rs".to_string()],
        "the first checkout is holding real work",
    );
    assert_eq!(
        tail.backlog_for(&sibling_id),
        vec!["src/base.rs".to_string()],
        "and so is the sibling",
    );

    // The first branch now drops Rust entirely. Its retained backlog is suddenly unservable, and
    // nothing in the incoming change set says so — the next pass brings no new paths at all.
    fs::write(
        stale.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\ntypescript = [\"web\"]\n",
    )
    .unwrap();
    fs::create_dir_all(stale.join("web")).unwrap();
    run_git(&stale, &["add", "."]);
    run_git(&stale, &["commit", "-q", "-m", "drop the rust target"]);

    // Backlog-only pass at the default cap of one.
    let no_overlays = std::collections::BTreeMap::new();
    tail.on_pass(&mut db, &config, &crate::watch::LiveChangedSets {
        base: Some(&empty_base),
        overlays: &no_overlays,
    })
    .unwrap();

    assert!(
        tail.backlog_for(&stale_id).is_empty(),
        "the stale checkout's work drained at its language gate: {:?}",
        tail.backlog_for(&stale_id),
    );
    // The point: that turn did NOT consume the single slot, so the sibling ran in the SAME pass.
    assert!(
        tail.served_checkouts().contains(&sibling_id),
        "the sibling was served once the stale checkout's turn proved empty: {:?}",
        tail.served_checkouts(),
    );
    assert!(
        !tail.served_checkouts().contains(&stale_id),
        "and the checkout that did nothing did not claim the slot: {:?}",
        tail.served_checkouts(),
    );
}

#[test]
fn a_checkout_that_stops_being_a_live_sibling_drops_its_state() {
    // `use_worktree_scope` does not fail for a worktree that has gone away — it validates the path
    // and falls back to the BASE scope. A removed checkout would otherwise stay resident with a
    // backlog nothing can resolve, holding a cap slot the live checkouts need.
    let (main_dir, linked_dirs) = repo_with_worktrees(2, None);
    let main = rag_rat_base::paths::canonicalize(&main_dir).unwrap();
    let linked = rag_rat_base::paths::canonicalize(&linked_dirs[0]).unwrap();
    let survivor = rag_rat_base::paths::canonicalize(&linked_dirs[1]).unwrap();
    let config = live_source_config(main.clone());
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    let gone_id = crate::index::worktree_id_of(&linked);
    let survivor_id = crate::index::worktree_id_of(&survivor);

    let _no_server = crate::watch::suppress_live_spawn();
    let mut tail = crate::watch::LiveOracleTail::new();
    let empty_base = std::collections::BTreeSet::new();
    let overlays = linked_only_change(&linked, &["src/base.rs"]);
    tail.on_pass(&mut db, &config, &crate::watch::LiveChangedSets {
        base: Some(&empty_base),
        overlays: &overlays,
    })
    .unwrap();
    assert_eq!(
        tail.backlog_for(&gone_id),
        vec!["src/base.rs".to_string()],
        "the checkout is holding work before it disappears",
    );

    // A sibling picks up work of its own, so there is something to promote into the freed slot.
    let survivor_change = linked_only_change(&survivor, &["src/base.rs"]);
    tail.on_pass(&mut db, &config, &crate::watch::LiveChangedSets {
        base: Some(&empty_base),
        overlays: &survivor_change,
    })
    .unwrap();

    // The first worktree goes away.
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "prune"]);

    let no_overlays = std::collections::BTreeMap::new();
    tail.on_pass(&mut db, &config, &crate::watch::LiveChangedSets {
        base: Some(&empty_base),
        overlays: &no_overlays,
    })
    .unwrap();

    assert!(
        !tail.checkout_roots().iter().any(|(id, _)| id == &gone_id),
        "a checkout that is no longer a live sibling must be dropped: {:?}",
        tail.checkout_roots(),
    );
    // The slot it was holding is freed IN THIS PASS, not left idle: dead checkouts are dropped
    // before the cap ranks, so the surviving sibling is promoted rather than sitting unserved —
    // and an unserved checkout schedules no wake, so with the periodic sweep disabled its work
    // would otherwise be stranded until an unrelated filesystem event.
    assert_eq!(
        tail.backlog_for(&survivor_id),
        vec!["src/base.rs".to_string()],
        "the surviving sibling still holds its work",
    );
    assert!(
        tail.served_checkouts().contains(&survivor_id),
        "and it was promoted into the freed slot this pass: {:?}",
        tail.served_checkouts(),
    );
    assert_eq!(
        db.active_worktree_id,
        crate::index::worktree_id_of(&config.root),
        "and the base scope is still restored",
    );
}
