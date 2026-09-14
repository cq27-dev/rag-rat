use std::fs;
use std::path::Path;

use super::*;

#[test]
fn request_strength_orders_evaluate_below_incremental_below_full() {
    assert!(AutosyncRequest::Evaluate < AutosyncRequest::Incremental);
    assert!(AutosyncRequest::Incremental < AutosyncRequest::Full);
}

#[test]
fn marker_tokens_round_trip_and_unknown_content_degrades_to_evaluate() {
    for request in [AutosyncRequest::Evaluate, AutosyncRequest::Incremental, AutosyncRequest::Full]
    {
        assert_eq!(AutosyncRequest::from_marker_str(request.as_marker_str()), request);
    }
    assert_eq!(AutosyncRequest::from_marker_str(" full\n"), AutosyncRequest::Full);
    assert_eq!(AutosyncRequest::from_marker_str("garbage"), AutosyncRequest::Evaluate);
    assert_eq!(AutosyncRequest::from_marker_str(""), AutosyncRequest::Evaluate);
}

/// The `AutosyncRequest` payload's max-wins merge holds under cross-"process" filesystem
/// contention. Threads share no state but the marker files (the exact shape of concurrent hook
/// invocations): one queues a FULL walk while weaker contenders storm the marker. The generic's
/// marker lock makes every read-modify-write atomic, so the surviving marker must be `full` no
/// matter the interleaving. (The generic's own tests cover the coalescing mechanism; this pins
/// the domain payload's merge direction end-to-end.)
#[test]
fn concurrent_marker_merges_never_lose_the_strongest_request() {
    let tmp = tempfile::TempDir::new().unwrap();
    let database = tmp.path().join("locks/index.sqlite");
    let single_flight = || {
        SingleFlight::<AutosyncRequest>::for_flight(
            locks::FlightKind::Papertrail,
            &database,
            "repo",
        )
    };
    std::thread::scope(|scope| {
        for contender in 0..8 {
            let sf = single_flight();
            scope.spawn(move || {
                let request = if contender == 3 {
                    AutosyncRequest::Full
                } else {
                    AutosyncRequest::Incremental
                };
                for _ in 0..50 {
                    sf.queue(request).unwrap();
                }
            });
        }
    });
    assert_eq!(single_flight().take().unwrap(), Some(AutosyncRequest::Full));
}

/// The exit handoff under contention: any trigger accepted as `Coalesced` must eventually be
/// covered by a runner — after every concurrent `run` returns, no request may be left
/// orphaned in the marker. Races many triggers against fast flights (the binding's endpoint
/// is unreachable, so each flight fails fast and persists health; after the first attempt
/// the minimum interval makes evaluations near-instant).
#[test]
fn racing_triggers_never_orphan_a_coalesced_request() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = test_config(tmp.path());
    config.trackers = vec![rag_rat_base::config::TrackerConfig {
        provider: rag_rat_base::config::Tracker::Github,
        project: Some("o/r".to_string()),
        remote: "origin".to_string(),
        // The discard port: connection refused, so flights fail fast without network.
        base_url: Some("http://127.0.0.1:9".to_string()),
        auth: None,
        tags: Vec::new(),
    }];
    config.allow_empty = true;
    IndexDatabase::rebuild(&config).unwrap();

    let mut ran = 0;
    let mut coalesced = 0;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|contender| {
                let config = &config;
                scope.spawn(move || {
                    let request = if contender % 2 == 0 {
                        AutosyncRequest::Incremental
                    } else {
                        AutosyncRequest::Full
                    };
                    run(config, request).unwrap()
                })
            })
            .collect();
        for handle in handles {
            match handle.join().unwrap() {
                AutosyncOutcome::Ran(_) => ran += 1,
                AutosyncOutcome::Coalesced => coalesced += 1,
                AutosyncOutcome::Disabled => panic!("bindings are configured"),
                AutosyncOutcome::NotIndexed => panic!("the index was built above"),
            }
        }
    });
    assert!(ran >= 1, "at least one trigger must have run the flight");
    let lock_repo = locks::write_lock_repo_id(&config);
    let pending = locks::FlightKind::Papertrail.pending_path(&config.database, &lock_repo);
    assert!(
        !pending.exists(),
        "an accepted trigger was orphaned in the marker ({ran} ran, {coalesced} coalesced)"
    );
}

#[test]
fn trigger_without_tracker_bindings_is_disabled_before_any_lock_or_database_open() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let outcome = run(&config, AutosyncRequest::Full).unwrap();
    assert!(matches!(outcome, AutosyncOutcome::Disabled));
    assert!(!config.database.exists(), "a disabled trigger must not create the database");
    let lock_repo = locks::write_lock_repo_id(&config);
    assert!(!locks::FlightKind::Papertrail.pending_path(&config.database, &lock_repo).exists());
}

