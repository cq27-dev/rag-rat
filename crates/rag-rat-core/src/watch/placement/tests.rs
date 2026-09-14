use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use notify::event::{
    AccessKind, AccessMode, CreateKind, EventKind, Flag, ModifyKind, RemoveKind, RenameMode,
};
use notify::{Event, RecursiveMode, Watcher as _, recommended_watcher};
use rag_rat_base::config::{Config, LlmConfig, ResolvedTarget, TargetKind, WatchConfig};
use rag_rat_base::language::Language;

use crate::IndexDatabase;
use crate::index::ignore_rules::IgnoreMatcher;
use crate::watch::tests::support::*;
use crate::watch::*;

/// Re-placing a tree must not register a path notify already watches. The watcher re-places
/// routinely — the linked-worktree trees after every pass, the base tree on every `.gitignore`
/// edit — and Windows' `ReadDirectoryChangesW` backend does not release the previous watch when a
/// path is registered again: each duplicate strands a directory handle, a semaphore and a 16 KiB
/// read request, so an unguarded re-place grows handles and memory for as long as the watcher runs
/// (#1269). A directory that REAPPEARS is the exception — its old watch is on a handle that no
/// longer refers to it, so placement must run again.
#[test]
fn re_placing_a_tree_does_not_register_an_already_watched_path_twice() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let scratch = scratch_root(format!("ragrat-watch-dedup-{}-{id}", std::process::id()));
    std::fs::create_dir_all(scratch.join("src/inner")).unwrap();
    let scratch = scratch.canonicalize().unwrap();
    let target_dirs = vec![PathBuf::from(".")];
    let (config, root) = whole_root_config(&scratch, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);

    let counters = placement_counters();
    let mut watcher = RecordingWatcher::default();
    watch_tree_pruned(&mut watcher, &counters, &root, &ignore);
    let first = watcher.watched.clone();
    assert!(first.len() >= 3, "the first placement walks the tree: {first:?}");

    watch_tree_pruned(&mut watcher, &counters, &root, &ignore);
    assert_eq!(
        watcher.watched, first,
        "re-placing an unchanged tree must not hand notify a single duplicate registration",
    );
    assert_eq!(
        counters.counts().0,
        first.len() as u64,
        "a skipped duplicate is not a placement attempt either",
    );

    // The nested directory is deleted and recreated: the recorded watch is on a dead handle, so
    // the create event has to re-place it rather than trust the record.
    let recreated = root.join("src/inner");
    std::fs::remove_dir_all(&recreated).unwrap();
    std::fs::create_dir(&recreated).unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(recreated.clone());
    assert!(
        watch_created_dirs(
            &mut watcher,
            &counters,
            &create,
            &config,
            &target_dirs,
            &mut ignore,
            None
        ),
        "a recreated directory below a target is a placement",
    );
    assert!(
        watcher.watched[first.len()..].iter().any(|(path, _)| path == &recreated),
        "the recreated directory is watched again: {:?}",
        &watcher.watched[first.len()..],
    );
    assert!(
        watcher.unwatched.contains(&recreated),
        "the dead registration is handed back, not merely forgotten: {:?}",
        watcher.unwatched,
    );
}

/// A backend rescan means events were DROPPED, so a watched directory could have been deleted and
/// recreated without a create event ever arriving — every placement record is suspect. The watcher
/// must retire the records (handing the watches back to notify, not merely forgetting them, so the
/// dead registrations are released rather than duplicated) and rebuild. The rebuild has to be as
/// COMPLETE as the retire: the `.gitignore` rule directories, the fleet binary's parent and the
/// worktree registry are placed nowhere but the initial placement, so re-placing only the
/// configured trees would leave a whole category dark for the rest of the process's life (#1269).
#[test]
fn a_rescan_retires_every_placement_and_rebuilds_the_full_watch_state() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let scratch = scratch_root(format!("ragrat-watch-rescan-{}-{id}", std::process::id()));
    let linked = scratch_root(format!("ragrat-watch-rescan-linked-{}-{id}", std::process::id()));
    std::fs::create_dir_all(scratch.join("src")).unwrap();
    rag_rat_base::test_git::run(&scratch, &["init", "-q"]);
    rag_rat_base::test_git::run(&scratch, &["config", "user.email", "t@e"]);
    rag_rat_base::test_git::run(&scratch, &["config", "user.name", "t"]);
    std::fs::write(scratch.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    rag_rat_base::test_git::run(&scratch, &["add", "-A"]);
    rag_rat_base::test_git::run(&scratch, &["commit", "-qm", "seed"]);
    let linked_arg = linked.to_string_lossy().into_owned();
    rag_rat_base::test_git::run(&scratch, &["worktree", "add", "-q", "-b", "feature", &linked_arg]);

    let fleet_bin = scratch.join("bin/rag-rat");
    std::fs::create_dir_all(fleet_bin.parent().unwrap()).unwrap();
    let scratch = scratch.canonicalize().unwrap();
    let target_dirs = vec![PathBuf::from("src")];
    let (config, root) = whole_root_config(&scratch, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    let counters = placement_counters();
    let mut watcher = RecordingWatcher::default();
    let mut linked_worktrees = LinkedWorktreeWatches::default();

    let (_, registry) = place_initial_watch_state(
        &mut watcher,
        &counters,
        &config,
        &target_dirs,
        &ignore,
        Some(fleet_bin.as_path()),
    );
    let placed = watcher.watched.clone();
    let registry = registry.expect("a git checkout has a worktree registry");
    for expected in [root.join("src"), fleet_bin.parent().unwrap().to_path_buf(), registry.clone()]
    {
        assert!(
            placed.iter().any(|(path, _)| path == &expected),
            "{expected:?} is watched by the initial placement: {placed:?}",
        );
    }

    let rebuilt_registry = rebuild_watch_state_after_rescan(
        &mut watcher,
        &counters,
        &config,
        &target_dirs,
        &mut ignore,
        &mut linked_worktrees,
        Some(fleet_bin.as_path()),
    );
    assert_eq!(
        rebuilt_registry.as_ref(),
        Some(&registry),
        "the rebuild hands back the registry for the caller to classify against",
    );

    for (path, _) in &placed {
        assert!(
            watcher.unwatched.contains(path),
            "a rescan hands {path:?} back to notify instead of stranding it: {:?}",
            watcher.unwatched,
        );
    }
    let rebuilt = &watcher.watched[placed.len()..];
    for (path, mode) in &placed {
        assert!(
            rebuilt.contains(&(path.clone(), *mode)),
            "a rescan re-places {path:?} — every category, not just the configured trees: \
             {rebuilt:?}",
        );
    }
}

/// A directory that goes away must drop its placement record. Only a reappearance at the same
/// path, a departed checkout or a backend rescan retires anything otherwise, so a repo that churns
/// uniquely-named directories under a target would grow the map by one key per directory for the
/// life of the watcher (#1269). The record is dropped only when the path is really gone — a remove
/// event racing a recreate must not strip a live directory of its watch.
#[test]
fn a_removed_directory_drops_its_placement_record() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let scratch = scratch_root(format!("ragrat-watch-removed-{}-{id}", std::process::id()));
    std::fs::create_dir_all(scratch.join("src/gone")).unwrap();
    std::fs::create_dir_all(scratch.join("src/stays")).unwrap();
    let scratch = scratch.canonicalize().unwrap();
    let target_dirs = vec![PathBuf::from("src")];
    let (config, root) = whole_root_config(&scratch, &target_dirs);
    let ignore = IgnoreMatcher::compile(&root, &target_dirs);
    let counters = placement_counters();
    let mut watcher = RecordingWatcher::default();
    let mut linked = LinkedWorktreeWatches::default();

    watch_configured_trees(&mut watcher, &counters, &config, &target_dirs, &ignore);
    let gone = root.join("src/gone");
    let stays = root.join("src/stays");
    assert!(watcher.watched.iter().any(|(path, _)| path == &gone), "{:?}", watcher.watched);

    // Still present: a remove event that races a recreate must leave the live watch alone.
    let mut ignore = ignore;
    let racing = Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(stays.clone());
    event_requests_maintenance(
        &mut watcher,
        &counters,
        &racing,
        &config,
        &target_dirs,
        &mut ignore,
        &mut linked,
        None,
    );
    assert!(
        !watcher.unwatched.contains(&stays),
        "a directory that still exists keeps its watch: {:?}",
        watcher.unwatched,
    );

    // Actually gone: the record is retired, so a later directory at that path is watched afresh.
    std::fs::remove_dir_all(&gone).unwrap();
    let removed = Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(gone.clone());
    event_requests_maintenance(
        &mut watcher,
        &counters,
        &removed,
        &config,
        &target_dirs,
        &mut ignore,
        &mut linked,
        None,
    );
    assert!(
        watcher.unwatched.contains(&gone),
        "the departed directory's watch is handed back: {:?}",
        watcher.unwatched,
    );

    std::fs::create_dir(&gone).unwrap();
    let before = watcher.watched.len();
    watch_configured_trees(&mut watcher, &counters, &config, &target_dirs, &ignore);
    assert!(
        watcher.watched[before..].iter().any(|(path, _)| path == &gone),
        "a re-place after the retire watches the path again: {:?}",
        &watcher.watched[before..],
    );
}

/// `git worktree remove` deletes the checkout directory, taking its watches with it; a later `add`
/// can recreate the very same path. The placement records must not survive that round trip, or the
/// restored checkout is skipped as already watched and never observed again (#1269).
#[test]
fn a_linked_checkout_removed_and_re_added_at_the_same_path_is_watched_again() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let scratch = scratch_root(format!("ragrat-watch-readd-{}-{id}", std::process::id()));
    std::fs::create_dir_all(scratch.join("wt/src")).unwrap();
    let scratch = scratch.canonicalize().unwrap();
    let target_dirs = vec![PathBuf::from("src")];
    let (config, root) = whole_root_config(&scratch, &target_dirs);
    let checkout = root.join("wt");
    let counters = placement_counters();
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = LinkedWorktreeWatches::default();

    worktrees.sync(&mut watcher, &counters, &config, vec![checkout.clone()]);
    let placed = watcher.watched.clone();
    assert!(placed.iter().any(|(path, _)| path.starts_with(&checkout)), "{placed:?}");

    // Removed: the checkout leaves the set, and its watches die with the directory.
    worktrees.sync(&mut watcher, &counters, &config, Vec::new());
    assert!(
        watcher.unwatched.iter().any(|path| path.starts_with(&checkout)),
        "a departed checkout's watches are handed back: {:?}",
        watcher.unwatched,
    );

    // Re-added at the same path.
    let before = watcher.watched.len();
    worktrees.sync(&mut watcher, &counters, &config, vec![checkout.clone()]);
    assert!(
        watcher.watched[before..].iter().any(|(path, _)| path.starts_with(&checkout)),
        "the restored checkout is watched again: {:?}",
        &watcher.watched[before..],
    );
}

