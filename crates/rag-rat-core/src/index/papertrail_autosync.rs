//! Cross-process papertrail auto-sync orchestration: one coalesced mirror flight per repository,
//! shared by every trigger source — the watcher's periodic deadline, git-hook maintenance, and
//! any number of concurrent processes. The flight lock + pending marker follow the maintenance
//! coalescing pattern (#267), with one addition: the marker carries the strongest coalesced
//! [`AutosyncRequest`] so a queued full walk is never weakened by a later incremental trigger.
//!
//! The flight NEVER holds the repository write lock: mirror runs wait on the network (page
//! fetches, rate-governor sleeps) for arbitrarily long, and every database commit inside them is
//! a short synchronous transaction serialized by SQLite itself. Ordinary index maintenance and
//! this flight therefore proceed independently.

use rag_rat_base::config::Config;
use rag_rat_base::locks::{self, FileLock};
use rag_rat_base::single_flight::{FlightOutcome, SingleFlight, Step};
use rag_rat_papertrail::{AutosyncRequest, PapertrailContext, PapertrailSyncReport};

use crate::index::IndexDatabase;

/// What one auto-sync trigger produced.
#[derive(Debug)]
pub enum AutosyncOutcome {
    /// The repo resolves no tracker bindings; there is nothing to sync.
    Disabled,
    /// The repo has never been indexed. Automatic sync serves already-indexed repos only — a
    /// deferred first-time-empty hook, or a shared database where this repo was only ever
    /// registered read-only, must not start mirroring. The request is queued in the pending
    /// marker so the first post-index trigger runs with it.
    NotIndexed,
    /// Another flight holds this repository's lock. The request was merged into the pending
    /// marker; the holder's follow-up pass (or the next periodic deadline) covers it.
    Coalesced,
    /// This process ran the flight, absorbing any coalesced follow-ups; the LAST run's report.
    Ran(Box<PapertrailSyncReport>),
}

/// Run — or coalesce into — the repository's single papertrail flight. Callers treat this as
/// best-effort: per-binding provider failures are persisted as binding health inside the run and
/// never surface here; only a process-level failure (the database cannot be opened, storage is
/// broken) returns `Err`, and it leaves the pending marker set so a later trigger retries.
pub fn run(config: &Config, request: AutosyncRequest) -> anyhow::Result<AutosyncOutcome> {
    // Resolve bindings from the config BEFORE opening the database: with no tracker bindings the
    // trigger is a no-op, and a per-commit git hook must not pay a database open to learn that.
    if PapertrailContext::resolve(config).trackers.is_empty() {
        return Ok(AutosyncOutcome::Disabled);
    }
    // The non-creating half of the indexed-only gate, BEFORE any open: opening a missing
    // database creates the (empty) file first and only then refuses on the missing schema, and
    // that artifact defeats every later `database.exists()` "build the index first" hint. A
    // repo with no store at all is trivially not indexed — but the accepted signal is queued
    // (the first post-index trigger absorbs it) so a first index pass racing this trigger can't
    // lose it.
    if !config.database.exists() {
        let lock_repo = locks::write_lock_repo_id(config);
        flight(config, &lock_repo).queue(request)?;
        return Ok(AutosyncOutcome::NotIndexed);
    }
    // Identity re-key retry: when a pass discovers its flight lock was keyed from a
    // since-upgraded identity, the stranded request (absorbed from the old-key marker) is carried
    // to fresh keys and re-run — an `Incremental` change signal is not re-derivable from persisted
    // cursor state, so it must not be dropped. Transitions are one-time per repo; the cap only
    // bounds pathological churn.
    let mut request = request;
    for _ in 0..3 {
        let lock_repo = locks::write_lock_repo_id(config);
        match flight(config, &lock_repo)
            .run(request, |queued| run_pass(config, &lock_repo, *queued))?
        {
            FlightOutcome::Coalesced => return Ok(AutosyncOutcome::Coalesced),
            FlightOutcome::Ran(Some(report)) => return Ok(AutosyncOutcome::Ran(report)),
            // Defensive: unreachable with a non-`None` initial (the first pass reports, defers,
            // re-keys, or errors before the drain can exit empty-handed).
            FlightOutcome::Ran(None) => return Ok(AutosyncOutcome::Coalesced),
            // `StopRequeue` (NotIndexed) left the signal in the marker for a future trigger.
            FlightOutcome::Stopped(None) => return Ok(AutosyncOutcome::NotIndexed),
            // `StopCarry` (Rekeyed) handed the stranded request (+ absorbed marker) back to re-key.
            FlightOutcome::Stopped(Some(stranded)) => request = stranded,
        }
    }
    // Pathological identity churn: preserve the signal where fresh-keyed triggers will find it,
    // and surface the condition to the caller's log.
    preserve_for_fresh_key(config, request);
    anyhow::bail!(
        "the repo identity kept changing across papertrail flight attempts; the request was \
         queued for the next trigger"
    )
}

