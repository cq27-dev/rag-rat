//! The live oracle's maintenance-pass tail (#74 slice 2 / #534): the watcher-owned state that
//! drives [`rag_rat_oracle::live_oracle_pass`] — a resident LSP session per live backend (lazily
//! spawned, idle-shutdown) and the backlog of changed paths a prior pass's request budget didn't
//! reach.
//!
//! Gating (all cheap, in order): `[oracle.live] enabled` (standalone — it does NOT imply or
//! require `[oracle] auto_run`), the backend's language among the checkout's indexed languages,
//! and a non-empty worklist (backlog ∪ this pass's changed paths in that language). A pass with
//! no reliable changed set (a heal/bootstrap leaves `clone_delta_hint = None`) contributes no
//! paths: live's scope is exactly "files the pass reindexed", and a whole-checkout sweep is the
//! BATCH pass's job.
//!
//! Backends are INDEPENDENT: each keeps its own session, backlog, and respawn backoff, so a
//! wedged `rust-analyzer` never stalls TypeScript resolution (and vice versa), and each spends
//! its own request budget. A mixed-language repo therefore runs several resident servers when it
//! has several live-capable languages indexed.
//!
//! Everything here is best-effort: a missing language server, a failed spawn, a warming server,
//! or a dead server mid-pass never fails the maintenance pass — the worklist rides to the next
//! pass via the backlog, and an aborted session is dropped so a later pass respawns with bounded
//! backoff. The tail reports its next backlog-retry or idle-shutdown deadline to the event loop.

use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, Instant};

use rag_rat_base::config::Config;
use rag_rat_base::time::now_ms;
use rag_rat_oracle::{LiveBackend, LiveOracleSession};

use crate::index::IndexDatabase;

const LIVE_ORACLE_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
struct RespawnBackoff {
    failures: u32,
    retry_at: Option<Instant>,
}

impl RespawnBackoff {
    fn ready(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|retry_at| now >= retry_at)
    }

    fn record_failure(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let exponent = self.failures.saturating_sub(1).min(4);
        let delay =
            LIVE_ORACLE_RETRY_INTERVAL.saturating_mul(1 << exponent).min(RESPAWN_BACKOFF_MAX);
        self.retry_at = now.checked_add(delay);
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.retry_at.map(|retry_at| retry_at.saturating_duration_since(now))
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Default)]
struct LiveOracleLifecycle {
    session_last_used_at: Option<Instant>,
    respawn_backoff: RespawnBackoff,
}

impl LiveOracleLifecycle {
    fn next_wake_in(&self, has_backlog: bool, idle: Duration, now: Instant) -> Option<Duration> {
        if has_backlog {
            return self
                .respawn_backoff
                .remaining(now)
                .filter(|remaining| !remaining.is_zero())
                .or(Some(LIVE_ORACLE_RETRY_INTERVAL));
        }
        self.session_last_used_at
            .map(|last_used_at| scheduled_idle_wake_in(last_used_at, now, idle))
    }

    fn can_respawn(&self, now: Instant) -> bool {
        self.respawn_backoff.ready(now)
    }

    fn on_spawned(&mut self, now: Instant) {
        self.session_last_used_at = Some(now);
    }

    fn on_session_used(&mut self, now: Instant) {
        debug_assert!(self.session_last_used_at.is_some());
        self.session_last_used_at = Some(now);
    }

    fn idle_shutdown_due(&self, has_pending_work: bool, idle: Duration, now: Instant) -> bool {
        self.session_last_used_at.is_some_and(|last_used_at| {
            should_idle_shutdown(has_pending_work, last_used_at, now, idle)
        })
    }

    fn on_session_ended(&mut self) {
        self.session_last_used_at = None;
    }

    fn on_failure(&mut self, now: Instant) {
        self.on_session_ended();
        self.respawn_backoff.record_failure(now);
    }

    fn on_stable_batch(&mut self) {
        self.respawn_backoff.reset();
    }
}

/// Resident live-oracle state for one watcher: one [`LiveBackendTail`] per live backend. Owned by
/// the pass-worker closure (it must outlive individual passes); the hook/CLI `maintenance_pass*`
/// entry points pass no state, so the live stage only ever runs from the resident watcher — a
/// one-shot CLI pass must not spawn a minutes-warming language server.
pub(crate) struct LiveOracleTail {
    backends: Vec<LiveBackendTail>,
    /// Which backend claims the shared request budget first on the next pass. Rotated every pass
    /// so a language with a constantly-large change set cannot starve the others (see
    /// [`Self::on_pass`]).
    first_claim: usize,
}

