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
use rag_rat_oracle::{LiveBackend, LiveOracleSession, LivePassAbort, LivePassReport};

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
    /// Paths a pass skipped because the session could not configure their files, held HERE rather
    /// than in `backlog`: a non-empty backlog is what schedules the next pass, and these can only
    /// be skipped again until the checkout's project layout changes. Moved into the backlog when a
    /// pass reports that it did — see [`LiveBackendTail::retain_unconfigured`].
    ///
    /// Bounded by being a SET fed only from worklists this backend actually ran: it holds at most
    /// one entry per distinct file of this backend's languages the watcher has seen change, and a
    /// path leaves again as soon as a pass carries it without skipping it. Re-editing the same
    /// unconfigurable file does not grow it.
    unconfigured_paths: BTreeSet<String>,
    lifecycle: LiveOracleLifecycle,
    /// Whether this backend's unmet checkout prerequisite has already been reported. The block is
    /// permanent until the checkout changes, so it is worth saying — once, not on every retry.
    prerequisite_reported: bool,
    /// Consecutive passes this backend has spent warming without ever resolving anything, and
    /// whether that has been reported. See [`WARMING_PASSES_BEFORE_REPORT`].
    warming_passes: u32,
    warming_reported: bool,
    /// Whether a pass that asked the server nothing because it could configure none of its files
    /// has already been reported. Cleared as soon as a pass issues a request. See
    /// [`LiveBackendTail::note_unconfigured`].
    unconfigured_reported: bool,
}

/// How many consecutive warming passes go by before the watcher says so. A cold language server
/// legitimately warms for a pass or two; a server that never becomes ready is a real problem the
/// operator cannot otherwise see, because the safe behaviour — never ask a warming server — is
/// also a SILENT one.
///
/// The causes are open-ended (a language server that cannot resolve a compiler for the project, a
/// broken toolchain install, a project too large to load inside the retry window), so this reports
/// the observed state rather than trying to predict any particular cause.
const WARMING_PASSES_BEFORE_REPORT: u32 = 5;

impl LiveBackendTail {
    fn new(backend: LiveBackend) -> Self {
        Self {
            backend,
            session: None,
            backlog: Vec::new(),
            unconfigured_paths: BTreeSet::new(),
            lifecycle: LiveOracleLifecycle::default(),
            prerequisite_reported: false,
            warming_passes: 0,
            warming_reported: false,
            unconfigured_reported: false,
        }
    }

    /// Delay until this backend needs another pass. A backlog takes priority; otherwise a
    /// resident session schedules its own idle shutdown.
    fn next_wake_in(&self, idle: Duration, now: Instant) -> Option<Duration> {
        self.lifecycle.next_wake_in(!self.backlog.is_empty(), idle, now)
    }

    /// Track consecutive warming passes and report a server that never becomes ready.
    ///
    /// Refusing to ask a warming server is the correct behaviour, but on its own it is
    /// indistinguishable from working: the backlog just rides forever. Say so once, and reset as
    /// soon as the backend gets anywhere, so a normally-warming server stays quiet.
    fn note_warming(&mut self, status: &str) {
        if status != "Warming" {
            self.warming_passes = 0;
            self.warming_reported = false;
            return;
        }
        self.warming_passes = self.warming_passes.saturating_add(1);
        if self.warming_passes >= WARMING_PASSES_BEFORE_REPORT && !self.warming_reported {
            self.warming_reported = true;
            tracing::warn!(
                target: "rag_rat_core::watch",
                tool = self.backend.tool.as_db_str(),
                passes = self.warming_passes,
                "live oracle: the language server has not reported a completed project load — \
                 it resolves nothing until it does. Check that it can load this project (for \
                 TypeScript, that a `typescript` package resolves for the tsconfig project)."
            );
        }
    }

    /// Report a pass that issued no requests at all while skipping candidates whose files the
    /// session cannot configure.
    ///
    /// Skipping such a file is the correct answer — the server would otherwise answer it with
    /// fallback flags, which resolve a call into another translation unit to the callee's header
    /// declaration, and that wrong answer would be persisted as a real verdict. But the skip is
    /// deliberately not deferred, so a pass made entirely of them writes no rows AND leaves no
    /// backlog: the two things the per-pass log keys on. Say it once, and reset as soon as a pass
    /// issues a request, so a checkout the session can configure stays quiet.
    fn note_unconfigured(&mut self, report: &LivePassReport) {
        // One request is enough to prove this session configures something in this checkout.
        if report.requests_used > 0 {
            self.unconfigured_reported = false;
            return;
        }
        if report.skipped_unconfigured == 0 || self.unconfigured_reported {
            return;
        }
        self.unconfigured_reported = true;
        tracing::warn!(
            target: "rag_rat_core::watch",
            tool = self.backend.tool.as_db_str(),
            skipped = report.skipped_unconfigured,
            "live oracle: this pass sent the server no requests and skipped candidates because \
             the session cannot configure their files. A compilation database is pinned for the \
             server (`--compile-commands-dir`) only when the checkout holds exactly one; \
             otherwise the server has to find each file's database itself, and a file it cannot \
             find one for is skipped rather than answered with fallback flags. Those files stay \
             unresolvable until the checkout's layout changes — leave a single compilation \
             database, or put each file's database in one of its ancestor directories or that \
             directory's `build/`."
        );
    }

