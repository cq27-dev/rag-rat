use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use crate::IndexDatabase;
use crate::index::ignore_rules::IgnoreMatcher;
use crate::watch::tests::support::*;
use crate::watch::*;

/// End-to-end: the event loop's between-pass DRAIN surfaces a placement failure in `index_status`
/// WHILE the watcher runs, with the periodic sweep disabled and no pass dispatched — the exact case
/// the post-resync flush exists for (#658 review). Records a failure into the loop's counters (as a
/// resync would, after the pass persisted), wakes the loop, and asserts the drain persisted it.
#[test]
fn the_event_loop_drain_persists_a_placement_failure_while_running() {
    let (_scratch, mut config, root) = src_checkout_config("watch-drain-flush");
    config.watch.periodic_sweep_secs = 0; // the exact case the drain must cover.
    IndexDatabase::rebuild(&config).unwrap();

    // A failure recorded into the loop's OWN counters — as the post-pass resync would, after the
    // pass worker already persisted. Nothing is in `repo_meta` yet (no pass wrote it).
    let counters = WatchPlacementCounters::default();
    let ignore_m = IgnoreMatcher::compile(&config.root, &config.target_directories());
    let mut failing = FailingWatcher;
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore_m);
    let failures = counters.counts().1;
    assert!(failures > 0);
    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        0,
        "precondition: no pass has persisted the count yet"
    );

    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel::<PassRequest>();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());
        // Wake the loop so an iteration runs the tail drain, give it a moment, then stop.
        tx.send(LoopMsg::Wake).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        drop(pass_tx);
        let _ = pass_rx;
        handle.join().unwrap();
    });

    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        failures,
        "the between-pass drain surfaces the drop in index_status while running (sweep disabled, \
         no pass)"
    );
}

/// The #506 regression: while a maintenance pass is in flight, the event loop must keep
/// classifying events and must fire the fleet hot-upgrade trigger — the exact window where a
/// `cargo install` used to land unseen. The test plays the pass worker itself and withholds
/// the completion until the end.
#[test]
fn a_pass_in_flight_does_not_starve_events_or_the_fleet_trigger() {
    let (_scratch, mut config, root) = src_checkout_config("watch-pass-in-flight");
    let fleet_bin = root.join("rag-rat-506-test-bin");
    std::fs::write(&fleet_bin, b"binary").unwrap();

    config.watch.debounce_ms = 10;
    config.watch.max_latency_ms = 50;
    config.watch.periodic_sweep_secs = 0;
    // This test pins #506 coalescing, not pacing: with the cooldown DISABLED the coalesced
    // follow-up must dispatch immediately on PassDone — exactly the pre-#823 behavior.
    config.watch.pass_cooldown_secs = 0;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let (fleet_tx, fleet_rx) = std::sync::mpsc::channel();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = move |bin: &Path| {
        let _ = fleet_tx.send(bin.to_path_buf());
    };

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: Some(&fleet_bin),
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let loop_thread = scope.spawn(move || event_loop.run());

        // A relevant BASE edit dispatches a base-only pass to the worker (played by this
        // test) — no linked checkout is implicated, so no overlay is swept (#577).
        tx.send(LoopMsg::Fs(Ok(mutation_event(root.join("src/lib.rs"))))).unwrap();
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance {
                run_gc: false,
                overlay_scope: OverlayScope::Linked(BTreeSet::new())
            }),
        );

        // While that pass is in flight (no PassDone), a new binary landing must still fire
        // the fleet trigger...
        tx.send(LoopMsg::Fs(Ok(mutation_event(fleet_bin.clone())))).unwrap();
        assert_eq!(
            fleet_rx.recv_timeout(Duration::from_secs(5)),
            Ok(fleet_bin.clone()),
            "the fleet trigger must fire during a pass, not after it",
        );
        // ...and a further edit is classified, coalescing into the armed debounce instead of
        // dispatching a concurrent pass.
        tx.send(LoopMsg::Fs(Ok(mutation_event(root.join("src/lib.rs"))))).unwrap();
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_millis(300)),
            Err(RecvTimeoutError::Timeout),
            "no second pass may dispatch while one is in flight",
        );

        // Completing the pass dispatches the coalesced follow-up.
        tx.send(LoopMsg::PassDone { live_oracle_wake_in: None }).unwrap();
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance {
                run_gc: false,
                overlay_scope: OverlayScope::Linked(BTreeSet::new())
            }),
        );

        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        let final_refresh_owed = loop_thread.join().unwrap();
        assert!(!final_refresh_owed, "every observed edit was consumed by a dispatched pass",);
    });
}