impl LiveOracleTail {
    pub(crate) fn new() -> Self {
        Self { backends: LiveBackend::all().map(LiveBackendTail::new).collect(), first_claim: 0 }
    }

    /// Delay until the watcher should run another pass without waiting for a filesystem event —
    /// the EARLIEST any backend needs one, since a single pass services them all.
    pub(crate) fn next_wake_in(&self, config: &Config, now: Instant) -> Option<Duration> {
        if !config.oracle.live.enabled {
            return None;
        }
        let idle = Duration::from_secs(config.oracle.live.idle_shutdown_secs);
        self.backends.iter().filter_map(|backend| backend.next_wake_in(idle, now)).min()
    }

    /// One pass's live stage, for every backend in turn.
    ///
    /// `max_requests_per_pass` bounds the whole MAINTENANCE PASS, not each backend: the pass holds
    /// the repository write lock while it runs, so the cap is a lock-hold guarantee and giving
    /// every backend its own copy would multiply the real bound by the number of live languages.
    /// The backends therefore draw from ONE budget, and the order rotates each pass so a language
    /// with a perpetually-large change set cannot starve the rest (whatever a backend does not
    /// reach stays in its own backlog).
    pub(crate) fn on_pass(
        &mut self,
        db: &IndexDatabase,
        config: &Config,
        changed_paths: Option<&BTreeSet<String>>,
    ) {
        if !config.oracle.live.enabled {
            return;
        }
        let mut budget = config.oracle.live.max_requests_per_pass;
        for index in claim_order(self.backends.len(), self.first_claim) {
            self.backends[index].on_pass(db, config, changed_paths, &mut budget);
        }
        self.first_claim = self.first_claim.wrapping_add(1);
    }
}

/// Backend indices for one pass, starting at `first` and wrapping — the rotation that keeps the
/// shared request budget fair across passes.
fn claim_order(count: usize, first: usize) -> impl Iterator<Item = usize> {
    (0..count).map(move |offset| (first.wrapping_add(offset)) % count.max(1))
}

/// One live backend's resident state: its LSP session, deferred-path backlog, and respawn/idle
/// lifecycle. Independent of every other backend's.
struct LiveBackendTail {
    backend: LiveBackend,
    session: Option<LiveOracleSession>,
    backlog: Vec<String>,
    lifecycle: LiveOracleLifecycle,
    /// Whether this backend's unmet checkout prerequisite has already been reported. The block is
    /// permanent until the checkout changes, so it is worth saying — once, not on every retry.
    prerequisite_reported: bool,
}

impl LiveBackendTail {
    fn new(backend: LiveBackend) -> Self {
        Self {
            backend,
            session: None,
            backlog: Vec::new(),
            lifecycle: LiveOracleLifecycle::default(),
            prerequisite_reported: false,
        }
    }

    /// Delay until this backend needs another pass. A backlog takes priority; otherwise a
    /// resident session schedules its own idle shutdown.
    fn next_wake_in(&self, idle: Duration, now: Instant) -> Option<Duration> {
        self.lifecycle.next_wake_in(!self.backlog.is_empty(), idle, now)
    }