/// A failed watch placement is COUNTED (so `index_status` can surface silent degradation), and the
/// existing subtree-skip behavior is unchanged: when the top dir's watch fails, `watch_tree_pruned`
/// does not descend, so exactly one failure is recorded for a whole failed subtree — while a
/// succeeding watcher walks every directory. The counters are OWNED per watcher, so each half of
/// this test reads its own instance — no dependence on other tests' placements (the point of the
/// #658 hardening).
#[test]
fn watch_placement_failures_are_counted_and_the_subtree_is_still_skipped() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("child/grandchild")).unwrap();
    let ignore = crate::index::ignore_rules::IgnoreMatcher::compile(root, &[root.to_path_buf()]);

    // A succeeding watcher places a watch on every directory and records no failure.
    let ok_counters = WatchPlacementCounters::default();
    let mut ok = RecordingWatcher::default();
    super::watch_tree_pruned(&mut ok, &ok_counters, root, &ignore);
    let (ok_attempts, ok_failures) = ok_counters.counts();
    assert_eq!(ok_failures, 0, "no failures for a succeeding watcher");
    assert!(ok_attempts >= 3, "succeeding watcher attempts every directory: {ok_attempts}");
    assert!(ok.watched.len() >= 3, "succeeding watcher descends: {:?}", ok.watched);
    assert!(ok_counters.newly_warnable_failures().is_none(), "no failures → never warnable");

    // A failing watcher fails on the top dir; the subtree is not walked, so exactly ONE failure is
    // counted for the whole subtree (the pre-existing coverage gap this observability surfaces).
    let fail_counters = WatchPlacementCounters::default();
    let mut failing = FailingWatcher;
    super::watch_tree_pruned(&mut failing, &fail_counters, root, &ignore);
    let (_, fail_failures) = fail_counters.counts();
    assert_eq!(
        fail_failures, 1,
        "one failure recorded for the top dir; descent stops, so the subtree is not re-attempted"
    );

    // Warning coalescing: a fresh failure is warnable once, then not (the pass would emit exactly
    // one log line for this batch, never one per directory).
    assert_eq!(fail_counters.newly_warnable_failures(), Some(1), "new failures warn once");
    assert!(
        fail_counters.newly_warnable_failures().is_none(),
        "already-warned failures do not re-warn without new ones"
    );
}

/// A watch that fails because the directory does NOT exist is an expected miss (a not-yet-created
/// configured target, a branch-specific subdir, a worktree registry that appears later) — the
/// ancestor/bootstrap/created-dir watches cover it — so it must NOT be counted as a
/// silently-dropped watch. Otherwise, because the persisted value is a never-lowered high-water
/// mark, an optional target dir would peg `index_status` at "degraded" forever and emit a spurious
/// ENOSPC warning (#658 review).
#[test]
fn a_watch_failure_on_an_absent_directory_is_not_counted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let ignore = IgnoreMatcher::compile(root, &[root.to_path_buf()]);
    let counters = WatchPlacementCounters::default();

    // A failing watcher pointed at a directory that does not exist: the watch errors, but the path
    // is absent, so nothing is counted.
    let mut failing = FailingWatcher;
    let absent = root.join("not-created-yet");
    watch_tree_pruned(&mut failing, &counters, &absent, &ignore);
    assert_eq!(counters.counts().1, 0, "an absent target dir is an expected miss, not a drop");

    // The SAME failing watcher on a directory that DOES exist is a genuine dropped watch — counted.
    std::fs::create_dir_all(root.join("present")).unwrap();
    watch_tree_pruned(&mut failing, &counters, &root.join("present"), &ignore);
    assert_eq!(counters.counts().1, 1, "a failed watch on an existing dir is a real drop");
}

/// The post-pass linked-worktree sync places watches AFTER the pass persisted the counter, so the
/// watcher flushes the failure high-water mark at shutdown — otherwise a drop introduced by that
/// sync, on a periodic-sweep-disabled watcher with no further events, would never reach
/// `index_status` (#658 review). Covers the flush persistence directly (the shutdown wiring in
/// `watcher_main` uses the real notify watcher and isn't unit-drivable).
#[test]
fn shutdown_flush_persists_watch_placement_failures() {
    let (_scratch, config, root) = src_checkout_config("watch-shutdown-flush");
    IndexDatabase::rebuild(&config).unwrap();

    // A fresh index has recorded nothing; a flush with zero failures stays a no-op.
    let empty = WatchPlacementCounters::default();
    flush_watch_placement_failures(&config, &empty, Duration::from_secs(3));
    let db = IndexDatabase::open_config(&config).unwrap();
    assert_eq!(db.status(&config.database).unwrap().watch_placement_failures, 0);
    drop(db);

    // Record genuine failures (failing watcher on an existing target), then flush at shutdown.
    let counters = WatchPlacementCounters::default();
    let ignore = IgnoreMatcher::compile(&config.root, &config.target_directories());
    let mut failing = FailingWatcher;
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore);
    let (_, failures) = counters.counts();
    assert!(failures > 0, "a failing watcher on an existing dir records a failure");

    flush_watch_placement_failures(&config, &counters, Duration::from_secs(3));
    let db = IndexDatabase::open_config(&config).unwrap();
    assert_eq!(
        db.status(&config.database).unwrap().watch_placement_failures,
        failures,
        "the shutdown flush persists the count so index_status surfaces it"
    );
}

/// The post-resync flush on the event loop uses a NON-blocking (`Duration::ZERO`) lock acquire so
/// it never stalls event classification (#658 review). Prove that path still persists when the lock
/// is free — i.e. `Duration::ZERO` is a real "try once and take it if free", not "never acquire" —
/// and that it routes through the lightweight config-scoped persist (no `open_config` heals).
#[test]
fn a_nonblocking_flush_persists_the_count_when_the_write_lock_is_free() {
    let (_scratch, config, root) = src_checkout_config("watch-nonblocking-flush");
    IndexDatabase::rebuild(&config).unwrap();

    let counters = WatchPlacementCounters::default();
    let ignore = IgnoreMatcher::compile(&config.root, &config.target_directories());
    let mut failing = FailingWatcher;
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore);
    let (_, failures) = counters.counts();
    assert!(failures > 0);

    flush_watch_placement_failures(&config, &counters, Duration::ZERO);
    let db = IndexDatabase::open_config(&config).unwrap();
    assert_eq!(
        db.status(&config.database).unwrap().watch_placement_failures,
        failures,
        "a non-blocking flush persists when the lock is free"
    );
}

/// The flush must stay SIDE-EFFECT-FREE on a first-time-empty checkout: a watcher that placed a
/// watch which failed but never built an index (nothing to discover yet) must NOT leave a
/// schemaless `.rag-rat/index.sqlite` behind — that would poison the friendly no-index read path
/// (#658 review).
#[test]
fn a_flush_on_a_checkout_without_an_index_creates_no_db_file() {
    // A source file exists (so the later rebuild has something to index), but no index has been
    // built yet — a source file on disk does not create the `.rag-rat/index.sqlite`.
    let (_scratch, config, root) = src_checkout_config("watch-flush-no-index");
    assert!(!config.database.exists(), "precondition: no index file yet");

    // Record real placement failures, then flush — as the shutdown path would.
    let counters = WatchPlacementCounters::default();
    let ignore = IgnoreMatcher::compile(&config.root, &config.target_directories());
    let mut failing = FailingWatcher;
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore);
    assert!(counters.counts().1 > 0);

    flush_watch_placement_failures(&config, &counters, Duration::from_secs(3));
    assert!(
        !config.database.exists(),
        "flushing on a checkout with no index must not create a schemaless DB file"
    );

    // ...but once the index IS created (as the shutdown discover does when content arrives in the
    // last debounce window), a subsequent flush persists the count — which is why the shutdown
    // flush runs AFTER `shutdown_discover`, not before (#658 review).
    IndexDatabase::rebuild(&config).unwrap();
    flush_watch_placement_failures(&config, &counters, Duration::from_secs(3));
    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        counters.counts().1,
        "a flush after the index is created writes the count into the freshly-created index"
    );
}

/// The non-blocking flush must SKIP (not block, not error) when another SQLite writer holds the
/// database — the event loop must never stall on classification/fleet triggers (#658 review). A
/// second connection holding a write transaction stands in for another repo's writer in a
/// consolidated DB; `busy_timeout = 0` turns the contended write into an immediate SKIP.
#[test]
fn a_nonblocking_flush_skips_a_busy_database_instead_of_blocking() {
    let (_scratch, config, root) = src_checkout_config("watch-busy-flush");
    IndexDatabase::rebuild(&config).unwrap();

    // Persist an initial count (lock free) so we can prove a later busy flush does NOT overwrite
    // it.
    let counters = WatchPlacementCounters::default();
    let ignore = IgnoreMatcher::compile(&config.root, &config.target_directories());
    let mut failing = FailingWatcher;
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore);
    let first = counters.counts().1;
    assert!(first > 0);
    flush_watch_placement_failures(&config, &counters, Duration::from_secs(3));
    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        first
    );

    // Grow the in-memory count, then hold the SQLite write lock from a second connection and
    // attempt a non-blocking flush of the higher count. `BEGIN IMMEDIATE` reserves the WAL
    // writer; the flush's `busy_timeout = 0` write must fail fast and SKIP, leaving the stored
    // value untouched.
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore);
    let grown = counters.counts().1;
    assert!(grown > first, "the second placement raised the in-memory count");
    let blocker = rusqlite::Connection::open(&config.database).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    flush_watch_placement_failures(&config, &counters, Duration::ZERO);
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        first,
        "a flush against a busy DB skips rather than blocking or overwriting"
    );

    // With the writer gone, the same flush persists the grown count — proving it was a skip, not a
    // loss.
    flush_watch_placement_failures(&config, &counters, Duration::ZERO);
    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        grown,
        "once the DB is free the flush persists the higher count"
    );
}

#[test]
fn gitignore_rule_watch_dirs_include_target_ancestors() {
    let root = PathBuf::from("repo");
    let dirs =
        gitignore_rule_watch_dirs(&root, &[PathBuf::from("src/generated"), PathBuf::from(".")]);
    assert!(dirs.contains(&root), "the config root itself is watched");
    assert!(
        dirs.contains(&root.join("src")),
        "a target's parent can carry a .gitignore governing files below it",
    );
    assert!(
        dirs.contains(&root.join("src/generated")),
        "the target root can carry its own .gitignore",
    );
    let unique = dirs.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(dirs.len(), unique.len(), "watch directories are de-duplicated");

    let rejected = gitignore_rule_watch_dirs(&root, &[
        PathBuf::from("../outside"),
        PathBuf::from("/absolute"),
    ]);
    assert_eq!(rejected, vec![root], "non-relative target components are ignored");
}

