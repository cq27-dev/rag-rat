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

use std::fs;
use std::path::{Path, PathBuf};

use super::{AutosyncRequest, PapertrailContext, PapertrailSyncReport};
use crate::config::Config;
use crate::index::IndexDatabase;
use crate::locks::{self, FileLock};

/// What one auto-sync trigger produced.
#[derive(Debug)]
pub enum AutosyncOutcome {
    /// The repo resolves no tracker bindings; there is nothing to sync.
    Disabled,
    /// The repo has never been indexed. Automatic sync serves already-indexed repos only — a
    /// deferred first-time-empty hook, or a shared database where this repo was only ever
    /// registered read-only, must not start mirroring. The first index pass unlocks it.
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
    // The non-creating half of the indexed-only gate, BEFORE any lock or open: opening a
    // missing database creates the (empty) file first and only then refuses on the missing
    // schema, and that artifact defeats every later `database.exists()` "build the index first"
    // hint. A repo with no store at all is trivially not indexed.
    if !config.database.exists() {
        return Ok(AutosyncOutcome::NotIndexed);
    }
    // Identity re-key retry: when a pass discovers its flight lock was keyed from a
    // since-upgraded identity, the stranded request (max-merged with anything consumed from the
    // old-key marker) is carried to fresh keys and re-run — an `Incremental` change signal is
    // not re-derivable from persisted cursor state, so it must not be dropped. Transitions are
    // one-time per repo; the cap only bounds pathological churn.
    let mut request = request;
    for _ in 0..3 {
        let lock_repo = locks::write_lock_repo_id(config);
        let lock_path = locks::papertrail_lock_path(&config.database, &lock_repo);
        let pending = PendingMarker::new(&config.database, &lock_repo);
        let Some(flight) = acquire_or_coalesce(&lock_path, &pending, request)? else {
            return Ok(AutosyncOutcome::Coalesced);
        };
        match drain(config, &lock_repo, &pending, flight, Some(request))? {
            Drained::NotIndexed => return Ok(AutosyncOutcome::NotIndexed),
            Drained::Ran(Some(report)) => return Ok(AutosyncOutcome::Ran(report)),
            // Defensive: unreachable with `initial = Some` (the first pass reports, defers,
            // re-keys, or errors before the drain can exit empty-handed).
            Drained::Ran(None) => return Ok(AutosyncOutcome::Coalesced),
            Drained::Rekeyed(stranded) => request = stranded,
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

/// Best-effort: queue a request stranded by an identity re-key into the CURRENT identity's
/// pending marker, where fresh-keyed triggers absorb it. The marker sits until the next trigger
/// (the watcher deadline bounds the wait) — the same bounded staleness every error-path marker
/// accepts.
fn preserve_for_fresh_key(config: &Config, request: AutosyncRequest) {
    let lock_repo = locks::write_lock_repo_id(config);
    let pending = PendingMarker::new(&config.database, &lock_repo);
    if let Ok(guard) = pending.lock() {
        let _ = guard.merge(request);
    }
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
    // The same identity re-key guard as `run_flight`, with manual semantics: the identity can
    // move between resolving the lock key and the post-open re-check (the wait for the flight
    // slot and `open_config`'s own git reads take real time, and the open can itself upgrade
    // the identity). An automatic runner steps aside; an explicit command instead RE-KEYS and
    // retries under the fresh identity so the user still gets their unconditional pass. A
    // transition is one-time per repo — the cap only bounds pathological churn.
    for _ in 0..3 {
        let lock_repo = locks::write_lock_repo_id(config);
        let lock_path = locks::papertrail_lock_path(&config.database, &lock_repo);
        let pending = PendingMarker::new(&config.database, &lock_repo);
        // No marker lock is held while blocking here (a contender holding the marker lock must
        // never block on the flight lock — see [`MarkerGuard`]); the runner's exit handoff
        // releases the flight lock only with an empty marker, so this wait ends at a clean
        // boundary.
        let flight = match FileLock::try_acquire(&lock_path)? {
            Some(flight) => flight,
            None => {
                on_wait();
                FileLock::acquire_blocking(&lock_path)?
            },
        };
        let db = IndexDatabase::open_config(config)?;
        if locks::write_lock_repo_id(config) != lock_repo {
            // Never mirror under a stale-keyed flight lock: a trigger resolving the upgraded
            // identity keys a DIFFERENT lock and could run concurrently over the same cursor.
            // A follow-up that coalesced into the OLD-key marker while this runner held the
            // stale lock is unreachable for fresh-keyed triggers — carry it forward (the
            // automatic path's `Rekeyed` carry, manual flavor) before releasing.
            drop(db);
            if let Some(stranded) = pending.lock()?.take()? {
                preserve_for_fresh_key(config, stranded);
            }
            drop(flight);
            continue;
        }
        let manual_report = db.papertrail_sync(full)?;
        drop(db);
        // Triggers that coalesced behind the manual pass still get their follow-up (and the
        // exit handoff) before the flight lock releases. A follow-up stranded by an identity
        // transition mid-drain is queued for fresh-keyed triggers instead of dropped.
        if let Drained::Rekeyed(stranded) = drain(config, &lock_repo, &pending, flight, None)? {
            preserve_for_fresh_key(config, stranded);
        }
        return Ok(manual_report);
    }
    anyhow::bail!(
        "the repo identity kept changing while acquiring the papertrail flight lock; re-run \
         `rag-rat papertrail sync` once the checkout settles"
    )
}

/// Take the flight lock, or coalesce `request` into the pending marker — atomically under the
/// marker lock, so a request only ever lands in the marker while some runner is still obligated
/// to check it (one half of the exit handoff in [`drain`]).
fn acquire_or_coalesce(
    lock_path: &Path,
    pending: &PendingMarker,
    request: AutosyncRequest,
) -> anyhow::Result<Option<FileLock>> {
    let guard = pending.lock()?;
    match FileLock::try_acquire(lock_path)? {
        Some(flight) => Ok(Some(flight)),
        None => {
            guard.merge(request)?;
            Ok(None)
        },
    }
}

enum Drained {
    Ran(Option<Box<PapertrailSyncReport>>),
    NotIndexed,
    /// The flight lock's key no longer matches the resolved identity; the stranded request
    /// (max-merged with anything consumed from the old-key marker) must be carried to fresh
    /// keys by the caller.
    Rekeyed(AutosyncRequest),
}

/// Drive scheduled passes while holding the flight lock: `initial` first (when given), then any
/// follow-ups coalesced into the marker mid-pass, until the EXIT HANDOFF — absorbing the marker
/// and releasing the flight lock under ONE marker-lock hold — finds nothing queued. A
/// contender's merge-and-try-acquire section ([`acquire_or_coalesce`]) is serialized either
/// before that hold (its request is seen here and earns the follow-up) or after the flight lock
/// is already free (its own try-acquire wins and IT becomes the runner); checking without the
/// lock, or releasing the flight lock outside it, reopens the window where a coalesced request
/// is written into the void and never runs.
fn drain(
    config: &Config,
    lock_repo: &str,
    pending: &PendingMarker,
    flight: FileLock,
    initial: Option<AutosyncRequest>,
) -> anyhow::Result<Drained> {
    let mut flight = Some(flight);
    let mut next = initial;
    let mut last_report = None;
    loop {
        {
            let guard = pending.lock()?;
            match (guard.take()?, next.take()) {
                (Some(queued), current) =>
                    next = Some(current.map_or(queued, |request| request.max(queued))),
                (None, Some(current)) => next = Some(current),
                (None, None) => {
                    // The exit handoff: release under the same hold that observed the empty
                    // marker.
                    drop(flight.take());
                    return Ok(Drained::Ran(last_report));
                },
            }
        }
        let request = next.take().expect("the loop is only entered with a request");
        match run_flight(config, lock_repo, request) {
            Ok(FlightPass::Report(report)) => last_report = Some(report),
            Ok(FlightPass::NotIndexed) => {
                // Nothing can run until the first index pass, and any queued follow-up is
                // equally unservable: consume it under the exit hold and stop. Post-index
                // triggers start synchronization.
                let guard = pending.lock()?;
                let _ = guard.take()?;
                drop(flight.take());
                return Ok(Drained::NotIndexed);
            },
            Ok(FlightPass::Rekeyed) => {
                // The identity this open resolved no longer matches the held lock's key (a
                // shallow clone upgrading to its portable id mid-flight). Triggers arriving
                // under the NEW identity key their own flight lock, so mirroring under the
                // stale key would run two concurrent flights over one cursor — stop here. The
                // old-key marker is unreachable for new-key triggers: fold it into the
                // stranded request the caller re-keys and retries with.
                let guard = pending.lock()?;
                let queued = guard.take()?;
                drop(flight.take());
                drop(guard);
                let stranded = queued.map_or(request, |carried| request.max(carried));
                return Ok(Drained::Rekeyed(stranded));
            },
            Err(error) => {
                // Best-effort retry signal: a failing marker write must not shadow the flight
                // error the caller is about to log.
                if let Ok(guard) = pending.lock() {
                    let _ = guard.merge(request);
                }
                return Err(error);
            },
        }
    }
}

enum FlightPass {
    Report(Box<PapertrailSyncReport>),
    NotIndexed,
    Rekeyed,
}

fn run_flight(
    config: &Config,
    lock_repo: &str,
    request: AutosyncRequest,
) -> anyhow::Result<FlightPass> {
    let db = IndexDatabase::open_config(config)?;
    // Identity re-key guard, on EVERY pass (the first included) at the latest pre-walk moment:
    // the flight lock was keyed from the identity resolved at entry, and both the wait for this
    // slot and `open_config`'s own git reads take real time. A fresh resolution here is exactly
    // what a future trigger would key its lock from, so a mismatch means a concurrent
    // fresh-keyed flight is possible and this runner must not start the walk.
    if locks::write_lock_repo_id(config) != lock_repo {
        return Ok(FlightPass::Rekeyed);
    }
    // Automatic sync serves already-INDEXED repos only. `open_config` registers read-only (it
    // never creates an index), so on a shared database this repo can be registered yet never
    // indexed; the #427 "an index pass ran here" signal (a recorded root, or the source_root
    // meta an identity-less root gets) is what separates the two.
    if !crate::index::is_root_already_indexed_conn(db.storage.connection(), config)? {
        return Ok(FlightPass::NotIndexed);
    }
    Ok(FlightPass::Report(Box::new(db.papertrail_sync_scheduled(request)?)))
}

/// The pending-marker pair: the marker file carrying the strongest coalesced request, and the
/// lock serializing every access to it. All reads and writes flow through a held [`MarkerGuard`]
/// so no path can touch the file outside the lock — without it, two coalescing contenders can
/// interleave their max-merge (a weaker request overwrites a queued `full`), and a runner's exit
/// check can miss a marker written just before its flight lock releases. Distinct from the
/// flight lock, which the runner holds for the whole network-bound run; this one is held for
/// microseconds per section, so contenders never wait on a flight.
struct PendingMarker {
    path: PathBuf,
    update_lock: PathBuf,
}

impl PendingMarker {
    fn new(database: &Path, repo_id: &str) -> Self {
        Self {
            path: locks::papertrail_pending_path(database, repo_id),
            update_lock: locks::papertrail_marker_lock_path(database, repo_id),
        }
    }

    fn lock(&self) -> anyhow::Result<MarkerGuard<'_>> {
        Ok(MarkerGuard { marker: self, _update: FileLock::acquire_blocking(&self.update_lock)? })
    }
}

/// One held marker-lock section. Lock ordering: a runner holding the FLIGHT lock may block on
/// this lock; a contender holding this lock only ever TRY-acquires the flight lock — the
/// non-blocking edge is what makes the pair deadlock-free.
struct MarkerGuard<'a> {
    marker: &'a PendingMarker,
    _update: FileLock,
}

impl MarkerGuard<'_> {
    /// Record `request` into the marker, max-merged with whatever is already queued there.
    fn merge(&self, request: AutosyncRequest) -> anyhow::Result<()> {
        let merged = match fs::read_to_string(&self.marker.path) {
            Ok(existing) => request.max(AutosyncRequest::from_marker_str(&existing)),
            Err(_) => request,
        };
        fs::write(&self.marker.path, merged.as_marker_str())?;
        Ok(())
    }