#[test]
fn concurrent_triggers_coalesce_into_the_held_flight() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = test_config(tmp.path());
    config.trackers = vec![rag_rat_base::config::TrackerConfig {
        provider: rag_rat_base::config::Tracker::Github,
        project: Some("o/r".to_string()),
        remote: "origin".to_string(),
        base_url: None,
        auth: None,
        tags: Vec::new(),
    }];
    // The store must exist: the non-creating indexed gate defers before the coalesce path
    // otherwise (and no real flight can hold the lock without one).
    config.allow_empty = true;
    IndexDatabase::rebuild(&config).unwrap();

    let lock_repo = locks::write_lock_repo_id(&config);
    let held = FileLock::try_acquire(
        &locks::FlightKind::Papertrail.lock_path(&config.database, &lock_repo),
    )
    .unwrap()
    .unwrap();
    let outcome = run(&config, AutosyncRequest::Incremental).unwrap();
    assert!(matches!(outcome, AutosyncOutcome::Coalesced));
    let pending = locks::FlightKind::Papertrail.pending_path(&config.database, &lock_repo);
    assert_eq!(fs::read_to_string(&pending).unwrap(), "incremental");
    // A second, stronger trigger upgrades the queued request in place.
    let outcome = run(&config, AutosyncRequest::Full).unwrap();
    assert!(matches!(outcome, AutosyncOutcome::Coalesced));
    assert_eq!(fs::read_to_string(&pending).unwrap(), "full");
    drop(held);
}

#[test]
fn flight_runs_the_scheduled_mirror_and_persists_binding_health_end_to_end() {
    use rag_rat_papertrail::transport::stub::{StubResponse, spawn_script_stub};
    let script = vec![
        StubResponse::ok(
            r#"{"incomplete_results":false,"items":[{"number":1,"html_url":"https://example.test/o/r/issues/1","state":"open","title":"one","body":"","updated_at":"2026-01-01T00:00:00Z","labels":[]}]}"#,
        ),
        StubResponse::ok("[]"),
        StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
        StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
        StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
        StubResponse::ok("[]"),
        StubResponse::ok("[]"),
    ];
    let (url, _stub) = spawn_script_stub(script);
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = test_config(tmp.path());
    config.trackers = vec![rag_rat_base::config::TrackerConfig {
        provider: rag_rat_base::config::Tracker::Github,
        project: Some("o/r".to_string()),
        remote: "origin".to_string(),
        base_url: Some(url),
        auth: None,
        tags: Vec::new(),
    }];

    // Auto-sync runs against an EXISTING index (hooks and the watcher only exist for
    // indexed repos); the flight refuses to create one.
    config.allow_empty = true;
    IndexDatabase::rebuild(&config).unwrap();

    // A fresh binding is due for its first full walk; the flight opens the database itself,
    // runs the policy-gated mirror, and reports the run.
    let outcome = run(&config, AutosyncRequest::Incremental).unwrap();
    let AutosyncOutcome::Ran(report) = outcome else {
        panic!("expected a completed flight, got {outcome:?}");
    };
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.bindings.len(), 1);
    assert!(report.bindings[0].completed_full_walk);

    // Binding health survives in the on-disk database for the next trigger's evaluation.
    let conn = rusqlite::Connection::open(&config.database).unwrap();
    let full_walk_ms: Option<i64> = conn
        .query_row(
            "SELECT last_full_sync_ms FROM papertrail_sync_cursor
                 WHERE tracker='github' AND project='o/r'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(full_walk_ms.is_some());
    let lock_repo = locks::write_lock_repo_id(&config);
    assert!(!locks::FlightKind::Papertrail.pending_path(&config.database, &lock_repo).exists());
}

/// The non-creating gate: a trigger firing before `rag-rat index` ever created the store
/// must defer WITHOUT leaving an empty database file behind — opening a missing database
/// creates the file before the schema check refuses, and that artifact defeats every later
/// `database.exists()` "build the index first" hint. The manual command refuses the same
/// way.
#[test]
fn triggers_before_any_store_exists_defer_without_creating_the_database() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = test_config(tmp.path());
    config.trackers = vec![rag_rat_base::config::TrackerConfig {
        provider: rag_rat_base::config::Tracker::Github,
        project: Some("o/r".to_string()),
        remote: "origin".to_string(),
        base_url: Some("http://127.0.0.1:9".to_string()),
        auth: None,
        tags: Vec::new(),
    }];

    let outcome = run(&config, AutosyncRequest::Incremental).unwrap();
    assert!(matches!(outcome, AutosyncOutcome::NotIndexed), "{outcome:?}");
    assert!(!config.database.exists(), "a deferred trigger must not create the database");
    // The signal is queued even though no store exists yet: a first index pass racing
    // this trigger must not lose it.
    let lock_repo = locks::write_lock_repo_id(&config);
    let pending = locks::FlightKind::Papertrail.pending_path(&config.database, &lock_repo);
    assert_eq!(fs::read_to_string(&pending).unwrap(), "incremental");

    let error = run_manual(&config, true, || {}).unwrap_err().to_string();
    assert!(error.contains("no index at this path yet"), "{error}");
    assert!(!config.database.exists(), "a refused manual sync must not create the database");
}