#[test]
fn recording_watcher_trait_methods_are_covered() {
    let mut watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    watcher.watch(Path::new("repo/src"), RecursiveMode::NonRecursive).unwrap();
    watcher.unwatch(Path::new("repo/src")).unwrap();
    assert_eq!(<RecordingWatcher as notify::Watcher>::kind(), notify::WatcherKind::NullWatcher,);
    assert_eq!(watcher.watched.len(), 1);
}

#[test]
fn overlay_scope_merge_unions_roots_and_all_absorbs() {
    // #577: hints accumulated while the debounce is armed must union attributable roots, and
    // an unattributable hint (rescan, registry change) must widen the whole pass to All.
    let a = OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/a")]));
    let b = OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/b")]));
    assert_eq!(
        a.clone().merge(b.clone()),
        OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/a"), PathBuf::from("/wt/b")])),
        "linked roots union"
    );
    assert_eq!(a.clone().merge(OverlayScope::All), OverlayScope::All, "All absorbs");
    assert_eq!(OverlayScope::All.merge(b), OverlayScope::All, "All absorbs from either side");
    assert_eq!(
        a.clone().merge(OverlayScope::Linked(BTreeSet::new())),
        a,
        "a base-only contribution adds no roots"
    );

    let paths = OverlayScope::Paths(BTreeMap::from([
        (PathBuf::from("/wt/a"), BTreeSet::from([PathBuf::from("/wt/a/src/a.rs")])),
        (PathBuf::from("/wt/b"), BTreeSet::from([PathBuf::from("/wt/b/src/b.rs")])),
    ]));
    assert_eq!(
        paths.merge(OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/a")]))),
        OverlayScope::Paths(BTreeMap::from([
            (PathBuf::from("/wt/a"), BTreeSet::new()),
            (PathBuf::from("/wt/b"), BTreeSet::from([PathBuf::from("/wt/b/src/b.rs")]),),
        ])),
        "a whole-checkout contribution widens only that checkout"
    );

    let a_paths = OverlayScope::Paths(BTreeMap::from([(
        PathBuf::from("/wt/a"),
        BTreeSet::from([PathBuf::from("/wt/a/src/a.rs")]),
    )]));
    let b_paths = OverlayScope::Paths(BTreeMap::from([(
        PathBuf::from("/wt/b"),
        BTreeSet::from([PathBuf::from("/wt/b/src/b.rs")]),
    )]));
    assert_eq!(
        a_paths.merge(b_paths),
        OverlayScope::Paths(BTreeMap::from([
            (PathBuf::from("/wt/a"), BTreeSet::from([PathBuf::from("/wt/a/src/a.rs")])),
            (PathBuf::from("/wt/b"), BTreeSet::from([PathBuf::from("/wt/b/src/b.rs")])),
        ])),
        "a newly-seen checkout retains its event paths"
    );
}

#[test]
fn initial_watch_state_places_base_gitignore_and_fleet_surfaces() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watch-initial-state-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/kept")).unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let root = root.canonicalize().unwrap();

    let (config, root) = whole_root_config(&root, &[PathBuf::from("src")]);
    let target_dirs = config.target_directories();
    let ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let (linked_worktrees, registry) = place_initial_watch_state(
        &mut watcher,
        &placement_counters(),
        &config,
        &target_dirs,
        &ignore,
        Some(&root.join("bin/rag-rat")),
    );

    assert!(linked_worktrees.states.is_empty());
    assert!(registry.is_none(), "non-git fixtures have no worktree registry");
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &root.join("src") && *mode == RecursiveMode::NonRecursive),
        "configured target roots are placed through the initial state helper",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path.as_path() == root.as_path()
                && *mode == RecursiveMode::NonRecursive),
        "the config root is watched for root .gitignore edits",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &root.join("bin") && *mode == RecursiveMode::NonRecursive),
        "fleet hot-upgrade watches the installed binary directory",
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn event_maintenance_helpers_place_dirs_recompile_and_refresh_linked_state() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watch-maint-helper-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/fresh")).unwrap();
    let root = root.canonicalize().unwrap();

    let (config, root) = whole_root_config(&root, &[PathBuf::from("src")]);
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut watcher = RecordingWatcher::default();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(root.join("src/fresh"));

    assert_eq!(
        event_requests_maintenance(
            &mut watcher,
            &placement_counters(),
            &create,
            &config,
            &target_dirs,
            &mut ignore,
            &mut linked_worktrees,
            None,
        ),
        Some(OverlayScope::Linked(BTreeSet::new())),
        "placing a newly-created BASE target dir must request a base-only maintenance pass",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &root.join("src/fresh")
                && *mode == RecursiveMode::NonRecursive),
        "event helper delegates created-directory placement",
    );

    let before_recompile = watcher.watched.len();
    recompile_ignore_and_place_watches(
        &mut watcher,
        &placement_counters(),
        &config,
        &target_dirs,
        &mut ignore,
        &mut linked_worktrees,
    );
    assert!(
        watcher.watched.len() > before_recompile,
        "gitignore recompiles also re-place base target watches",
    );

    sync_linked_worktrees_after_pass(
        &mut watcher,
        &placement_counters(),
        &config,
        &mut linked_worktrees,
    );
    assert!(linked_worktrees.states.is_empty());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn event_maintenance_helper_requests_pass_for_relevant_and_registry_events() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watch-maint-branches-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let root = root.canonicalize().unwrap();

    let (config, root) = whole_root_config(&root, &[PathBuf::from("src")]);
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut watcher = RecordingWatcher::default();
    let relevant_file = mutation_event(root.join("src/lib.rs"));

    assert_eq!(
        event_requests_maintenance(
            &mut watcher,
            &placement_counters(),
            &relevant_file,
            &config,
            &target_dirs,
            &mut ignore,
            &mut linked_worktrees,
            None,
        ),
        Some(OverlayScope::Linked(BTreeSet::new())),
        "a base target edit fires a base-only pass",
    );

    let registry = root.join(".git/worktrees");
    let registry_event = mutation_event(registry.join("feature/HEAD"));
    assert_eq!(
        event_requests_maintenance(
            &mut watcher,
            &placement_counters(),
            &registry_event,
            &config,
            &target_dirs,
            &mut ignore,
            &mut linked_worktrees,
            Some(&registry),
        ),
        Some(OverlayScope::All),
        "a worktree-registry change is unattributable, so the pass sweeps every overlay",
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn initial_watch_state_places_worktree_registry() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let main = scratch_root(format!("ragrat-watch-registry-{}-{id}", std::process::id()));
    let linked = scratch_root(format!("ragrat-watch-registry-linked-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&main);
    let _ = std::fs::remove_dir_all(&linked);
    std::fs::create_dir_all(main.join("src")).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        rag_rat_base::test_git::run(dir, args);
    };
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked_arg = linked.to_string_lossy().into_owned();
    git(&main, &["worktree", "add", "-q", "-b", "feature", &linked_arg]);

    let main = main.canonicalize().unwrap();
    let (config, main) = whole_root_config(&main, &[PathBuf::from("src")]);
    let target_dirs = config.target_directories();
    let ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let (linked_worktrees, registry) = place_initial_watch_state(
        &mut watcher,
        &placement_counters(),
        &config,
        &target_dirs,
        &ignore,
        None,
    );
    let registry = registry.expect("git worktree repo exposes a registry directory");

    assert!(
        !linked_worktrees.states.is_empty(),
        "the linked checkout should receive watcher state",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &registry && *mode == RecursiveMode::NonRecursive),
        "the worktree registry must be watched so add/remove events schedule maintenance",
    );

    git(&main, &["worktree", "remove", "-f", &linked_arg]);
    std::fs::remove_dir_all(&main).ok();
    std::fs::remove_dir_all(&linked).ok();
}

#[test]
fn watch_created_dirs_ignores_non_appearance_events() {
    let root = PathBuf::from("/repo");
    let target_dirs = vec![PathBuf::from("src")];
    let (config, root) = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let access = Event::new(EventKind::Access(AccessKind::Any)).add_path(root.join("src/fresh"));

    assert!(!watch_created_dirs(
        &mut watcher,
        &placement_counters(),
        &access,
        &config,
        &target_dirs,
        &mut ignore,
        None
    ));
    assert!(watcher.watched.is_empty());
}