/// This repo's papertrail single-flight coordinator, keyed to `lock_repo` (the currently-resolved
/// identity). Rebuilt per attempt: the identity can upgrade mid-flight (a shallow clone resolving
/// its portable id), which re-keys every flight path.
fn flight(config: &Config, lock_repo: &str) -> SingleFlight<AutosyncRequest> {
    SingleFlight::new(
        locks::papertrail_lock_path(&config.database, lock_repo),
        locks::papertrail_pending_path(&config.database, lock_repo),
        locks::papertrail_marker_lock_path(&config.database, lock_repo),
    )
}

/// One scheduled flight pass under the held flight lock, mapped to a single-flight [`Step`]:
/// - the repo's identity key went stale since the lock was taken → `StopCarry` (a fresh-keyed
///   flight is now possible; re-key and retry rather than mirror twice over one cursor);
/// - the repo is not indexed yet → `StopRequeue` (automatic sync serves indexed repos only; leave
///   the accepted signal for the first post-index trigger — the deliberate marker-without-a-runner
///   exception);
/// - otherwise the scheduled pass runs → `Ran`.
fn run_pass(
    config: &Config,
    lock_repo: &str,
    request: AutosyncRequest,
) -> anyhow::Result<Step<Box<PapertrailSyncReport>>> {
    let db = IndexDatabase::open_config(config)?;
    // Identity re-key guard on EVERY pass at the latest pre-walk moment: the flight lock was keyed
    // from the identity resolved at entry, and both the wait for the slot and `open_config`'s own
    // git reads take real time. A fresh resolution here is exactly what a future trigger keys its
    // lock from, so a mismatch means a concurrent fresh-keyed flight is possible.
    if locks::write_lock_repo_id(config) != lock_repo {
        return Ok(Step::StopCarry);
    }
    // `open_config` registers read-only (it never creates an index), so on a shared database this
    // repo can be registered yet never indexed; the #427 "an index pass ran here" signal separates
    // the two.
    if !rag_rat_db::schema::is_root_already_indexed_conn(db.storage.connection(), config)? {
        return Ok(Step::StopRequeue);
    }
    Ok(Step::Ran(Box::new(db.papertrail_sync_scheduled(request)?)))
}

/// Best-effort: queue a request stranded by an identity re-key into the CURRENT identity's
/// pending marker, where fresh-keyed triggers absorb it. The marker sits until the next trigger
/// (the watcher deadline bounds the wait) — the same bounded staleness every error-path marker
/// accepts.
fn preserve_for_fresh_key(config: &Config, request: AutosyncRequest) {
    let lock_repo = locks::write_lock_repo_id(config);
    let _ = flight(config, &lock_repo).queue(request);
}

/// The explicit `papertrail sync` command: unconditional manual semantics (every binding
/// dispatched, reference discovery refreshed, `full` honored), SHARING the per-repo flight lock
/// with automatic sync — two mirror runs over one binding would interleave their cursor
/// load/save cycles and clobber each other's walk state. Unlike automatic triggers, a manual
/// invocation never degrades into a policy-gated follow-up: when an automatic flight is in the
/// air it WAITS for the lock (announcing the wait through `on_wait`, interruptible like any
/// foreground command) and then runs the full manual pass itself.
pub fn run_manual(
    config: &Config,
    full: bool,
    mut on_wait: impl FnMut(),
) -> anyhow::Result<PapertrailSyncReport> {
    // Refuse BEFORE any lock or open: opening a missing database creates the (empty) file
    // first and only then refuses on the missing schema, and that artifact defeats every later
    // `database.exists()` "build the index first" hint.
    anyhow::ensure!(
        config.database.exists(),
        "no index at this path yet; build one with `rag-rat index` or `rag-rat index --full`"
    );
    // The same identity re-key guard as `run_pass`, with manual semantics: the identity can
    // move between resolving the lock key and the post-open re-check (the wait for the flight
    // slot and `open_config`'s own git reads take real time, and the open can itself upgrade
    // the identity). An automatic runner steps aside; an explicit command instead RE-KEYS and
    // retries under the fresh identity so the user still gets their unconditional pass. A
    // transition is one-time per repo — the cap only bounds pathological churn.
    for _ in 0..3 {
        let lock_repo = locks::write_lock_repo_id(config);
        let sf = flight(config, &lock_repo);
        // No marker lock is held while blocking here (a contender holding the marker lock only
        // TRY-acquires the flight lock — see the single-flight lock ordering); the runner's exit
        // handoff releases the flight lock only with an empty marker, so this wait ends at a clean
        // boundary.
        let flight_lock = match FileLock::try_acquire(sf.flight_lock_path())? {
            Some(flight_lock) => flight_lock,
            None => {
                on_wait();
                FileLock::acquire_blocking(sf.flight_lock_path())?
            },
        };
        let db = IndexDatabase::open_config(config)?;
        if locks::write_lock_repo_id(config) != lock_repo {
            // Never mirror under a stale-keyed flight lock: a trigger resolving the upgraded
            // identity keys a DIFFERENT lock and could run concurrently over the same cursor.
            // A follow-up that coalesced into the OLD-key marker while this runner held the
            // stale lock is unreachable for fresh-keyed triggers — carry it forward before
            // releasing.
            drop(db);
            if let Some(stranded) = sf.take()? {
                preserve_for_fresh_key(config, stranded);
            }
            drop(flight_lock);
            continue;
        }
        let manual_report = db.papertrail_sync(full)?;
        drop(db);
        // Triggers that coalesced behind the manual pass still get their (scheduled) follow-up and
        // the exit handoff before the flight lock releases. A follow-up stranded by an identity
        // transition mid-drain is queued for fresh-keyed triggers instead of dropped.
        if let FlightOutcome::Stopped(Some(stranded)) =
            sf.drain(flight_lock, None, |queued| run_pass(config, &lock_repo, *queued))?
        {
            preserve_for_fresh_key(config, stranded);
        }
        return Ok(manual_report);
    }
    anyhow::bail!(
        "the repo identity kept changing while acquiring the papertrail flight lock; re-run \
         `rag-rat papertrail sync` once the checkout settles"
    )
}