    /// This backend's share of one pass: resolve pending work, or shut an otherwise-workless
    /// session down once idle. Never returns an error — a failure is logged and the work rides
    /// the next pass.
    fn on_pass(
        &mut self,
        db: &IndexDatabase,
        config: &Config,
        changed_paths: Option<&BTreeSet<String>>,
        budget: &mut u64,
    ) {
        let live_cfg = &config.oracle.live;
        let now = Instant::now();
        let started_at_ms = now_ms();

        // The worklist: backlog first (older edits wait longest), then this pass's changed paths
        // in this backend's language, deduped. `changed_paths` is `None` on a heal/bootstrap — no
        // reliable superset, so only the backlog rides.
        let worklist =
            assemble_worklist(std::mem::take(&mut self.backlog), changed_paths, &self.backend);
        let idle_shutdown_due = self.lifecycle.idle_shutdown_due(
            !worklist.is_empty(),
            Duration::from_secs(live_cfg.idle_shutdown_secs),
            now,
        );
        if worklist.is_empty() {
            // Pending work always wins over idle shutdown: a short idle timeout must not kill a
            // warming server immediately before its retained backlog retries.
            if idle_shutdown_due && let Some(session) = self.session.take() {
                tracing::debug!(target: "rag_rat_core::watch", "live oracle: idle shutdown");
                self.lifecycle.on_session_ended();
                session.shutdown();
            }
            return;
        }
        // An earlier backend spent the pass's whole request allowance. Keep this backend's work
        // (a spawn + a zero-budget pass would only defer it again, after paying a language-server
        // warm-up) and let the rotation give it first claim on a later pass.
        if *budget == 0 {
            self.backlog = worklist;
            return;
        }
        // This backend's language must be an indexed language of the checkout.
        if !config.targets.iter().any(|target| target.language == self.backend.language) {
            return;
        }

        // Spawn the session lazily on the first eligible pass. A decline leaves the work in the
        // backlog for a later pass — the same degrade-quietly UX as a missing embedding model.
        if self.session.is_none() {
            if !self.lifecycle.can_respawn(now) {
                self.backlog = worklist;
                return;
            }
            match LiveOracleSession::spawn(self.backend.tool, &config.root) {
                Ok(session) => {
                    self.session = Some(session);
                    self.lifecycle.on_spawned(now);
                },
                // A prerequisite block is PERMANENT until the checkout changes, so retrying it
                // silently would leave an operator with a live oracle that never runs and no
                // reason why. Say it once, then fall through to the ordinary backoff (which is
                // the right cadence for something that cannot fix itself).
                Err(rag_rat_oracle::LiveSpawnBlocked::Prerequisite(hint)) => {
                    if !self.prerequisite_reported {
                        self.prerequisite_reported = true;
                        tracing::warn!(
                            target: "rag_rat_core::watch",
                            tool = self.backend.tool.as_db_str(),
                            "live oracle blocked: {hint}"
                        );
                    }
                },
                Err(rag_rat_oracle::LiveSpawnBlocked::Unavailable) => {},
            }
        }
        let Some(session) = &mut self.session else {
            self.backlog = worklist;
            self.lifecycle.on_failure(Instant::now());
            return;
        };
        // A session exists, so whatever blocked earlier is resolved; report it again if it returns.
        self.prerequisite_reported = false;

        let result = db.run_live_oracle_pass(session, &worklist, *budget, started_at_ms);
        // Count idleness from completion: a request batch longer than the idle window must not
        // force an immediate shutdown and cold respawn.
        self.lifecycle.on_session_used(Instant::now());
        match result {
            Ok(report) => {
                // Charge what was actually spent against the pass-wide allowance, so the
                // backends that run after this one see a real remainder.
                *budget = budget.saturating_sub(report.requests_used);
                self.backlog = report.unfinished_paths.clone();
                // An aborted pass means the server died or wedged mid-resolution: drop the
                // session so the next pass respawns a clean one instead of reusing a broken
                // transport (the aborted files are already requeued in `unfinished_paths`).
                if report.status.starts_with("Aborted:")
                    && let Some(_aborted_session) = self.session.take()
                {
                    // Let the binding hard-kill on Drop; graceful shutdown would attempt another
                    // bounded request against the same wedged transport.
                    tracing::warn!(
                        target: "rag_rat_core::watch",
                        status = %report.status,
                        "live oracle: server aborted; session dropped, respawn after backoff"
                    );
                    self.lifecycle.on_failure(Instant::now());
                } else if report.status != "Warming" {
                    // A completed request batch proves the replacement session is stable enough
                    // to end an earlier crash/spawn-failure streak. Warm-up alone does not.
                    self.lifecycle.on_stable_batch();
                }
                if report.rows_written > 0 || !report.unfinished_paths.is_empty() {
                    tracing::info!(
                        target: "rag_rat_core::watch",
                        rows_written = report.rows_written,
                        upgraded = report.upgraded,
                        confirmed = report.confirmed,
                        contradicted = report.contradicted,
                        requests = report.requests_used,
                        deferred = report.unfinished_paths.len(),
                        refinements_invalidated = report.refinements_invalidated,
                        status = %report.status,
                        "live oracle pass"
                    );
                }
            },
            Err(err) => {
                // A DB-side failure (the only `Err`): drop the session so the next pass
                // respawns fresh, keep the whole worklist riding, and never fail the pass.
                if let Some(session) = self.session.take() {
                    session.shutdown();
                }
                self.backlog = worklist;
                self.lifecycle.on_failure(Instant::now());
                tracing::warn!(
                    target: "rag_rat_core::watch",
                    error = %err,
                    "live oracle pass failed; worklist deferred to the next pass"
                );
            },
        }
    }
}

fn idle_wake_in(last_used_at: Instant, now: Instant, idle: Duration) -> Duration {
    idle.saturating_sub(now.saturating_duration_since(last_used_at))
}