#[test]
fn missing_config_root_bootstrap_dirs_use_existing_ancestor_chain() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let checkout = scratch_root(format!("ragrat-bootstrap-chain-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&checkout);
    std::fs::create_dir_all(checkout.join("packages")).unwrap();
    let checkout = checkout.canonicalize().unwrap();
    let packages = checkout.join("packages");
    let config_root = packages.join("crate");

    assert_eq!(
        missing_config_root_bootstrap_dirs(&config_root, &checkout),
        vec![checkout.clone(), packages.clone()],
        "the deepest existing ancestor must be watched so its missing child creation is delivered",
    );
    assert!(
        missing_config_root_bootstrap_dirs(&config_root, &checkout.join("sibling")).is_empty(),
        "unrelated bootstrap roots must not gain watches",
    );
    std::fs::create_dir_all(&config_root).unwrap();
    assert!(
        missing_config_root_bootstrap_dirs(&config_root, &checkout).is_empty(),
        "no bootstrap is needed once the config root exists",
    );

    std::fs::remove_dir_all(&checkout).ok();
}

#[test]
fn created_dir_placement_classifies_target_ancestors_and_subtrees() {
    let root = PathBuf::from("/repo");
    let nested = vec![PathBuf::from("src/generated")];
    let (config, root) = whole_root_config(&root, &nested);

    assert_eq!(
        created_dir_placement(&config, &nested, &PathBuf::from("/elsewhere/src"), None),
        CreatedDirPlacement::OutsideTargets,
    );
    assert_eq!(
        created_dir_placement(&config, &nested, &root.join("vendor"), None),
        CreatedDirPlacement::OutsideTargets,
    );
    assert_eq!(
        created_dir_placement(&config, &nested, &root, None),
        CreatedDirPlacement::TargetAncestor,
    );
    assert_eq!(
        created_dir_placement(&config, &nested, &root.join("src"), None),
        CreatedDirPlacement::TargetAncestor,
    );
    assert_eq!(
        created_dir_placement(&config, &nested, &root.join("src/generated"), None),
        CreatedDirPlacement::TargetSubtree,
    );
    assert_eq!(
        created_dir_placement(&config, &nested, &root.join("src/generated/pkg"), None),
        CreatedDirPlacement::TargetSubtree,
    );

    let whole_root = vec![PathBuf::from(".")];
    let (whole_config, root) = whole_root_config(&root, &whole_root);
    assert_eq!(
        created_dir_placement(&whole_config, &whole_root, &root.join("anything"), None),
        CreatedDirPlacement::TargetSubtree,
    );

    let checkout = PathBuf::from("/checkout");
    let subdir_root = checkout.join("packages/crate");
    let (subdir_config, subdir_root) = whole_root_config(&subdir_root, &nested);
    assert_eq!(
        created_dir_placement(&subdir_config, &nested, &checkout.join("packages"), Some(&checkout)),
        CreatedDirPlacement::TargetAncestor,
    );
    assert_eq!(
        created_dir_placement(&subdir_config, &nested, &subdir_root, Some(&checkout)),
        CreatedDirPlacement::TargetAncestor,
    );
    assert_eq!(
        created_dir_placement(&subdir_config, &nested, &checkout.join("vendor"), Some(&checkout)),
        CreatedDirPlacement::OutsideTargets,
    );
}

#[test]
fn event_touches_worktree_matches_checkout_targets_and_registry() {
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: PathBuf::from("/main"),
        database: PathBuf::from("/main/.rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let worktree = PathBuf::from("/wt/feat");
    let registry = PathBuf::from("/main/.git/worktrees");
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);

    // A target file in a linked worktree fires (its overlay needs refreshing).
    assert!(
        event_touches_worktree(
            &mutation_event(worktree.join("src/a.rs")),
            &worktrees,
            Some(&registry),
        )
        .fires()
    );
    // A non-target file in the worktree does not.
    assert!(
        !event_touches_worktree(
            &mutation_event(worktree.join("README.md")),
            &worktrees,
            Some(&registry),
        )
        .fires()
    );
    // A change in the worktree registry (a `git worktree add`/`remove`) fires.
    assert!(
        event_touches_worktree(
            &mutation_event(registry.join("feat/HEAD")),
            &worktrees,
            Some(&registry),
        )
        .fires()
    );
    // A `.gitignore` edit in the linked checkout fires (it changes the overlay's ignored set),
    // mirroring the base classifier (#219 review).
    assert!(
        event_touches_worktree(
            &mutation_event(worktree.join(".gitignore")),
            &worktrees,
            Some(&registry),
        )
        .fires()
    );
    // A `.gitignore` OUTSIDE any watched checkout does not.
    assert!(
        !event_touches_worktree(
            &mutation_event(PathBuf::from("/elsewhere/.gitignore")),
            &worktrees,
            Some(&registry),
        )
        .fires()
    );
    // A read event never fires (anti-feedback, same as the base watcher).
    let read = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
        .add_path(worktree.join("src/a.rs"));
    assert!(!event_touches_worktree(&read, &worktrees, Some(&registry)).fires());
    // A backend rescan fires when there is linked-worktree or registry state to refresh.
    let rescan = Event::new(EventKind::Other).set_flag(Flag::Rescan);
    assert!(event_touches_worktree(&rescan, &worktrees, None).fires());
    assert!(
        event_touches_worktree(&rescan, &LinkedWorktreeWatches::default(), Some(&registry),)
            .fires()
    );
    assert!(!event_touches_worktree(&rescan, &LinkedWorktreeWatches::default(), None).fires());
    // No worktrees and no registry → nothing fires.
    assert!(
        !event_touches_worktree(
            &mutation_event(worktree.join("src/a.rs")),
            &LinkedWorktreeWatches::default(),
            None,
        )
        .fires()
    );
}

#[test]
fn event_touches_worktree_attributes_the_touched_checkout_roots() {
    // #577: the hint names WHICH linked checkouts an event implicates, so the dispatched pass
    // refreshes those overlays instead of sweeping the fleet; registry changes and rescans are
    // unattributable and widen to AllWorktrees.
    let (config, _) = whole_root_config(&PathBuf::from("/main"), &[PathBuf::from("src")]);
    let wt_a = PathBuf::from("/wt/a");
    let wt_b = PathBuf::from("/wt/b");
    let registry = PathBuf::from("/main/.git/worktrees");
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        wt_a.clone(),
        wt_b.clone(),
    ]);

    assert_eq!(
        event_touches_worktree(&mutation_event(wt_a.join("src/a.rs")), &worktrees, None),
        WorktreeEventHint::Paths(BTreeMap::from([(
            wt_a.clone(),
            BTreeSet::from([wt_a.join("src/a.rs")]),
        )])),
        "a target edit is attributed to its own checkout only"
    );
    assert_eq!(
        event_touches_worktree(&mutation_event(wt_b.join("src/b.rs")), &worktrees, None),
        WorktreeEventHint::Paths(BTreeMap::from([(
            wt_b.clone(),
            BTreeSet::from([wt_b.join("src/b.rs")]),
        )])),
    );
    assert_eq!(
        event_touches_worktree(&mutation_event(wt_a.join("README.md")), &worktrees, None),
        WorktreeEventHint::None,
        "a non-target path implicates nothing"
    );
    assert_eq!(
        event_touches_worktree(
            &Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(wt_a.join("src/removed")),
            &worktrees,
            None,
        ),
        WorktreeEventHint::Paths(BTreeMap::from([(wt_a.clone(), BTreeSet::new())])),
        "a removed directory widens its checkout because explicit-path indexing cannot tombstone \
         descendants"
    );
    assert_eq!(
        event_touches_worktree(
            &Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                .add_path(wt_b.join("src/moved")),
            &worktrees,
            None,
        ),
        WorktreeEventHint::Paths(BTreeMap::from([(wt_b.clone(), BTreeSet::new())])),
        "a removal-side rename is directory-ambiguous and must widen its checkout"
    );
    assert_eq!(
        event_touches_worktree(
            &mutation_event(registry.join("feat/HEAD")),
            &worktrees,
            Some(&registry),
        ),
        WorktreeEventHint::AllWorktrees,
        "a registry change (worktree add/remove) is unattributable"
    );
    let rescan = Event::new(EventKind::Other).set_flag(Flag::Rescan);
    assert_eq!(
        event_touches_worktree(&rescan, &worktrees, None),
        WorktreeEventHint::AllWorktrees,
        "a rescan means events were dropped — refresh everything"
    );
    // A branch-local `rag-rat.toml` edit changes the checkout's TARGET SET without moving
    // either HEAD (#577 review): like a `.gitignore` edit, it must fire and be attributed to
    // its checkout so the overlay is refreshed with the new branch config.
    assert_eq!(
        event_touches_worktree(&mutation_event(wt_b.join("rag-rat.toml")), &worktrees, None),
        WorktreeEventHint::Paths(BTreeMap::from([(
            wt_b.clone(),
            BTreeSet::from([wt_b.join("rag-rat.toml")]),
        )])),
        "a linked checkout's config edit fires for that checkout"
    );
    assert_eq!(
        event_touches_worktree(
            &mutation_event(PathBuf::from("/elsewhere/rag-rat.toml")),
            &worktrees,
            None
        ),
        WorktreeEventHint::None,
        "a config file outside any watched checkout does not fire"
    );
}

#[test]
fn linked_worktree_events_honor_its_ignore_rules() {
    // A linked worktree can be watched for a whole-root target, but ignored subtrees must still
    // be dropped before they fire an overlay refresh. This is the classification half of the
    // linked-watch fix: without the per-worktree IgnoreMatcher, `ignored_dir/out.rs` and
    // `target/debug/build.rs` both matched `**/*.rs` and armed the debounce.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree = scratch_root(format!("ragrat-wt-ign-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    std::fs::create_dir_all(worktree.join("ignored_dir")).unwrap();
    std::fs::create_dir_all(worktree.join("target/debug")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join(".gitignore"), "ignored_dir/\ntarget/\n").unwrap();

    let (config, worktree) = whole_root_config(&worktree, &[PathBuf::from(".")]);
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);

    assert!(
        event_touches_worktree(&mutation_event(worktree.join("src/lib.rs")), &worktrees, None)
            .fires(),
        "an unignored linked target file still fires",
    );
    assert!(
        !event_touches_worktree(
            &mutation_event(worktree.join("ignored_dir/out.rs")),
            &worktrees,
            None
        )
        .fires(),
        "a linked worktree gitignored source-looking path must not fire",
    );
    assert!(
        !event_touches_worktree(
            &mutation_event(worktree.join("target/debug/build.rs")),
            &worktrees,
            None
        )
        .fires(),
        "a linked worktree floor/gitignored build path must not fire",
    );
    assert!(
        event_touches_worktree(&mutation_event(worktree.join(".gitignore")), &worktrees, None)
            .fires(),
        "a linked worktree .gitignore edit still fires so rules can be recompiled",
    );
    // The live clangd oracle runs per checkout, so a LINKED worktree spawns its own clangd, which
    // persists an index into that worktree's own `.cache/clangd/`. That tree is machine-written
    // state living inside the checkout — the same category as `.rag-rat`, and floored for the same
    // reason. A source-shaped path there must be classified as ignored rather than armed as an
    // edit; clangd's actual `.idx` artifacts carry no target extension and never fire either way.
    assert!(
        !event_touches_worktree(
            &mutation_event(worktree.join(".cache/clangd/index/lib.rs")),
            &worktrees,
            None
        )
        .fires(),
        "a linked worktree's own clangd index tree must never arm the debounce",
    );
    // …while the rest of a `.cache` the worktree genuinely tracks is still watched, since the
    // floor is deliberately narrower than the whole directory.
    assert!(
        event_touches_worktree(
            &mutation_event(worktree.join(".cache/generated/api.rs")),
            &worktrees,
            None
        )
        .fires(),
        "the narrow floor must not silence a tracked .cache subtree",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_worktree_watch_placement_uses_configured_pruned_targets() {
    // Placement half of the linked-watch fix: linked checkouts used to be subscribed with one
    // `Recursive` watch on the checkout root, which descended into `target/` and any ignored
    // dependency/build tree. They should get the same non-recursive, gitignore-pruned target
    // placement as the main checkout.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree = scratch_root(format!("ragrat-wt-place-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src/kept")).unwrap();
    std::fs::create_dir_all(worktree.join("src/ignored_dir")).unwrap();
    std::fs::create_dir_all(worktree.join("target/debug")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join(".gitignore"), "src/ignored_dir/\ntarget/\n").unwrap();

    let (config, worktree) = whole_root_config(&worktree, &[PathBuf::from("src")]);
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);
    let state = &worktrees.states[0];

    assert_eq!(state.config.root.as_path(), worktree.as_path());
    assert!(
        watcher.watched.iter().any(|(path, mode)| path == &state.config.root.join("src")
            && *mode == RecursiveMode::NonRecursive),
        "the configured target root must be watched non-recursively",
    );
    assert!(
        watcher.watched.iter().any(|(path, mode)| path == &state.config.root.join("src/kept")
            && *mode == RecursiveMode::NonRecursive),
        "non-ignored target subdirs must be watched",
    );
    assert!(
        watcher.watched.iter().all(|(_, mode)| *mode == RecursiveMode::NonRecursive),
        "linked worktrees must not receive a recursive checkout watch: {:?}",
        watcher.watched,
    );
    assert!(
        watcher.watched.iter().all(|(path, _)| !path.starts_with(state.config.root.join("target"))
            && !path.starts_with(state.config.root.join("src/ignored_dir"))),
        "ignored or non-target build trees must not be watched: {:?}",
        watcher.watched,
    );

    std::fs::remove_dir_all(&state.config.root).ok();
}