/// A shared database can hold a repo that was REGISTERED (read-only opens do that) but
/// never indexed; automatic sync must defer until the first index pass instead of starting
/// a mirror for it.
#[test]
fn flight_defers_until_the_repo_is_indexed() {
    let indexed_tmp = tempfile::TempDir::new().unwrap();
    let mut indexed_config = test_config(indexed_tmp.path());
    indexed_config.allow_empty = true;
    IndexDatabase::rebuild(&indexed_config).unwrap();

    // A second repo (a real git repo, so it resolves its OWN identity instead of falling
    // back to the sole registered one) sharing the same database, with a binding but no
    // index pass ever run.
    let unindexed_root = temp_git_repo("autosync-unindexed");
    let mut config = test_config(&unindexed_root);
    config.database = indexed_config.database.clone();
    config.trackers = vec![rag_rat_base::config::TrackerConfig {
        provider: rag_rat_base::config::Tracker::Github,
        project: Some("o/r".to_string()),
        remote: "origin".to_string(),
        base_url: Some("http://127.0.0.1:9".to_string()),
        auth: None,
        tags: Vec::new(),
    }];

    let outcome = run(&config, AutosyncRequest::Full).unwrap();
    assert!(matches!(outcome, AutosyncOutcome::NotIndexed), "{outcome:?}");
    // No mirror work happened and no follow-up signal is owed. (Counts are scoped to the
    // binding under test — the schema bootstrap seeds a poison-sibling row into every
    // repo-scoped table.)
    let conn = rusqlite::Connection::open(&config.database).unwrap();
    let cursor_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM papertrail_sync_cursor WHERE project='o/r'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(cursor_rows, 0);
    // The accepted signal is queued for the first post-index trigger, at full strength.
    let lock_repo = locks::write_lock_repo_id(&config);
    let pending = locks::FlightKind::Papertrail.pending_path(&config.database, &lock_repo);
    assert_eq!(fs::read_to_string(&pending).unwrap(), "full");

    // The first index pass unlocks automatic sync (the flight then runs, absorbing the
    // queued signal, and persists the unreachable binding's failure as health).
    config.allow_empty = true;
    IndexDatabase::rebuild(&config).unwrap();
    let outcome = run(&config, AutosyncRequest::Incremental).unwrap();
    assert!(matches!(outcome, AutosyncOutcome::Ran(_)), "{outcome:?}");
    assert!(!pending.exists(), "the queued signal was absorbed");
    let cursor_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM papertrail_sync_cursor WHERE project='o/r'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(cursor_rows, 1);
}

/// An explicit sync never degrades into a policy-gated follow-up: it announces the wait,
/// blocks until the running flight releases the lock, then runs the full manual pass.
#[test]
fn manual_sync_waits_out_a_running_flight_instead_of_degrading() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = test_config(tmp.path());
    config.allow_empty = true;
    IndexDatabase::rebuild(&config).unwrap();
    let lock_repo = locks::write_lock_repo_id(&config);
    let lock_path = locks::FlightKind::Papertrail.lock_path(&config.database, &lock_repo);
    let held = FileLock::try_acquire(&lock_path).unwrap().unwrap();

    let waited = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let holder = scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(held);
        });
        // No bindings: the manual pass itself is a cheap local report — the point is the
        // lock choreography, not the mirror.
        let report = run_manual(&config, false, || {
            waited.store(true, std::sync::atomic::Ordering::Relaxed);
        })
        .unwrap();
        assert!(report.bindings.is_empty());
        holder.join().unwrap();
    });
    assert!(waited.load(std::sync::atomic::Ordering::Relaxed), "the wait is announced");
    // No degraded follow-up was queued anywhere.
    assert!(!locks::FlightKind::Papertrail.pending_path(&config.database, &lock_repo).exists());
}

