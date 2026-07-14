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
    let lock_repo = locks::write_lock_repo_id(config);
    let lock_path = locks::papertrail_lock_path(&config.database, &lock_repo);
    let pending = PendingMarker::new(&config.database, &lock_repo);
    let Some(_flight) = FileLock::try_acquire(&lock_path)? else {
        pending.merge(request)?;
        return Ok(AutosyncOutcome::Coalesced);
    };
    let mut request = request;
    let last_report = loop {
        // This run covers everything requested so far: absorb (and clear) the marker BEFORE
        // running, so only triggers arriving mid-flight earn the follow-up evaluation below.
        if let Some(coalesced) = pending.take()? {
            request = request.max(coalesced);
        }
        let report = match run_flight(config, request) {
            Ok(report) => report,
            Err(error) => {
                // Best-effort retry signal: a failing marker write must not shadow the flight
                // error the caller is about to log.
                let _ = pending.merge(request);
                return Err(error);
            },
        };
        if !pending.is_set() {
            break report;
        }
        // The follow-up's strength comes from the marker itself, absorbed at the loop top.
        request = AutosyncRequest::Evaluate;
    };
    Ok(AutosyncOutcome::Ran(Box::new(last_report)))
}

fn run_flight(config: &Config, request: AutosyncRequest) -> anyhow::Result<PapertrailSyncReport> {
    let db = IndexDatabase::open_config(config)?;
    db.papertrail_sync_scheduled(request)
}

/// The pending-marker pair: the marker file carrying the strongest coalesced request, and the
/// short-lived lock serializing every read-modify-write of it. Without the lock two coalescing
/// contenders can interleave their max-merge — one reads the old marker, another writes `full`,
/// the first overwrites it with `incremental` — silently postponing an explicitly queued full
/// walk. Distinct from the flight lock, which the runner holds for the whole network-bound run;
/// this one is held for microseconds per update, so contenders never wait on a flight.
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

    /// Record `request` into the marker, max-merged with whatever is already queued there.
    fn merge(&self, request: AutosyncRequest) -> anyhow::Result<()> {
        let _update = FileLock::acquire_blocking(&self.update_lock)?;
        let merged = match fs::read_to_string(&self.path) {
            Ok(existing) => request.max(AutosyncRequest::from_marker_str(&existing)),
            Err(_) => request,
        };
        fs::write(&self.path, merged.as_marker_str())?;
        Ok(())
    }

    /// Consume the marker. Runs under the same update lock as [`Self::merge`], so a request
    /// merged concurrently is either absorbed into the returned value or lands after the removal
    /// and survives for the next check.
    fn take(&self) -> anyhow::Result<Option<AutosyncRequest>> {
        let _update = FileLock::acquire_blocking(&self.update_lock)?;
        let Ok(content) = fs::read_to_string(&self.path) else {
            return Ok(None);
        };
        let _ = fs::remove_file(&self.path);
        Ok(Some(AutosyncRequest::from_marker_str(&content)))
    }

    fn is_set(&self) -> bool {
        self.path.exists()
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

        marker.merge(AutosyncRequest::Full).unwrap();
        // A later, weaker trigger must not weaken the queued full walk.
        marker.merge(AutosyncRequest::Incremental).unwrap();
        assert_eq!(fs::read_to_string(&marker.path).unwrap(), "full");

        assert_eq!(marker.take().unwrap(), Some(AutosyncRequest::Full));
        assert!(!marker.is_set());
        assert_eq!(marker.take().unwrap(), None);

        // And the upgrade direction: a stronger trigger replaces a weaker queued one.
        marker.merge(AutosyncRequest::Evaluate).unwrap();
        marker.merge(AutosyncRequest::Incremental).unwrap();
        assert_eq!(marker.take().unwrap(), Some(AutosyncRequest::Incremental));
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
                        marker.merge(request).unwrap();
                    }
                });
            }
        });
        let marker = PendingMarker::new(&database, "repo");
        assert_eq!(marker.take().unwrap(), Some(AutosyncRequest::Full));
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