    /// Consume the marker. A request merged concurrently is either absorbed into the returned
    /// value or lands after the removal and survives for the next check.
    fn take(&self) -> anyhow::Result<Option<AutosyncRequest>> {
        let Ok(content) = fs::read_to_string(&self.marker.path) else {
            return Ok(None);
        };
        let _ = fs::remove_file(&self.marker.path);
        Ok(Some(AutosyncRequest::from_marker_str(&content)))
    }

    /// Test-only visibility into the marker's presence; production paths decide through
    /// [`Self::take`] so the decision and the consumption share one hold.
    #[cfg(test)]
    fn is_set(&self) -> bool {
        self.marker.path.exists()
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn pending_marker_merges_max_wins_and_is_consumed_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = PendingMarker::new(&tmp.path().join("locks/index.sqlite"), "repo");

        marker.lock().unwrap().merge(AutosyncRequest::Full).unwrap();
        // A later, weaker trigger must not weaken the queued full walk.
        marker.lock().unwrap().merge(AutosyncRequest::Incremental).unwrap();
        assert_eq!(fs::read_to_string(&marker.path).unwrap(), "full");

        assert_eq!(marker.lock().unwrap().take().unwrap(), Some(AutosyncRequest::Full));
        assert!(!marker.lock().unwrap().is_set());
        assert_eq!(marker.lock().unwrap().take().unwrap(), None);

        // And the upgrade direction: a stronger trigger replaces a weaker queued one.
        marker.lock().unwrap().merge(AutosyncRequest::Evaluate).unwrap();
        marker.lock().unwrap().merge(AutosyncRequest::Incremental).unwrap();
        assert_eq!(marker.lock().unwrap().take().unwrap(), Some(AutosyncRequest::Incremental));
    }