/// Manual sync holds the shared flight lock for its whole pass, then drains any follow-up
/// requests that coalesced behind it before releasing.
#[test]
fn manual_sync_runs_under_the_flight_lock_and_drains_queued_followups() {
    use rag_rat_papertrail::transport::stub::{StubResponse, spawn_script_stub};
    let script = vec![
        StubResponse::ok(
            r#"{"incomplete_results":false,"items":[{"number":1,"html_url":"https://example.test/o/r/issues/1","state":"open","title":"one","body":"","updated_at":"2026-01-01T00:00:00Z","labels":[]}]}"#,
        ),
        StubResponse::ok("[]"),
        StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
        StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
        StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
        StubResponse::ok("[]"),
        StubResponse::ok("[]"),
    ];
    let (url, _stub) = spawn_script_stub(script);
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = test_config(tmp.path());
    config.trackers = vec![rag_rat_base::config::TrackerConfig {
        provider: rag_rat_base::config::Tracker::Github,
        project: Some("o/r".to_string()),
        remote: "origin".to_string(),
        base_url: Some(url),
        auth: None,
        tags: Vec::new(),
    }];
    config.allow_empty = true;
    IndexDatabase::rebuild(&config).unwrap();

    // A request queued before the manual pass rides its drain: the manual walk covers it
    // (the follow-up evaluation lands inside the attempt interval and settles to a skip).
    let lock_repo = locks::write_lock_repo_id(&config);
    flight(&config, &lock_repo).queue(AutosyncRequest::Incremental).unwrap();

    let report = run_manual(&config, false, || panic!("the lock is free; no wait")).unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.bindings.len(), 1);
    assert!(report.bindings[0].completed_full_walk);

    // Everything queued was drained and the flight lock released.
    assert!(!locks::FlightKind::Papertrail.pending_path(&config.database, &lock_repo).exists());
    assert!(
        FileLock::try_acquire(
            &locks::FlightKind::Papertrail.lock_path(&config.database, &lock_repo)
        )
        .unwrap()
        .is_some()
    );
}

/// A runner whose flight lock was keyed from a since-upgraded identity must step aside
/// BEFORE any mirror work — on the first pass too, not only marker-driven follow-ups — so
/// it can never overlap a fresh-keyed flight over the same cursor.
#[test]
fn stale_keyed_runner_steps_aside_before_any_mirror_work() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = test_config(tmp.path());
    config.trackers = vec![rag_rat_base::config::TrackerConfig {
        provider: rag_rat_base::config::Tracker::Github,
        project: Some("o/r".to_string()),
        remote: "origin".to_string(),
        base_url: Some("http://127.0.0.1:9".to_string()),
        auth: None,
        tags: Vec::new(),
    }];
    config.allow_empty = true;
    IndexDatabase::rebuild(&config).unwrap();

    // Simulate the post-transition world: the runner still holds a flight lock keyed from
    // the OLD identity, while the config now resolves to a different id.
    let stale_repo = "stale-pre-upgrade-id";
    assert_ne!(locks::write_lock_repo_id(&config), stale_repo);
    let sf = flight(&config, stale_repo);
    let stale_lock_path = sf.flight_lock_path().to_path_buf();
    let flight_lock = FileLock::try_acquire(&stale_lock_path).unwrap().unwrap();
    sf.queue(AutosyncRequest::Full).unwrap();

    let drained = sf
        .drain(flight_lock, Some(AutosyncRequest::Incremental), |queued| {
            run_pass(&config, stale_repo, *queued)
        })
        .unwrap();
    // The stranded request carries the strongest of the trigger and the old-key marker, so
    // the caller's re-key retry loses nothing.
    assert!(
        matches!(drained, FlightOutcome::Stopped(Some(AutosyncRequest::Full))),
        "no pass may run under the stale key"
    );
    // No mirror work happened, the stranded old-key marker was consumed, and the stale
    // flight lock was released.
    let conn = rusqlite::Connection::open(&config.database).unwrap();
    let cursor_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM papertrail_sync_cursor WHERE project='o/r'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(cursor_rows, 0);
    assert!(!locks::FlightKind::Papertrail.pending_path(&config.database, stale_repo).exists());
    assert!(FileLock::try_acquire(&stale_lock_path).unwrap().is_some());
}

fn temp_git_repo(tag: &str) -> rag_rat_base::test_scratch::ScratchDir {
    let root = rag_rat_base::test_scratch::ScratchDir::new(tag);
    let git = |args: &[&str]| {
        rag_rat_base::test_git::run(&root, args);
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.email=t@example.invalid",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "root",
    ]);
    root
}

fn test_config(root: &Path) -> Config {
    let config_root = rag_rat_base::test_scratch::canonical_config_root(root.to_path_buf());
    Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join("db/index.sqlite"),
        root: config_root,
        targets: Vec::new(),
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    }
}