    /// Fold one pass's unconfigurable skips into the retained set, and requeue that whole set when
    /// the pass reports the layout change that can make those files resolvable.
    ///
    /// Without this an operator who does exactly what [`Self::note_unconfigured`] asked of them —
    /// consolidate the checkout's compilation databases — sees nothing happen: a pass admits only
    /// this backend's changed paths plus the backlog, so editing the layout cannot requeue the
    /// sources it just made resolvable, and they stay without live evidence until someone edits
    /// them again.
    ///
    /// An ordinary abort (a dead or wedged server) does NOT requeue them: the checkout's layout is
    /// exactly what it was, so a replacement session would skip them again. Some may still ride
    /// `unfinished_paths` — an abort defers every candidate-bearing path after the file it stopped
    /// on, unconfigurable ones included — which is harmless: one re-skip and they drop back out of
    /// the backlog and into this set.
    fn retain_unconfigured(&mut self, worklist: &[String], report: &LivePassReport) {
        if report.abort == Some(LivePassAbort::LayoutChanged) {
            // Moved, not copied: the next session answers "can I configure this?" afresh, and
            // whatever it still cannot configure comes back here from its own pass. A path the
            // aborted worklist already put in the backlog collapses in `assemble_worklist`.
            self.backlog.extend(std::mem::take(&mut self.unconfigured_paths));
            return;
        }
        // A path this pass carried and did not skip is either configurable now or carries no
        // candidates at all; either way it no longer belongs here.
        for path in worklist {
            self.unconfigured_paths.remove(path);
        }
        self.unconfigured_paths.extend(report.skipped_unconfigured_paths.iter().cloned());
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
        // One of this backend's languages must be indexed in the checkout (clangd serves two).
        if !config.targets.iter().any(|target| self.backend.resolves_language(target.language)) {
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
                self.retain_unconfigured(&worklist, &report);
                // An aborted pass means the server died or wedged mid-resolution, or the checkout
                // moved out from under the session: drop it so the next pass respawns a clean one
                // instead of reusing a broken transport or a stale argv (the aborted files are
                // already requeued in `unfinished_paths`).
                if report.abort.is_some()
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
                self.note_warming(&report.status);
                self.note_unconfigured(&report);
                // A pass whose candidates were all skipped as unconfigured writes no rows and
                // defers nothing (those skips are not retried), so it has to be admitted to the
                // log on its own count or the pass leaves no trace at all.
                if report.rows_written > 0
                    || !report.unfinished_paths.is_empty()
                    || report.skipped_unconfigured > 0
                {
                    tracing::info!(
                        target: "rag_rat_core::watch",
                        rows_written = report.rows_written,
                        upgraded = report.upgraded,
                        confirmed = report.confirmed,
                        contradicted = report.contradicted,
                        requests = report.requests_used,
                        deferred = report.unfinished_paths.len(),
                        skipped_unconfigured = report.skipped_unconfigured,
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
    fn a_server_that_never_warms_is_reported_once_then_stays_quiet() {
        // Refusing to ask a warming server is correct but SILENT — on its own it is
        // indistinguishable from the backend working, and the backlog just rides forever. The
        // watcher has to say so, once, and stop as soon as the backend gets anywhere.
        let mut tail =
            LiveBackendTail::new(LiveBackend::for_tool(rag_rat_oracle::OracleTool::TsLsp).unwrap());
        for _ in 0..WARMING_PASSES_BEFORE_REPORT - 1 {
            tail.note_warming("Warming");
            assert!(!tail.warming_reported, "a normally-warming server must stay quiet");
        }
        tail.note_warming("Warming");
        assert!(tail.warming_reported, "a server that never warms must be reported");
        tail.note_warming("Warming");
        assert!(tail.warming_reported, "reported ONCE, not on every later pass");

        // Any progress at all clears the streak, so a later cold start reports afresh.
        tail.note_warming("Completed");
        assert_eq!(tail.warming_passes, 0);
        assert!(!tail.warming_reported);
    }

    /// A `MakeWriter` that appends every formatted log line into a shared buffer, so a test can
    /// assert on the `tracing` events a pass actually emitted — and on how many times.
    #[derive(Clone)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `body` with warnings captured, returning what it logged. The subscriber is thread-local
    /// (`with_default`), so parallel tests do not see each other's output.
    fn captured_warnings(body: impl FnOnce()) -> String {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(CaptureWriter(std::sync::Arc::clone(&buffer)))
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        let logged = buffer.lock().unwrap().clone();
        String::from_utf8(logged).expect("formatted log lines are UTF-8")
    }

    #[test]
    fn a_backend_that_can_configure_nothing_is_reported_once_then_stays_quiet() {
        // A candidate skipped because the session cannot configure its file is deliberately NOT
        // deferred — retrying cannot help until the checkout's layout changes. So a pass made
        // entirely of such skips writes no rows and leaves no backlog, and without a report of its
        // own the backend resolves nothing pass after pass while saying nothing at all.
        let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
        let all_skipped =
            || LivePassReport { skipped_unconfigured: 3, ..LivePassReport::default() };
        let occurrences = |logged: &str| logged.matches("cannot configure their files").count();

        let logged = captured_warnings(|| {
            tail.note_unconfigured(&all_skipped());
            tail.note_unconfigured(&all_skipped());
        });
        assert_eq!(occurrences(&logged), 1, "reported ONCE, not on every later pass: {logged:?}");

        // A pass that issues a request proves the session configures something here, so the
        // report is not repeated for it…
        let quiet = captured_warnings(|| {
            tail.note_unconfigured(&LivePassReport {
                requests_used: 1,
                skipped_unconfigured: 1,
                ..LivePassReport::default()
            });
        });
        assert_eq!(occurrences(&quiet), 0, "a pass that resolves anything is not a dry spell");
        // …and the streak is cleared, so a later all-skipped pass reports afresh.
        let again = captured_warnings(|| tail.note_unconfigured(&all_skipped()));
        assert_eq!(occurrences(&again), 1, "a new dry spell must be reported: {again:?}");
    }

    /// A pass that skipped `paths` because the session could not configure their files.
    fn all_skipped_report(paths: &[String]) -> LivePassReport {
        LivePassReport {
            skipped_unconfigured: paths.len() as u64,
            skipped_unconfigured_paths: paths.to_vec(),
            ..LivePassReport::default()
        }
    }

    /// A pass that ended early for `abort` without reaching any file.
    fn aborted_report(abort: LivePassAbort) -> LivePassReport {
        LivePassReport { abort: Some(abort), ..LivePassReport::default() }
    }

    #[test]
    fn a_path_the_session_cannot_configure_is_retained_without_scheduling_another_pass() {
        // The skip is deliberately not deferred, and a non-empty backlog is exactly what makes the
        // watcher schedule another pass — so parking these in the backlog would spin it forever on
        // work every pass can only skip again. They still have to be kept somewhere, or the layout
        // change that makes them resolvable has nothing to bring back.
        let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
        let worklist = vec!["b/main.c".to_string()];
        tail.retain_unconfigured(&worklist, &all_skipped_report(&worklist));

        assert!(tail.backlog.is_empty(), "a permanently-skipped path must not ride the backlog");
        assert_eq!(tail.unconfigured_paths, BTreeSet::from(["b/main.c".to_string()]));
        assert_eq!(
            tail.next_wake_in(Duration::from_secs(600), Instant::now()),
            None,
            "what is retained here must not schedule a pass on its own",
        );

        // Deduped across passes: re-editing the same unconfigurable file cannot grow the set.
        tail.retain_unconfigured(&worklist, &all_skipped_report(&worklist));
        assert_eq!(tail.unconfigured_paths.len(), 1);

        // A pass that carries the path and does NOT skip it drops it again — whatever it is now,
        // it is no longer a file waiting on a layout change.
        tail.retain_unconfigured(&worklist, &LivePassReport::default());
        assert!(tail.unconfigured_paths.is_empty());
    }

    #[test]
    fn only_a_layout_change_requeues_the_paths_the_session_could_not_configure() {
        let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
        let worklist = vec!["b/main.c".to_string()];
        tail.retain_unconfigured(&worklist, &all_skipped_report(&worklist));

        // A wedged server is not a layout change: the checkout is exactly as it was, so the file
        // is exactly as unconfigurable and a requeue would buy nothing but another skip.
        tail.retain_unconfigured(&[], &aborted_report(LivePassAbort::Server));
        assert!(tail.backlog.is_empty(), "a server abort must not requeue an unconfigurable path");
        assert_eq!(tail.unconfigured_paths.len(), 1, "…and must not drop it either");

        // A layout change does. This is the operator having fixed exactly what the warning asked
        // for, and the whole point is that something happens when they do.
        tail.retain_unconfigured(&[], &aborted_report(LivePassAbort::LayoutChanged));
        assert_eq!(tail.backlog, vec!["b/main.c".to_string()]);
        assert!(tail.unconfigured_paths.is_empty(), "moved into the backlog, not copied");
        assert_eq!(
            tail.next_wake_in(Duration::from_secs(600), Instant::now()),
            Some(LIVE_ORACLE_RETRY_INTERVAL),
            "the requeued work schedules the pass that resolves it",
        );
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