    #[test]
    fn concurrent_marker_merges_never_lose_the_strongest_request() {
        // Race many cross-"process" contenders (threads sharing no state but the filesystem, the
        // exact shape of concurrent hook invocations): one queues a FULL walk while weaker
        // contenders storm the marker. The update lock makes every read-modify-write atomic, so
        // the surviving marker must be `full` no matter the interleaving.
        let tmp = tempfile::TempDir::new().unwrap();
        let database = tmp.path().join("locks/index.sqlite");
        std::thread::scope(|scope| {
            for contender in 0..8 {
                let database = database.clone();
                scope.spawn(move || {
                    let marker = PendingMarker::new(&database, "repo");
                    let request = if contender == 3 {
                        AutosyncRequest::Full
                    } else {
                        AutosyncRequest::Incremental
                    };
                    for _ in 0..50 {
                        marker.lock().unwrap().merge(request).unwrap();
                    }
                });
            }
        });
        let marker = PendingMarker::new(&database, "repo");
        assert_eq!(marker.lock().unwrap().take().unwrap(), Some(AutosyncRequest::Full));
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
        config.trackers = vec![crate::config::TrackerConfig {
            provider: crate::config::Tracker::Github,
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
        config.trackers = vec![crate::config::TrackerConfig {
            provider: crate::config::Tracker::Github,
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
        use super::super::transport::stub::{StubResponse, spawn_script_stub};
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
        config.trackers = vec![crate::config::TrackerConfig {
            provider: crate::config::Tracker::Github,
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
        config.trackers = vec![crate::config::TrackerConfig {
            provider: crate::config::Tracker::Github,
            project: Some("o/r".to_string()),
            remote: "origin".to_string(),
            base_url: Some("http://127.0.0.1:9".to_string()),
            auth: None,
            tags: Vec::new(),
        }];

        let outcome = run(&config, AutosyncRequest::Incremental).unwrap();
        assert!(matches!(outcome, AutosyncOutcome::NotIndexed), "{outcome:?}");
        assert!(!config.database.exists(), "a deferred trigger must not create the database");

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
        config.trackers = vec![crate::config::TrackerConfig {
            provider: crate::config::Tracker::Github,
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
        let lock_repo = locks::write_lock_repo_id(&config);
        assert!(!locks::papertrail_pending_path(&config.database, &lock_repo).exists());

        // The first index pass unlocks automatic sync (the flight then runs and persists the
        // unreachable binding's failure as health).
        config.allow_empty = true;
        IndexDatabase::rebuild(&config).unwrap();
        let outcome = run(&config, AutosyncRequest::Incremental).unwrap();
        assert!(matches!(outcome, AutosyncOutcome::Ran(_)), "{outcome:?}");
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
        use super::super::transport::stub::{StubResponse, spawn_script_stub};
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
        config.trackers = vec![crate::config::TrackerConfig {
            provider: crate::config::Tracker::Github,
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
        PendingMarker::new(&config.database, &lock_repo)
            .lock()
            .unwrap()
            .merge(AutosyncRequest::Incremental)
            .unwrap();

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
        config.trackers = vec![crate::config::TrackerConfig {
            provider: crate::config::Tracker::Github,
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
        let stale_lock_path = locks::papertrail_lock_path(&config.database, stale_repo);
        let flight = FileLock::try_acquire(&stale_lock_path).unwrap().unwrap();
        let pending = PendingMarker::new(&config.database, stale_repo);
        pending.lock().unwrap().merge(AutosyncRequest::Full).unwrap();

        let drained =
            drain(&config, stale_repo, &pending, flight, Some(AutosyncRequest::Incremental))
                .unwrap();
        // The stranded request carries the strongest of the trigger and the old-key marker, so
        // the caller's re-key retry loses nothing.
        assert!(
            matches!(drained, Drained::Rekeyed(AutosyncRequest::Full)),
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

    fn temp_git_repo(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("ragrat-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(&root).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
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