#[cfg(test)]
mod tests {
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
        for request in
            [AutosyncRequest::Evaluate, AutosyncRequest::Incremental, AutosyncRequest::Full]
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
            SingleFlight::<AutosyncRequest>::new(
                locks::papertrail_lock_path(&database, "repo"),
                locks::papertrail_pending_path(&database, "repo"),
                locks::papertrail_marker_lock_path(&database, "repo"),
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
        let pending = locks::papertrail_pending_path(&config.database, &lock_repo);
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
        assert!(!locks::papertrail_pending_path(&config.database, &lock_repo).exists());
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
        let held =
            FileLock::try_acquire(&locks::papertrail_lock_path(&config.database, &lock_repo))
                .unwrap()
                .unwrap();
        let outcome = run(&config, AutosyncRequest::Incremental).unwrap();
        assert!(matches!(outcome, AutosyncOutcome::Coalesced));
        let pending = locks::papertrail_pending_path(&config.database, &lock_repo);
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
        assert!(!locks::papertrail_pending_path(&config.database, &lock_repo).exists());
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
        let pending = locks::papertrail_pending_path(&config.database, &lock_repo);
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
            .query_row(
                "SELECT COUNT(*) FROM papertrail_sync_cursor WHERE project='o/r'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursor_rows, 0);
        // The accepted signal is queued for the first post-index trigger, at full strength.
        let lock_repo = locks::write_lock_repo_id(&config);
        let pending = locks::papertrail_pending_path(&config.database, &lock_repo);
        assert_eq!(fs::read_to_string(&pending).unwrap(), "full");

        // The first index pass unlocks automatic sync (the flight then runs, absorbing the
        // queued signal, and persists the unreachable binding's failure as health).
        config.allow_empty = true;
        IndexDatabase::rebuild(&config).unwrap();
        let outcome = run(&config, AutosyncRequest::Incremental).unwrap();
        assert!(matches!(outcome, AutosyncOutcome::Ran(_)), "{outcome:?}");
        assert!(!pending.exists(), "the queued signal was absorbed");
        let cursor_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM papertrail_sync_cursor WHERE project='o/r'",
                [],
                |row| row.get(0),
            )
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
        let lock_path = locks::papertrail_lock_path(&config.database, &lock_repo);
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
        assert!(!locks::papertrail_pending_path(&config.database, &lock_repo).exists());
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
        assert!(!locks::papertrail_pending_path(&config.database, &lock_repo).exists());
        assert!(
            FileLock::try_acquire(&locks::papertrail_lock_path(&config.database, &lock_repo))
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
            .query_row(
                "SELECT COUNT(*) FROM papertrail_sync_cursor WHERE project='o/r'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursor_rows, 0);
        assert!(!locks::papertrail_pending_path(&config.database, stale_repo).exists());
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
        Config {
            trackers: Vec::new(),
            papertrail: Default::default(),
            sync: Default::default(),
            repo_id_override: None,
            database_key_pinned: true,
            root: root.to_path_buf(),
            database: root.join("db/index.sqlite"),
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
}