#[test]
fn linked_worktree_watch_set_sync_rebuilds_existing_root_state() {
    // A linked checkout can keep the same filesystem path while switching to a branch whose
    // local config has different targets. The pass reconciliation must rebuild state for every
    // current root, not only add brand-new roots.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree = scratch_root(format!("ragrat-wt-sync-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    std::fs::create_dir_all(worktree.join("extra")).unwrap();
    let worktree = worktree.canonicalize().unwrap();

    let (src_config, worktree) = whole_root_config(&worktree, &[PathBuf::from("src")]);
    let (extra_config, _) = whole_root_config(&worktree, &[PathBuf::from("extra")]);
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = LinkedWorktreeWatches::default();
    worktrees.sync(&mut watcher, &placement_counters(), &src_config, vec![worktree.clone()]);
    assert_eq!(worktrees.states[0].target_dirs, vec![PathBuf::from("src")]);

    worktrees.sync(&mut watcher, &placement_counters(), &extra_config, vec![worktree.clone()]);
    assert_eq!(worktrees.states.len(), 1);
    assert_eq!(worktrees.states[0].target_dirs, vec![PathBuf::from("extra")]);
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &worktree.join("extra")),
        "sync should place watches for the refreshed target set",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_worktree_watch_set_handles_created_dirs_and_recompile() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree = scratch_root(format!("ragrat-wt-set-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join(".gitignore"), "").unwrap();

    let (config, worktree) = whole_root_config(&worktree, &[PathBuf::from("src")]);
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);

    let fresh = worktree.join("src/fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(fresh.clone());
    assert_eq!(
        worktrees.watch_created_dirs(&mut watcher, &placement_counters(), &create),
        BTreeSet::from([worktree.clone()]),
        "created target dirs should request a maintenance pass scoped to their checkout",
    );
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &fresh),
        "created target dirs are watched through the centralized linked-worktree state",
    );

    std::fs::write(worktree.join(".gitignore"), "src/fresh/\n").unwrap();
    worktrees.recompile_ignore_and_place_watches(&mut watcher, &placement_counters());
    assert!(
        worktrees.states[0].ignore.is_ignored(&fresh, true),
        "recompile refreshes the state's matcher",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_worktree_watch_set_handles_created_target_ancestors() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree = scratch_root(format!("ragrat-wt-ancestor-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&worktree).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join(".gitignore"), "").unwrap();

    let target_dirs = vec![PathBuf::from("src/generated")];
    let (config, worktree) = whole_root_config(&worktree, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);

    watcher.watched.clear();
    let ancestor = worktree.join("src");
    std::fs::create_dir_all(&ancestor).unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(ancestor.clone());
    assert_eq!(
        worktrees.watch_created_dirs(&mut watcher, &placement_counters(), &create),
        BTreeSet::from([worktree.clone()]),
        "created target ancestors should request a maintenance pass after placing watches",
    );

    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &ancestor && *mode == RecursiveMode::NonRecursive),
        "a newly-created linked target ancestor must be watched non-recursively",
    );
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &worktree.join("src/generated")),
        "created ancestors should re-place configured target watches in case the target already \
         exists",
    );
    assert!(
        watcher.watched.iter().all(|(_, mode)| *mode == RecursiveMode::NonRecursive),
        "ancestor handling must not reintroduce recursive checkout watches",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_worktree_target_ancestor_gitignore_is_compiled() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree = scratch_root(format!("ragrat-wt-ancestor-ignore-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src/generated")).unwrap();
    std::fs::create_dir_all(worktree.join("src/sibling")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join("src/.gitignore"), "generated/\n").unwrap();
    std::fs::write(worktree.join("src/sibling/.gitignore"), "marker.rs\n").unwrap();

    let target_dirs = vec![PathBuf::from("src/generated")];
    let (config, worktree) = whole_root_config(&worktree, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);
    let ignore = &worktrees.states[0].ignore;

    assert!(
        ignore.is_ignored(&worktree.join("src/generated/lib.rs"), false),
        "target ancestor .gitignore rules must govern nested linked targets",
    );
    assert!(
        !ignore.is_ignored(&worktree.join("src/sibling/marker.rs"), false),
        "compiling target ancestors must not scan unindexed siblings below that ancestor",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_subdir_root_watch_placement_keeps_checkout_root_when_config_root_missing() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let repo = scratch_root(format!("ragrat-wt-missing-root-main-{}-{id}", std::process::id()));
    let checkout =
        scratch_root(format!("ragrat-wt-missing-root-linked-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&checkout);
    std::fs::create_dir_all(repo.join("packages/crate/src")).unwrap();
    std::fs::write(repo.join("packages/crate/src/lib.rs"), "fn lib() {}\n").unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "t@e"],
        vec!["config", "user.name", "t"],
        vec!["add", "."],
        vec!["commit", "-q", "-m", "base"],
    ] {
        rag_rat_base::test_git::run(&repo, &args);
    }
    std::fs::create_dir_all(checkout.join("packages")).unwrap();
    let checkout = checkout.canonicalize().unwrap();
    let config_root = rag_rat_base::paths::canonicalize(repo.join("packages/crate")).unwrap();
    let target_dirs = vec![PathBuf::from("src")];
    let (config, _) = whole_root_config(&config_root, &target_dirs);

    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        checkout.clone(),
    ]);
    let linked_root = checkout.join("packages/crate");

    assert_eq!(worktrees.states[0].config.root, linked_root);
    assert!(!linked_root.exists(), "the linked branch has not created the configured root yet");
    assert!(
        watcher.watched.iter().any(|(path, mode)| path.as_path() == checkout.as_path()
            && *mode == RecursiveMode::NonRecursive),
        "a missing linked subdir-root needs a non-recursive checkout-root bootstrap watch",
    );
    assert!(
        watcher.watched.iter().any(|(path, mode)| path == &checkout.join("packages")
            && *mode == RecursiveMode::NonRecursive),
        "an existing parent of the missing linked root must be watched for the final component",
    );
    assert!(
        watcher.watched.iter().all(|(_, mode)| *mode == RecursiveMode::NonRecursive),
        "missing-root bootstrapping must not restore recursive checkout watches",
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&checkout).ok();
}

#[test]
fn watch_created_dirs_reinstalls_watches_for_recreated_config_root() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watch-recreated-root-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();
    let root = root.canonicalize().unwrap();
    let target_dirs = vec![PathBuf::from("src")];
    let (config, root) = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(root.clone());

    assert!(
        watch_created_dirs(
            &mut watcher,
            &placement_counters(),
            &create,
            &config,
            &target_dirs,
            &mut ignore,
            None
        ),
        "recreated config roots should re-place target watches and request maintenance",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path.as_path() == root.as_path()
                && *mode == RecursiveMode::NonRecursive),
        "the recreated config root itself should stay watched non-recursively",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &root.join("src") && *mode == RecursiveMode::NonRecursive),
        "configured targets below the recreated root should be watched again",
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn watch_created_dirs_bootstraps_missing_linked_subdir_root_ancestors() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let checkout =
        scratch_root(format!("ragrat-watch-linked-ancestor-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&checkout);
    std::fs::create_dir_all(checkout.join("packages")).unwrap();
    std::fs::create_dir_all(checkout.join("vendor")).unwrap();
    let checkout = checkout.canonicalize().unwrap();
    let packages = checkout.join("packages");
    let target_dirs = vec![PathBuf::from("src")];
    let (config, _) = whole_root_config(&packages.join("crate"), &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let create_packages =
        Event::new(EventKind::Create(CreateKind::Folder)).add_path(packages.clone());

    assert!(
        watch_created_dirs(
            &mut watcher,
            &placement_counters(),
            &create_packages,
            &config,
            &target_dirs,
            &mut ignore,
            Some(&checkout),
        ),
        "an intermediate ancestor of a missing linked config root must keep the bootstrap moving",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &packages && *mode == RecursiveMode::NonRecursive),
        "the appeared ancestor itself must be watched for the next path component",
    );
    assert!(
        watcher.watched.iter().all(|(_, mode)| *mode == RecursiveMode::NonRecursive),
        "missing linked-root ancestors must not reintroduce recursive checkout watches",
    );

    watcher.watched.clear();
    let vendor = checkout.join("vendor");
    let create_vendor = Event::new(EventKind::Create(CreateKind::Folder)).add_path(vendor.clone());
    assert!(
        !watch_created_dirs(
            &mut watcher,
            &placement_counters(),
            &create_vendor,
            &config,
            &target_dirs,
            &mut ignore,
            Some(&checkout),
        ),
        "sibling directories under the checkout are outside the missing config root",
    );
    assert!(watcher.watched.is_empty(), "outside siblings should not gain watches");

    std::fs::remove_dir_all(&checkout).ok();
}

#[test]
fn linked_created_target_dir_requests_maintenance_when_directory_event_is_not_relevant() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree = scratch_root(format!("ragrat-wt-create-pass-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join(".gitignore"), "").unwrap();

    let target_dirs = vec![PathBuf::from("src")];
    let (config, worktree) = whole_root_config(&worktree, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);

    watcher.watched.clear();
    let pkg = worktree.join("src/pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("lib.rs"), "fn pkg() {}\n").unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(pkg.clone());

    assert!(
        !event_touches_worktree(&create, &worktrees, None).fires(),
        "extensionless directory events are not target-file events",
    );
    assert_eq!(
        worktrees.watch_created_dirs(&mut watcher, &placement_counters(), &create),
        BTreeSet::from([worktree.clone()]),
        "placing a linked target-dir watch must request a maintenance pass",
    );
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &pkg),
        "the linked target directory is still watched for subsequent edits",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_created_dir_watch_signal_does_not_short_circuit_state_updates() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let first = scratch_root(format!("ragrat-wt-create-all-a-{}-{id}", std::process::id()));
    let second = scratch_root(format!("ragrat-wt-create-all-b-{}-{id}", std::process::id()));
    for root in [&first, &second] {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".gitignore"), "").unwrap();
    }
    let first = first.canonicalize().unwrap();
    let second = second.canonicalize().unwrap();

    let target_dirs = vec![PathBuf::from("src")];
    let (config, first) = whole_root_config(&first, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        first.clone(),
        second.clone(),
    ]);

    watcher.watched.clear();
    let first_pkg = first.join("src/pkg");
    let second_pkg = second.join("src/pkg");
    std::fs::create_dir_all(&first_pkg).unwrap();
    std::fs::create_dir_all(&second_pkg).unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder))
        .add_path(first_pkg.clone())
        .add_path(second_pkg.clone());

    assert_eq!(
        worktrees.watch_created_dirs(&mut watcher, &placement_counters(), &create),
        BTreeSet::from([first.clone(), second.clone()]),
        "BOTH linked states place their watch and are reported (no short-circuit)",
    );
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &first_pkg),
        "the first linked state should still be updated",
    );
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &second_pkg),
        "the second linked state should still be updated after the first returns true",
    );

    std::fs::remove_dir_all(&first).ok();
    std::fs::remove_dir_all(&second).ok();
}