/// The #823 regression, end-to-end: events arriving during a pass leave the debounce armed and
/// long-elapsed at `PassDone`, and the loop used to dispatch the coalesced follow-up immediately —
/// back-to-back passes for as long as editing continued. With a cooldown configured, the follow-up
/// must wait it out (counted from pass COMPLETION) and then dispatch.
#[test]
fn events_during_a_pass_do_not_redispatch_until_the_cooldown_elapses() {
    let (_scratch, mut config, root) = src_checkout_config("watch-cooldown");

    config.watch.debounce_ms = 10;
    config.watch.max_latency_ms = 50;
    config.watch.periodic_sweep_secs = 0;
    config.watch.pass_cooldown_secs = 2;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let loop_thread = scope.spawn(move || event_loop.run());

        // A relevant edit dispatches the first pass (this test plays the worker).
        tx.send(LoopMsg::Fs(Ok(mutation_event(root.join("src/lib.rs"))))).unwrap();
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance {
                run_gc: false,
                overlay_scope: OverlayScope::Linked(BTreeSet::new())
            }),
        );

        // More edits land while the pass is in flight — they coalesce into the armed debounce.
        tx.send(LoopMsg::Fs(Ok(mutation_event(root.join("src/lib.rs"))))).unwrap();
        // Let the debounce (10 ms) and even the max-latency cap (50 ms) elapse BEFORE the pass
        // completes — the production shape: the pass runs minutes, so at PassDone the armed
        // debounce is already past due and the loop's own PassDone iteration is where the
        // back-to-back redispatch used to happen.
        std::thread::sleep(Duration::from_millis(100));
        tx.send(LoopMsg::PassDone { live_oracle_wake_in: None }).unwrap();

        // The pre-#823 behavior was an immediate redispatch here. The follow-up must instead
        // wait out the cooldown (2 s; the 700 ms probe leaves ample CI-jitter margin)...
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_millis(700)),
            Err(RecvTimeoutError::Timeout),
            "the coalesced follow-up must not dispatch before the cooldown elapses",
        );
        // ...and then dispatch, carrying the coalesced scope.
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(10)),
            Ok(PassRequest::Maintenance {
                run_gc: false,
                overlay_scope: OverlayScope::Linked(BTreeSet::new())
            }),
            "the follow-up dispatches once the cooldown elapses",
        );

        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        let final_refresh_owed = loop_thread.join().unwrap();
        assert!(!final_refresh_owed, "the held-back edit was consumed by the delayed follow-up");
    });
}

/// #823: the periodic sweep is the missed-event backstop and must never be starved by the
/// cooldown. With an absurdly long cooldown armed by a completed pass, a due sweep still
/// dispatches on time.
#[test]
fn a_due_periodic_sweep_overrides_the_pass_cooldown() {
    let (_scratch, mut config, _) = src_checkout_config("watch-sweep-cooldown");

    // No debounce fires (nothing event-driven); the sweep is due every second while the
    // cooldown would hold event-driven dispatch for an hour.
    config.watch.debounce_ms = 60_000;
    config.watch.max_latency_ms = 60_000;
    config.watch.periodic_sweep_secs = 1;
    config.watch.pass_cooldown_secs = 3_600;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());

        // The first periodic sweep dispatches; completing it arms the hour-long cooldown.
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance { run_gc: false, overlay_scope: OverlayScope::All }),
        );
        tx.send(LoopMsg::PassDone { live_oracle_wake_in: None }).unwrap();

        // The next sweep falls due one second later — deep inside the cooldown — and must
        // dispatch anyway: the backstop bypasses the cooldown.
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance { run_gc: false, overlay_scope: OverlayScope::All }),
            "a due periodic sweep must never be starved by the pass cooldown",
        );

        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        drop(pass_tx);
        let _ = handle.join();
    });
}

#[test]
fn watcher_spawn_is_disabled_when_watch_is_off_or_env_opt_out_is_set() {
    let (_scratch, mut config, root) = src_checkout_config("watch-spawn-optout");
    config.watch.enabled = false;
    assert!(Watcher::spawn(config).is_none(), "disabled watch config must not spawn a thread");

    let (mut enabled, _) = whole_root_config(&root, &[PathBuf::from("src")]);
    enabled.watch.enabled = true;
    // SAFETY: this test is the only one touching RAG_RAT_NO_WATCH in this process.
    unsafe {
        std::env::set_var("RAG_RAT_NO_WATCH", "1");
    }
    assert!(Watcher::spawn(enabled).is_none(), "RAG_RAT_NO_WATCH must suppress the watcher");
    unsafe {
        std::env::remove_var("RAG_RAT_NO_WATCH");
    }
}