fn scheduled_idle_wake_in(last_used_at: Instant, now: Instant, idle: Duration) -> Duration {
    let remaining = idle_wake_in(last_used_at, now, idle);
    // If a pass failed before reaching the tail, do not redispatch an overdue deadline in a tight
    // loop. A serviced idle wake removes the session and therefore returns no next wake.
    if remaining.is_zero() { LIVE_ORACLE_RETRY_INTERVAL } else { remaining }
}

fn should_idle_shutdown(
    has_pending_work: bool,
    last_used_at: Instant,
    now: Instant,
    idle: Duration,
) -> bool {
    !has_pending_work && now.saturating_duration_since(last_used_at) >= idle
}

/// The pass's worklist for one backend: backlog paths first (oldest edits wait longest), then the
/// changed paths this backend's language claims, deduped, order-preserving. `None` changed paths
/// (a heal/bootstrap pass) contributes nothing — only the backlog rides.
///
/// The language filter is what keeps backends disjoint: a changed `.ts` file must never reach the
/// Rust session, which would open it under the wrong `languageId` and spend budget on a file its
/// server cannot resolve.
fn assemble_worklist(
    backlog: Vec<String>,
    changed_paths: Option<&BTreeSet<String>>,
    backend: &LiveBackend,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut worklist = Vec::new();
    for path in backlog {
        if seen.insert(path.clone()) {
            worklist.push(path);
        }
    }
    if let Some(paths) = changed_paths {
        for path in paths.iter().filter(|path| backend.claims_path(path)) {
            if seen.insert(path.clone()) {
                worklist.push(path.clone());
            }
        }
    }
    worklist
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(tool: rag_rat_oracle::OracleTool) -> LiveBackend {
        LiveBackend::for_tool(tool).expect("a live backend")
    }

    #[test]
    fn worklist_dedupes_backlog_against_changed_and_filters_other_languages() {
        let rust = backend(rag_rat_oracle::OracleTool::RaLsp);
        let backlog = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let changed = BTreeSet::from([
            "src/b.rs".to_string(),
            "src/c.rs".to_string(),
            "Cargo.toml".to_string(),
        ]);
        let worklist = assemble_worklist(backlog, Some(&changed), &rust);
        // Backlog order first, then new changed paths; duplicates collapse; non-Rust dropped.
        assert_eq!(worklist, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    }

    #[test]
    fn worklist_without_changed_set_rides_backlog_only() {
        let rust = backend(rag_rat_oracle::OracleTool::RaLsp);
        let backlog = vec!["src/a.rs".to_string()];
        // A heal/bootstrap pass (None) contributes no paths.
        assert_eq!(assemble_worklist(backlog, None, &rust), vec!["src/a.rs"]);
        assert!(assemble_worklist(Vec::new(), None, &rust).is_empty());
    }

    #[test]
    fn each_backend_claims_only_its_own_languages_changed_paths() {
        // One changed set, several backends: a `.ts` file reaching the Rust session would be
        // opened under the wrong languageId and burn budget on a file that server cannot
        // resolve, and vice versa.
        let changed = BTreeSet::from([
            "src/a.rs".to_string(),
            "src/b.ts".to_string(),
            "src/c.tsx".to_string(),
            "README.md".to_string(),
        ]);
        assert_eq!(
            assemble_worklist(
                Vec::new(),
                Some(&changed),
                &backend(rag_rat_oracle::OracleTool::RaLsp)
            ),
            vec!["src/a.rs"],
        );
        assert_eq!(
            assemble_worklist(
                Vec::new(),
                Some(&changed),
                &backend(rag_rat_oracle::OracleTool::TsLsp)
            ),
            vec!["src/b.ts", "src/c.tsx"],
        );
    }

    #[test]
    fn the_tail_wakes_for_the_earliest_backend_that_needs_one() {
        // A single pass services every backend, so the tail's deadline is the minimum across
        // them — a backend with a backlog must not wait for another backend's longer idle timer.
        let mut tail = LiveOracleTail::new();
        assert!(tail.backends.len() >= 2, "the multi-backend case must actually be exercised");
        let now = Instant::now();
        let idle = Duration::from_secs(600);
        // One backend holds a backlog (retry cadence); another holds an idle session.
        tail.backends[0].backlog.push("src/a.rs".to_string());
        tail.backends[1].lifecycle.on_spawned(now);
        let earliest = tail
            .backends
            .iter()
            .filter_map(|backend| backend.next_wake_in(idle, now))
            .min()
            .expect("at least one backend schedules a wake");
        assert_eq!(earliest, LIVE_ORACLE_RETRY_INTERVAL, "the backlog's retry wins over idle");
    }

    #[test]
    fn the_claim_order_rotates_so_no_backend_starves_the_shared_budget() {
        // `max_requests_per_pass` bounds the whole pass, so the backends share one allowance. If
        // the order were fixed, a language whose change set always exhausts it would keep every
        // other language's backlog permanently unserviced.
        let order = |first| claim_order(3, first).collect::<Vec<_>>();
        assert_eq!(order(0), vec![0, 1, 2]);
        assert_eq!(order(1), vec![1, 2, 0]);
        assert_eq!(order(2), vec![2, 0, 1]);
        // Every backend is still visited exactly once per pass, whatever the rotation.
        assert_eq!(order(7).len(), 3);
        assert_eq!(order(7).iter().collect::<HashSet<_>>().len(), 3);
        // A wrapped counter must not panic or skip anyone.
        assert_eq!(claim_order(2, usize::MAX).collect::<Vec<_>>(), vec![1, 0]);
        assert_eq!(claim_order(0, 5).count(), 0, "no backends is not a division by zero");
    }

    #[test]
    fn one_backends_failure_does_not_disturb_another() {
        // Backends are independent: a crash streak on one must not gate the other's respawn, or
        // a wedged rust-analyzer would silently stop TypeScript resolution.
        let mut tail = LiveOracleTail::new();
        let now = Instant::now();
        tail.backends[0].lifecycle.on_failure(now);
        assert!(!tail.backends[0].lifecycle.can_respawn(now));
        assert!(tail.backends[1].lifecycle.can_respawn(now), "sibling backoff must be separate");
    }

    #[test]
    fn respawn_backoff_doubles_and_caps_without_allowing_early_retries() {
        let mut backoff = RespawnBackoff::default();
        let mut now = Instant::now();
        for expected in [30, 60, 120, 240, 300, 300] {
            backoff.record_failure(now);
            assert!(!backoff.ready(now));
            assert_eq!(backoff.remaining(now), Some(Duration::from_secs(expected)));
            now += Duration::from_secs(expected);
            assert!(backoff.ready(now));
        }
        backoff.reset();
        assert!(backoff.ready(now));
        assert_eq!(backoff.remaining(now), None);
    }

    #[test]
    fn idle_wake_counts_from_last_session_use() {
        let last_used = Instant::now();
        assert_eq!(
            idle_wake_in(last_used, last_used + Duration::from_secs(3), Duration::from_secs(10)),
            Duration::from_secs(7),
        );
        assert_eq!(
            idle_wake_in(last_used, last_used + Duration::from_secs(10), Duration::from_secs(10)),
            Duration::ZERO,
        );
        assert_eq!(
            scheduled_idle_wake_in(
                last_used,
                last_used + Duration::from_secs(10),
                Duration::from_secs(10),
            ),
            LIVE_ORACLE_RETRY_INTERVAL,
            "an unserviced overdue deadline must not spin the maintenance loop",
        );
        assert!(should_idle_shutdown(
            false,
            last_used,
            last_used + Duration::from_secs(10),
            Duration::from_secs(10),
        ));
        assert!(
            !should_idle_shutdown(
                true,
                last_used,
                last_used + Duration::from_secs(60),
                Duration::from_secs(10),
            ),
            "pending warming work must win even when the session is otherwise idle",
        );
    }

    #[test]
    fn lifecycle_prioritizes_pending_work_and_rearms_unserviced_idle_wakes() {
        let mut lifecycle = LiveOracleLifecycle::default();
        let started = Instant::now();
        let idle = Duration::from_secs(10);
        lifecycle.on_spawned(started);
        assert_eq!(lifecycle.next_wake_in(false, idle, started), Some(idle));

        let overdue = started + Duration::from_secs(60);
        assert!(!lifecycle.idle_shutdown_due(true, idle, overdue));
        assert!(lifecycle.idle_shutdown_due(false, idle, overdue));
        assert_eq!(
            lifecycle.next_wake_in(false, idle, overdue),
            Some(LIVE_ORACLE_RETRY_INTERVAL),
            "a pass that did not service the overdue wake must not spin",
        );

        lifecycle.on_session_ended();
        assert_eq!(lifecycle.next_wake_in(false, idle, overdue), None);

        lifecycle.on_failure(overdue);
        assert!(!lifecycle.can_respawn(overdue));
        assert_eq!(lifecycle.next_wake_in(true, idle, overdue), Some(LIVE_ORACLE_RETRY_INTERVAL),);
    }
}