#[test]
fn watch_created_dirs_skips_dirs_ignored_before_or_after_recompile() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watch-created-ignore-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("src/already_ignored")).unwrap();
    std::fs::create_dir_all(root.join("src/newly_ignored")).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "src/already_ignored/\n").unwrap();

    let target_dirs = vec![PathBuf::from("src")];
    let (config, root) = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    let mut watcher = RecordingWatcher::default();

    let already = root.join("src/already_ignored");
    let create_already =
        Event::new(EventKind::Create(CreateKind::Folder)).add_path(already.clone());
    watch_created_dirs(
        &mut watcher,
        &placement_counters(),
        &create_already,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    assert!(
        watcher.watched.iter().all(|(path, _)| path != &already),
        "a dir ignored before recompile should not be watched",
    );

    std::fs::write(root.join(".gitignore"), "src/already_ignored/\nsrc/newly_ignored/\n").unwrap();
    let newly = root.join("src/newly_ignored");
    let create_newly = Event::new(EventKind::Create(CreateKind::Folder)).add_path(newly.clone());
    watch_created_dirs(
        &mut watcher,
        &placement_counters(),
        &create_newly,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    assert!(
        watcher.watched.iter().all(|(path, _)| path != &newly),
        "a dir ignored only after recompile should not be watched",
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn event_touches_worktree_rebases_subdir_rooted_config() {
    // #219 review: when `config.root` is a repo SUBDIR (`<repo>/crate`), a linked checkout's
    // edit arrives as `<checkout>/crate/src/a.rs`. Stripping only the checkout root leaves
    // `crate/src/a.rs`, which `target_for_path` (config-root-relative, expecting `src/a.rs`)
    // rejects — so the subdir prefix must be stripped too.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let repo = scratch_root(format!("ragrat-wt-subdir-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join("crate/src")).unwrap();
    std::fs::write(repo.join("crate/src/a.rs"), "fn a() {}\n").unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "t@e"],
        vec!["config", "user.name", "t"],
        vec!["add", "."],
        vec!["commit", "-q", "-m", "base"],
    ] {
        rag_rat_base::test_git::run(&repo, &args);
    }
    // `config.root` is the `crate` SUBDIR of the repo.
    let config_root = rag_rat_base::paths::canonicalize(repo.join("crate")).unwrap();
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: config_root,
        database: repo.join("crate/.rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    // A linked checkout mirrors the layout: `<checkout>/crate/src/a.rs`.
    let checkout = scratch_root(format!("ragrat-wt-subdir-co-{}-{id}", std::process::id()));
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        checkout.clone(),
    ]);
    assert!(
        event_touches_worktree(&mutation_event(checkout.join("crate/src/a.rs")), &worktrees, None,)
            .fires(),
        "a subdir-rooted config must fire on a linked edit under <checkout>/<subdir>/<target>"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn event_is_relevant_skips_gitignored_paths_consistently_with_walker() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watchev-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("crates")).unwrap();
    std::fs::write(root.join(".gitignore"), "gen/\n").unwrap();

    let config_root = rag_rat_base::test_scratch::canonical_config_root(root.to_path_buf());
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("crates")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    // The classifier matches an event by stripping `config.root` off its path, and the
    // fixture Config canonicalizes that root — so spell the fixture's paths and ignore
    // rules through the SAME root, exactly as the live watcher (which subscribes under
    // `config.root`) delivers them (#1027).
    let root = config.root.clone();
    let ignore = IgnoreMatcher::compile(&root, &[]);

    // A real source edit under the target fires.
    let src = root.join("crates/lib.rs");
    assert!(event_is_relevant(&config, &ignore, &mutation_event(src)), "source edit fires");

    // A floor dir (target/) never fires, even though it would be language-matched.
    let built = root.join("target/debug/foo.rs");
    assert!(!event_is_relevant(&config, &ignore, &mutation_event(built)), "floor dir skipped");

    // A gitignored dir under root never fires.
    let generated = root.join("gen/out.rs");
    assert!(!event_is_relevant(&config, &ignore, &mutation_event(generated)), "gitignored skipped",);

    // A read of a watched source file never fires (anti-feedback gate), even if not ignored.
    let read = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
        .add_path(root.join("crates/lib.rs"));
    assert!(!event_is_relevant(&config, &ignore, &read), "reads never fire");

    // A creation under the target fires.
    let created =
        Event::new(EventKind::Create(CreateKind::File)).add_path(root.join("crates/new.rs"));
    assert!(event_is_relevant(&config, &ignore, &created), "new source file fires");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn gitignore_edit_is_relevant_and_recompile_reflects_new_rules() {
    // EARLIER-ROUND FINDINGS (kept correct): a `.gitignore` mutation must fire a pass even
    // though `.gitignore` is not a target language, AND recompiling the matcher must make
    // subsequent classification honor the new rules — so a now-ignored file stops firing and a
    // now-unignored file resumes.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watchgi-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("crates")).unwrap();
    // Initially nothing is gitignored.
    std::fs::write(root.join(".gitignore"), "").unwrap();

    let config_root = rag_rat_base::test_scratch::canonical_config_root(root.to_path_buf());
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("crates")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };

    // The classifier matches an event by stripping `config.root` off its path, and the
    // fixture Config canonicalizes that root — so spell the fixture's paths and ignore
    // rules through the SAME root, exactly as the live watcher (which subscribes under
    // `config.root`) delivers them (#1027).
    let root = config.root.clone();
    let ignore = IgnoreMatcher::compile(&root, &[]);
    let secret = root.join("crates/secret.rs");
    // Before the rule edit: a normal source edit fires.
    assert!(event_is_relevant(&config, &ignore, &mutation_event(secret.clone())), "fires pre");

    // A `.gitignore` mutation is itself relevant (finding 4) — even a root `.gitignore`, and
    // even a nested one that has no target language.
    let gi_edit = mutation_event(root.join(".gitignore"));
    assert!(event_is_relevant(&config, &ignore, &gi_edit), "gitignore edit fires a pass");
    let nested_gi = mutation_event(root.join("crates/.gitignore"));
    assert!(event_is_relevant(&config, &ignore, &nested_gi), "nested gitignore edit fires");

    // Now the user adds `secret.rs` to `.gitignore`; recompiling must make the classifier drop
    // it.
    std::fs::write(root.join(".gitignore"), "secret.rs\n").unwrap();
    let ignore = IgnoreMatcher::compile(&root, &[]);
    assert!(
        !event_is_relevant(&config, &ignore, &mutation_event(secret)),
        "recompiled matcher honors the new ignore rule (now-ignored file stops firing)",
    );
    // A different, still-unignored source file keeps firing.
    let other = root.join("crates/keep.rs");
    assert!(event_is_relevant(&config, &ignore, &mutation_event(other)), "unignored still fires");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn worktree_root_gitignore_edit_recompiles_for_subdir_config_root() {
    // FINDING 1 + 3 combined (test d for the subdirectory case): `config.root` is a subdir of a
    // Git worktree. A live edit to the WORKTREE-ROOT `.gitignore` (above config.root) must,
    // after recompiling the shared matcher, drop a now-ignored file under the subdir and keep
    // an unrelated one firing — proving ancestor rules are honored AND the recompile
    // takes effect.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let wt = scratch_root(format!("ragrat-wtgi-{}-{id}", std::process::id()));
    std::fs::create_dir_all(wt.join("crates")).unwrap();
    let wt = wt.canonicalize().unwrap();
    rag_rat_base::test_git::run(&wt, &["init", "-q"]);
    std::fs::write(wt.join(".gitignore"), "").unwrap();

    let sub = wt.join("crates"); // config.root is the subdirectory.
    let target_dirs = vec![PathBuf::from(".")];
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: sub.clone(),
        database: sub.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: target_dirs.clone(),
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };

    // Before: a source file under the subdir fires.
    let ignore = IgnoreMatcher::compile(&sub, &target_dirs);
    let secret = sub.join("secret.rs");
    assert!(event_is_relevant(&config, &ignore, &mutation_event(secret.clone())), "fires pre");

    // Edit the WORKTREE-ROOT `.gitignore` to ignore `secret.rs` repo-wide, then recompile.
    std::fs::write(wt.join(".gitignore"), "secret.rs\n").unwrap();
    let ignore = IgnoreMatcher::compile(&sub, &target_dirs);
    assert!(
        !event_is_relevant(&config, &ignore, &mutation_event(secret)),
        "worktree-root rule (above config.root) drops the file after recompile (finding 1 + 3)",
    );
    assert!(
        event_is_relevant(&config, &ignore, &mutation_event(sub.join("keep.rs"))),
        "an unrelated source file under the subdir still fires",
    );

    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn reads_are_not_mutations_but_writes_are() {
    use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind};
    // Reads must never fire — this is the anti-feedback-loop gate.
    assert!(!kind_is_mutation(&EventKind::Access(AccessKind::Open(AccessMode::Read))));
    assert!(!kind_is_mutation(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
    assert!(!kind_is_mutation(&EventKind::Access(AccessKind::Any)));
    // Real content changes must fire.
    assert!(kind_is_mutation(&EventKind::Create(CreateKind::File)));
    assert!(kind_is_mutation(&EventKind::Remove(RemoveKind::File)));
    assert!(kind_is_mutation(&EventKind::Modify(ModifyKind::Data(DataChange::Any))));
    assert!(kind_is_mutation(&EventKind::Access(AccessKind::Close(AccessMode::Write))));
}

#[test]
fn worktree_watch_targets_excludes_the_main_checkout_for_a_subdir_config_root() {
    // #219 review: when `config.root` is a repo SUBDIR (`<repo>/crate`),
    // `live_worktree_contexts` reports the main checkout as `<repo>` (its workdir), but
    // filtering by `worktree_id_of(config.root)` (`<repo>/crate`) wouldn't match — so
    // the main checkout would be misread as a LINKED worktree and the watcher would
    // recursively subscribe to the whole repo root. The base id must be the ENCLOSING
    // worktree root.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let main = scratch_root(format!("ragrat-wwt-{}-{id}", std::process::id()));
    std::fs::create_dir_all(main.join("crate/src")).unwrap();
    let git =
        |dir: &Path, args: &[&str]| rag_rat_base::test_git::command(dir, args).status().unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join("crate/src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);

    let sub = rag_rat_base::paths::canonicalize(main.join("crate")).unwrap(); // config.root is the subdir.
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: sub.clone(),
        database: sub.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };

    let (roots, _registry) = worktree_watch_targets(&config);
    let main_id = crate::index::worktree_id_of(&rag_rat_base::paths::canonicalize(&main).unwrap());
    assert!(
        !roots.iter().any(|r| crate::index::worktree_id_of(r) == main_id),
        "the main checkout must NOT be watched as a linked worktree: {roots:?}",
    );

    std::fs::remove_dir_all(&main).ok();
}

#[test]
fn gitignore_watch_dirs_includes_worktree_root_for_subdir_config_root() {
    // FINDING 1 (round 3): when `config.root` is a subdirectory of a Git worktree, the watcher
    // must also subscribe to the ancestor chain up to the worktree root so a root-`.gitignore`
    // edit (which lives ABOVE the recursively-watched target dirs) is delivered.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let wt = scratch_root(format!("ragrat-wdirs-{}-{id}", std::process::id()));
    std::fs::create_dir_all(wt.join("crates/app")).unwrap();
    let wt = wt.canonicalize().unwrap();
    rag_rat_base::test_git::run(&wt, &["init", "-q"]);

    let sub = wt.join("crates/app");
    let dirs = gitignore_watch_dirs(&sub);
    // The chain from the worktree root down to config.root, inclusive.
    assert_eq!(
        dirs.first().map(PathBuf::as_path),
        Some(wt.as_path()),
        "worktree root is watched (finding 1)"
    );
    assert!(dirs.contains(&wt.join("crates")), "intermediate ancestor watched");
    assert_eq!(dirs.last(), Some(&sub), "config.root itself is watched");

    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn gitignore_watch_dirs_non_git_tree_is_just_root() {
    // Outside a Git worktree the chain collapses to just `config.root` (already covered by the
    // recursive target watches) — no ancestor sweep above an un-versioned directory.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-wdirs-ng-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    // Best-effort: only meaningful when /tmp isn't itself inside a git worktree. If it is,
    // skip.
    if crate::index::git_history::worktree_root(&root).is_some() {
        std::fs::remove_dir_all(&root).ok();
        return;
    }
    assert_eq!(gitignore_watch_dirs(&root), vec![root.clone()]);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn root_gitignore_edit_is_delivered_to_a_real_watcher() {
    // FINDING 1 end-to-end (test d): a live edit to the worktree-root `.gitignore` — which sits
    // ABOVE the target dirs — must actually be *delivered* by the notify watcher once we
    // subscribe to `gitignore_watch_dirs`. We spawn a real recommended_watcher over exactly the
    // dirs the watcher subscribes to (target dir + ancestor chain) and assert the edit arrives.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let wt = scratch_root(format!("ragrat-deliv-{}-{id}", std::process::id()));
    std::fs::create_dir_all(wt.join("crates")).unwrap();
    let wt = wt.canonicalize().unwrap();
    rag_rat_base::test_git::run(&wt, &["init", "-q"]);
    std::fs::write(wt.join(".gitignore"), "").unwrap();

    let sub = wt.join("crates"); // config.root is the subdirectory.
    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&wt).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    // Subscribe exactly as watcher_main does: the gitignore-pruned target subtree (issue #331)
    // + the ancestor gitignore chain. The root `.gitignore` edit is delivered by the chain
    // watch, not the target subtree, so the pruned placement doesn't weaken this assertion.
    let ignore = IgnoreMatcher::compile(&sub, &[PathBuf::from(".")]);
    watch_tree_pruned(&mut w, &placement_counters(), &sub, &ignore);
    for dir in gitignore_watch_dirs(&sub) {
        let _ = w.watch(&dir, RecursiveMode::NonRecursive);
    }

    // Edit the worktree-root `.gitignore` (above config.root).
    std::fs::write(wt.join(".gitignore"), "secret.rs\n").unwrap();

    // Drain events for up to ~3s; assert at least one references the root `.gitignore`.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut delivered = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(event)) =>
                if event.paths.iter().any(|p| is_gitignore_path(p)) {
                    delivered = true;
                    break;
                },
            Ok(Err(_)) => {},
            Err(RecvTimeoutError::Timeout) => {},
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(w);
    std::fs::remove_dir_all(&wt).ok();
    assert!(delivered, "root .gitignore edit above config.root must be delivered (finding 1)");
}

#[cfg(target_os = "linux")]
#[test]
fn watcher_main_routes_gitignore_mutations_through_central_helpers() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watch-loop-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        rag_rat_base::test_git::run(dir, args);
    };
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "t@e"]);
    git(&root, &["config", "user.name", "t"]);
    std::fs::write(root.join(".gitignore"), "").unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "seed"]);
    let root = root.canonicalize().unwrap();

    let (mut config, root) = whole_root_config(&root, &[PathBuf::from("src")]);
    config.watch.debounce_ms = 20;
    config.watch.max_latency_ms = 50;
    config.watch.periodic_sweep_secs = 0;
    let watcher = Watcher::spawn(config).expect("real watcher should start");
    let db = root.join(".rag-rat/index.sqlite");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !db.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(db.exists(), "startup maintenance pass should create the index");

    std::thread::sleep(Duration::from_millis(100));
    std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(watcher);
    std::fs::remove_dir_all(&root).ok();
}