#[test]
fn event_loop_ignores_fs_errors_and_exits_on_disconnect() {
    let (_scratch, mut config, _) = src_checkout_config("watch-fs-errors");
    config.watch.debounce_ms = 50;
    config.watch.max_latency_ms = 200;
    config.watch.periodic_sweep_secs = 0;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel::<PassRequest>();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };

    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());
        tx.send(LoopMsg::Fs(Err(notify::Error::generic("disk full")))).unwrap();
        tx.send(LoopMsg::Wake).unwrap();
        drop(tx);
        drop(pass_tx);
        let _ = pass_rx;
        let final_refresh_owed = handle.join().unwrap();
        assert!(!final_refresh_owed, "ignored errors and disconnect should not arm a refresh");
    });
}

#[test]
fn periodic_sweep_dispatches_all_overlay_scope() {
    let (_scratch, mut config, _) = src_checkout_config("watch-sweep-scope");
    config.watch.debounce_ms = 60_000;
    config.watch.max_latency_ms = 60_000;
    config.watch.periodic_sweep_secs = 1;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };

    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());
        let request = pass_rx.recv_timeout(Duration::from_secs(5)).expect("periodic sweep pass");
        assert_eq!(
            request,
            PassRequest::Maintenance { run_gc: false, overlay_scope: OverlayScope::All },
            "the periodic backstop must refresh every overlay"
        );
        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        drop(pass_tx);
        let _ = handle.join();
    });
}

#[test]
fn live_oracle_deadline_dispatches_without_events_or_periodic_sweeps() {
    let (_scratch, mut config, _) = src_checkout_config("watch-oracle-deadline");
    config.watch.periodic_sweep_secs = 0;
    config.watch.pass_cooldown_secs = 3_600;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let _startup = scheduler.dispatch_startup();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };

    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());
        tx.send(LoopMsg::PassDone { live_oracle_wake_in: Some(Duration::from_millis(25)) })
            .unwrap();
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance {
                run_gc: false,
                overlay_scope: OverlayScope::Linked(BTreeSet::new())
            }),
            "backlog retries and idle shutdown must share an independent wake source",
        );

        stop.store(true, Ordering::Relaxed);
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        drop(pass_tx);
        assert!(!handle.join().unwrap());
    });
}

#[test]
fn idle_watcher_enqueues_papertrail_evaluation_without_filesystem_activity() {
    let (_scratch, mut config, _) = src_checkout_config("watch-idle-papertrail");
    config.watch.debounce_ms = 60_000;
    config.watch.max_latency_ms = 60_000;
    config.watch.periodic_sweep_secs = 0;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, _pass_rx) = std::sync::mpsc::channel();
    let (papertrail_tx, papertrail_rx) = std::sync::mpsc::channel();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx,
        scheduler: &mut scheduler,
        papertrail_tx: Some(&papertrail_tx),
        papertrail_interval: Some(Duration::from_millis(50)),
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());
        // No filesystem events at all: the deadline alone must enqueue an evaluation.
        assert_eq!(
            papertrail_rx.recv_timeout(Duration::from_secs(5)),
            Ok(rag_rat_papertrail::AutosyncRequest::Evaluate),
        );
        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        let _ = handle.join();
    });
}

#[test]
fn papertrail_deadline_fires_during_an_in_flight_pass_and_ticks_coalesce() {
    let (_scratch, mut config, root) = src_checkout_config("watch-papertrail-deadline");
    config.watch.debounce_ms = 10;
    config.watch.max_latency_ms = 50;
    config.watch.periodic_sweep_secs = 0;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let pass_tx_for_loop = pass_tx.clone();
    let (papertrail_tx, papertrail_rx) = std::sync::mpsc::channel();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: Some(&papertrail_tx),
        papertrail_interval: Some(Duration::from_millis(100)),
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());

        // Put an ordinary maintenance pass in flight (this test plays the worker and does NOT
        // complete it).
        tx.send(LoopMsg::Fs(Ok(mutation_event(root.join("src/lib.rs"))))).unwrap();
        assert!(pass_rx.recv_timeout(Duration::from_secs(5)).is_ok());

        // The papertrail deadline still fires — an in-flight pass must not postpone it.
        assert_eq!(
            papertrail_rx.recv_timeout(Duration::from_secs(5)),
            Ok(rag_rat_papertrail::AutosyncRequest::Evaluate),
            "the papertrail deadline must fire during an in-flight maintenance pass",
        );

        // Several more deadline ticks elapse while the papertrail flight is in the air (no
        // PapertrailDone): they must coalesce, not queue.
        std::thread::sleep(Duration::from_millis(350));
        assert!(
            papertrail_rx.try_recv().is_err(),
            "ticks during an in-flight papertrail run must coalesce into one follow-up",
        );

        // Completing the flight dispatches exactly the one coalesced follow-up.
        tx.send(LoopMsg::PapertrailDone).unwrap();
        assert_eq!(
            papertrail_rx.recv_timeout(Duration::from_secs(5)),
            Ok(rag_rat_papertrail::AutosyncRequest::Evaluate),
        );

        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        drop(pass_tx);
        let _ = handle.join();
    });
}