// Linux/inotify only: this asserts the watch-PLACEMENT optimization (an ignored subtree gets no
// watch, so its edits are never delivered) — the mitigation for inotify `max_user_watches`
// exhaustion that motivated #331/#332. inotify places one NON-recursive watch per directory, so
// a dir that is never watched delivers nothing. The other backends coalesce differently:
// `ReadDirectoryChangesW` (Windows) and FSEvents (macOS) report the ignored DIRECTORY entry's
// mtime bump from a nested write on the parent's watch, so placement can't suppress delivery
// and the outcome is timing-dependent (this test fails on Windows and flakes on macOS).
// That is harmless — the CLASSIFICATION filter (`event_is_relevant`) drops the ignored path
// before any indexing, and THAT guarantee is verified on every OS by
// `event_is_relevant_skips_gitignored_paths_consistently_with_walker`. See #446.
#[cfg(target_os = "linux")]
#[test]
fn gitignored_subdir_under_a_target_is_not_watched() {
    // ISSUE #331: a gitignored directory under a target dir must NOT receive an inotify watch
    // (that's how a recursive watch exhausted `fs.inotify.max_user_watches`). End-to-end: an
    // edit inside the ignored subtree is never delivered, while an edit to a non-ignored
    // sibling is — proving placement, not just classification, honors `.gitignore`.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-331ign-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("ignored_dir/nested")).unwrap();
    std::fs::create_dir_all(root.join("kept_dir/nested")).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "ignored_dir/\n").unwrap();
    // Seed a file in each so the dirs exist before the watch is placed.
    std::fs::write(root.join("ignored_dir/nested/a.rs"), "// a\n").unwrap();
    std::fs::write(root.join("kept_dir/nested/b.rs"), "// b\n").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    // Place watches exactly as watcher_main does: the gitignore-pruned target subtree.
    let ignore = IgnoreMatcher::compile(&root, &[PathBuf::from(".")]);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);
    drain_until_quiet(&rx, 100, 1000);

    // A write inside the gitignored subtree must NOT be delivered (the dir was never watched).
    let ignored_probe = root.join("ignored_dir/nested");
    std::fs::write(ignored_probe.join("a.rs"), "// a edited\n").unwrap();
    let ignored_seen = drain_until_path_under(&rx, &ignored_probe, 2);

    // A write to a non-ignored sibling under the same target MUST be delivered.
    let kept_probe = root.join("kept_dir/nested");
    std::fs::write(kept_probe.join("b.rs"), "// b edited\n").unwrap();
    let kept_seen = drain_until_path_under(&rx, &kept_probe, 3);

    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(!ignored_seen, "an edit inside a gitignored subtree must not be delivered (#331)");
    assert!(kept_seen, "an edit in a non-ignored sibling must still be delivered");
}

#[test]
fn newly_created_non_ignored_dir_gets_watched() {
    // ISSUE #331: target dirs are watched NON-recursively, so a directory created AFTER the
    // watch is placed needs an explicit pruned watch (`watch_created_dirs`), or edits inside it
    // would never fire. End-to-end: create a dir post-spawn, run the create-event handling,
    // then write a file inside it and assert the change is delivered.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-331new-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "ignored_dir/\n").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    let target_dirs = vec![PathBuf::from(".")];
    let (config, root) = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);

    // Create a NEW non-ignored directory after the initial placement.
    let fresh = root.join("fresh_dir");
    std::fs::create_dir_all(&fresh).unwrap();
    // Feed the create event through the same handler watcher_main runs, which places the watch.
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(fresh.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &create,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );

    // A write inside the freshly-watched dir must now be delivered.
    std::fs::write(fresh.join("c.rs"), "// c\n").unwrap();
    let seen = drain_until_path_under(&rx, &fresh, 3);

    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(seen, "an edit in a newly-created non-ignored dir must be delivered (#331)");
}

#[test]
fn a_bare_directory_create_is_not_relevant_so_placement_must_be_unconditional() {
    // ISSUE #332 (P1): a new subdir under a NonRecursive-watched target (#331) needs its own
    // watch via `watch_created_dirs`. But a bare `mkdir src/foo` is NOT a relevant event — a
    // directory is extensionless, so it matches no `**/*.rs` target glob — which is exactly why
    // `watch_created_dirs` must run UNCONDITIONALLY in the loop, NOT gated behind
    // `event_is_relevant`. The original bug gated it, so new dirs were never watched and their
    // files stayed invisible until the periodic sweep.
    use std::sync::atomic::{AtomicU64, Ordering};

    use notify::event::CreateKind;
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-332rel-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let ignore = IgnoreMatcher::compile(&root, &[PathBuf::from("src")]);
    let dir_create =
        Event::new(EventKind::Create(CreateKind::Folder)).add_path(root.join("src/foo"));
    let file_create =
        Event::new(EventKind::Create(CreateKind::File)).add_path(root.join("src/foo/lib.rs"));
    std::fs::remove_dir_all(&root).ok();
    assert!(
        !event_is_relevant(&config, &ignore, &dir_create),
        "a bare directory create must NOT be relevant — so watch_created_dirs must be \
         unconditional (#332 P1)",
    );
    assert!(
        event_is_relevant(&config, &ignore, &file_create),
        "the FILE under it IS relevant — but its event only arrives if src/foo was watched first",
    );
}

#[test]
fn a_directory_moved_into_a_target_is_watched() {
    // ISSUE #332: moving a directory INTO a watched target (`mv /tmp/pkg src/pkg`) is reported
    // as a name Modify (`RenameMode::To`), not a Create — `watch_created_dirs` must handle it
    // too, or edits under the moved dir are missed (the parent is NonRecursive, #331).
    use std::sync::atomic::{AtomicU64, Ordering};

    use notify::event::{ModifyKind, RenameMode};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-332mv-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    let target_dirs = vec![PathBuf::from(".")];
    let (config, root) = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);
    // Simulate `mv` landing a directory into the target: create it, then feed a rename-To
    // event.
    let moved = root.join("moved_pkg");
    std::fs::create_dir_all(&moved).unwrap();
    let rename =
        Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To))).add_path(moved.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &rename,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    std::fs::write(moved.join("d.rs"), "// d\n").unwrap();
    let seen = drain_until_path_under(&rx, &moved, 3);
    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(seen, "an edit in a directory MOVED into a target must be delivered (#332)");
}

#[test]
fn relaxing_an_ignore_rule_re_places_watches_on_the_unignored_subtree() {
    // ISSUE #332: pruned watches are placed at startup against the then-current rules. If a
    // user REMOVES an ignore rule for an existing subtree, re-placing watches (after
    // recompiling the matcher) must add a watch for it — otherwise edits inside it
    // never fire a pass.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-332re-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("formerly_ignored")).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "formerly_ignored/\n").unwrap();
    std::fs::write(root.join("formerly_ignored/e.rs"), "// e\n").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    // Startup placement against the original rules: the dir is ignored → NOT watched.
    let ignore = IgnoreMatcher::compile(&root, &[PathBuf::from(".")]);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);
    // Relax the rule, then recompile + RE-PLACE (what the loop now does on a `.gitignore`
    // edit).
    std::fs::write(root.join(".gitignore"), "").unwrap();
    let ignore = IgnoreMatcher::compile(&root, &[PathBuf::from(".")]);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);
    // An edit in the formerly-ignored (now eligible) subtree must now be delivered.
    std::fs::write(root.join("formerly_ignored/e.rs"), "// e edited\n").unwrap();
    let seen = drain_until_path_under(&rx, &root.join("formerly_ignored"), 3);
    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(
        seen,
        "after relaxing an ignore rule, re-placement must watch the unignored subtree (#332)",
    );
}

#[test]
#[cfg(unix)]
fn a_symlink_to_a_directory_is_not_followed_into_watches() {
    // ISSUE #332 (P2): `watch_created_dirs` must NOT follow a symlink-to-dir. A symlink created
    // (or moved) under a target pointing at a huge tree OUTSIDE config.root (a dep cache,
    // another checkout) would, if followed, make `watch_tree_pruned` recurse through it
    // and place watches outside the indexed root → re-exhaust inotify.
    // `symlink_metadata` reports the link as a link (`is_dir() == false`), so the path
    // is skipped.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-332sym-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();
    // A real directory OUTSIDE the target, with a file in it — the symlink's target.
    let outside = scratch_root(format!("ragrat-332symtgt-{}-{id}", id));
    std::fs::create_dir_all(&outside).unwrap();
    let outside = outside.canonicalize().unwrap();
    std::fs::write(outside.join("f.rs"), "// f\n").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    let target_dirs = vec![PathBuf::from(".")];
    let (config, root) = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);

    // Symlink the outside dir UNDER the target, then feed its create event.
    let link = root.join("linked");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(link.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &create,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );

    // An edit to the file INSIDE the link target must NOT be delivered (the link wasn't
    // watched, and the outside dir is not under config.root at all).
    std::fs::write(outside.join("f.rs"), "// f edited\n").unwrap();
    let followed = drain_until_path_under(&rx, &outside, 2);

    drop(w);
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&outside).ok();
    assert!(!followed, "a symlink-to-dir must not be followed into watches (#332)");
}

#[test]
fn a_non_target_top_level_dir_is_not_watched() {
    // ISSUE #332 (P2): config.root is watched NON-recursively (the gitignore-chain ancestor
    // watches), so a create of a top-level dir OUTSIDE any target (`vendor/`, a sibling of the
    // `src` target) is delivered to the loop too. `watch_created_dirs` must gate on the
    // target relation so it never watches such a dir — it can't be indexed and would just burn
    // inotify watches. A new subdir UNDER the target still gets watched.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-332nt-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    let target_dirs = vec![PathBuf::from("src")];
    let (config, root) = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    // Watch the target subtree + (mirroring watcher_main) config.root itself non-recursively,
    // so a top-level create is delivered here exactly as it would be in production.
    watch_tree_pruned(&mut w, &placement_counters(), &root.join("src"), &ignore);
    let _ = w.watch(&root, RecursiveMode::NonRecursive);

    // A NON-target top-level dir: created + its event fed → must NOT be watched. Probe a file
    // two levels deep (`vendor/sub/v.rs`) — a delivery there could ONLY come from a watch on
    // `vendor` (or below), never from the root's own NON-recursive watch, which sees the
    // top-level `vendor` entry but not its contents. (Probing `vendor/v.rs` would falsely match
    // the root watch's delivery of the direct `vendor` child.)
    let vendor = root.join("vendor");
    let vendor_sub = vendor.join("sub");
    std::fs::create_dir_all(&vendor_sub).unwrap();
    let vendor_ev = Event::new(EventKind::Create(CreateKind::Folder)).add_path(vendor.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &vendor_ev,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    std::fs::write(vendor_sub.join("v.rs"), "// v\n").unwrap();
    let vendor_seen = drain_until_path_under(&rx, &vendor_sub, 2);

    // A new dir UNDER the target: must be watched.
    let pkg = root.join("src/pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    let pkg_ev = Event::new(EventKind::Create(CreateKind::Folder)).add_path(pkg.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &pkg_ev,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    std::fs::write(pkg.join("p.rs"), "// p\n").unwrap();
    let pkg_seen = drain_until_path_under(&rx, &pkg, 3);

    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(!vendor_seen, "a non-target top-level dir must not be watched (#332)");
    assert!(pkg_seen, "a new dir under the target must still be watched");
}

// Linux/inotify only — same rationale as `gitignored_subdir_under_a_target_is_not_watched`:
// this asserts watch PLACEMENT (a nested-ignored moved-in subdir gets no watch). On
// Windows/macOS the nested write bumps the ignored dir entry's mtime and the parent watch
// reports it; classification still drops it. See #446.
#[cfg(target_os = "linux")]
#[test]
fn a_moved_in_dir_with_a_nested_gitignore_prunes_against_it() {
    // ISSUE #332 (P2): a dir MOVED into a target carrying its OWN nested `.gitignore` must be
    // pruned against that nested rule. The long-lived matcher was compiled before the subtree
    // existed, so it doesn't know the nested rule; `watch_created_dirs` recompiles before
    // walking so `watch_tree_pruned` skips the nested-ignored subdir.
    use std::sync::atomic::{AtomicU64, Ordering};

    use notify::event::{ModifyKind, RenameMode};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-332nest-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    let target_dirs = vec![PathBuf::from(".")];
    let (config, root) = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);
    drain_until_quiet(&rx, 100, 1000);

    // Build the moved-in dir with a NESTED `.gitignore` ignoring `ignored_sub/`, plus a kept
    // sibling — all created BEFORE feeding the rename event (so the matcher was stale to them).
    let pkg = root.join("pkg");
    std::fs::create_dir_all(pkg.join("ignored_sub/deep")).unwrap();
    std::fs::create_dir_all(pkg.join("kept_sub/deep")).unwrap();
    std::fs::write(pkg.join(".gitignore"), "ignored_sub/\n").unwrap();
    std::fs::write(pkg.join("ignored_sub/deep/x.rs"), "// x\n").unwrap();
    std::fs::write(pkg.join("kept_sub/deep/y.rs"), "// y\n").unwrap();
    let rename =
        Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To))).add_path(pkg.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &rename,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    drain_until_quiet(&rx, 100, 1000);

    // The nested-ignored subdir must NOT be watched; the kept sibling MUST be.
    let ignored_probe = pkg.join("ignored_sub/deep");
    std::fs::write(ignored_probe.join("x.rs"), "// x edited\n").unwrap();
    let ignored_seen = drain_until_path_under(&rx, &ignored_probe, 2);
    let kept_probe = pkg.join("kept_sub/deep");
    std::fs::write(kept_probe.join("y.rs"), "// y edited\n").unwrap();
    let kept_seen = drain_until_path_under(&rx, &kept_probe, 3);

    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(!ignored_seen, "a moved-in nested-.gitignore-ignored subdir must not be watched");
    assert!(kept_seen, "the kept sibling under the moved-in dir must be watched (#332)");
}

#[test]
fn shutdown_discover_skips_when_write_lock_is_held() {
    let (_scratch, config, _) = src_checkout_config("watch-shutdown-discover");
    let lock_repo = rag_rat_base::locks::write_lock_repo_id(&config);
    let holder_config = config.clone();
    let holder_repo = lock_repo.clone();
    let release = Arc::new(AtomicBool::new(false));
    let release_for_holder = Arc::clone(&release);
    let holder = std::thread::spawn(move || {
        let _held =
            rag_rat_base::locks::WriteLock::acquire_blocking(&holder_config.database, &holder_repo)
                .unwrap();
        while !release_for_holder.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(25));
        }
    });
    std::thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    assert!(
        !shutdown_discover(&config).unwrap(),
        "shutdown discover must skip when another thread holds the write lock"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "shutdown discover should time out waiting for the lock, not block until it is released"
    );
    release.store(true, Ordering::Relaxed);
    holder.join().unwrap();
}
