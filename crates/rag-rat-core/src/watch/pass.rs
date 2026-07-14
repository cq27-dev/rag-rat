use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::Event;

use super::overlay::{OverlayScope, ReconcileBudget, refresh_worktree_overlays};
use crate::config::Config;
use crate::index::IndexDatabase;
use crate::index::ai::ReconcileOptions;
use crate::locks::{self};

pub(crate) const GC_EVERY_PASSES: u64 = 20;
pub(crate) const PASS_RECONCILE_MAX_SECONDS: u64 = 60;
/// Quiet window before the background clone-graph rebuild fires (#472): the pass tail only spends
/// budget on the clone graph once `content_revision()` has been STABLE this long. Every content
/// change invalidates the whole precomputed graph (its freshness key is the global revision), so
/// an ungated tail under sustained editing discards the in-flight generation and rebuilds from
/// symbol 0 every pass — measured at ~1 GB of DB writes per pass. Shared by the watcher and the
/// hook-driven CLI `maintenance`; explicit `clones --precompute` / full `index` stay immediate.
///
/// Convergence: the watcher's periodic sweeps pick a deferred rebuild up within one sweep of the
/// window elapsing. On a HOOK-ONLY install (no watcher) a deferral waits for the next git action —
/// the same cadence as an embedding backlog left by a time-capped hook pass, and bounded in
/// impact: reads keep serving the last Complete generation, and the write-time clone check falls
/// back to the RAM path until the rebuild lands.
pub const CLONE_GRAPH_QUIET_MS: i64 = 5 * 60 * 1000;
pub(crate) const STARTUP_CATCHUP_RUN_GC: bool = false;
pub(crate) const SKIP_TIMEOUT: Duration = Duration::from_secs(3);

/// Debounce state with a hard max-latency cap. Pure (clock injected) so it is unit-testable without
/// real filesystem events.
#[derive(Debug)]
pub(crate) struct Debounce {
    debounce: Duration,
    max_latency: Duration,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debounce {
    pub(crate) fn new(debounce: Duration, max_latency: Duration) -> Self {
        Self { debounce, max_latency, first: None, last: None }
    }

    pub(crate) fn on_event(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    pub(crate) fn reset(&mut self) {
        self.first = None;
        self.last = None;
    }

    /// When a pass should fire: the earlier of "quiet window since the last event" and "max latency
    /// since the first event". The cap is what guarantees progress under sustained writes.
    pub(crate) fn fire_at(&self) -> Option<Instant> {
        let (first, last) = (self.first?, self.last?);
        Some((last + self.debounce).min(first + self.max_latency))
    }

    pub(crate) fn due_in(&self, now: Instant) -> Option<Duration> {
        self.fire_at().map(|at| at.saturating_duration_since(now))
    }

    pub(crate) fn should_fire(&self, now: Instant) -> bool {
        self.fire_at().is_some_and(|at| now >= at)
    }
}

/// Clock for the periodic all-worktrees sweep backstop, pure like [`Debounce`] (clock injected).
/// It measures time since the last **`All`-scoped** pass COMPLETED — not since any pass (#577
/// review): an event-scoped pass doesn't perform the sweep's duties (refreshing unlisted
/// worktrees, retrying overlay embed backlogs), so a steady drip of scoped passes must escalate
/// the next pass to `All` once the interval elapses rather than keep postponing the backstop.
#[derive(Debug)]
pub(crate) struct SweepClock {
    /// `None` disables the periodic sweep (`periodic_sweep_secs = 0`) — never due.
    interval: Option<Duration>,
    last_sweep: Instant,
    /// Whether the pass currently in flight sweeps every worktree. Starts `true`: the startup
    /// catch-up (an `All` pass) is dispatched just before the event loop runs.
    in_flight_sweeps_all: bool,
}

impl SweepClock {
    pub(crate) fn new(interval: Option<Duration>, now: Instant) -> Self {
        Self { interval, last_sweep: now, in_flight_sweeps_all: true }
    }

    pub(crate) fn on_dispatch(&mut self, scope_is_all: bool) {
        self.in_flight_sweeps_all = scope_is_all;
    }

    /// A pass completed; only an `All`-scoped one resets the backstop interval.
    pub(crate) fn on_pass_done(&mut self, now: Instant) {
        if self.in_flight_sweeps_all {
            self.last_sweep = now;
        }
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        self.interval.is_some_and(|p| now >= self.last_sweep + p)
    }

    pub(crate) fn due_in(&self, now: Instant) -> Option<Duration> {
        self.interval.map(|p| (self.last_sweep + p).saturating_duration_since(now))
    }
}

/// What the event loop asks the pass worker to run (#506).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PassRequest {
    /// Startup catch-up: covers edits made while no watcher was running, and retries a base
    /// embedding backlog left by a bounded prior pass. Does not count toward the gc cadence.
    StartupCatchup,
    Maintenance {
        run_gc: bool,
        /// Which linked-worktree overlays this pass refreshes (#577): the checkouts the event
        /// loop attributed events to since the last dispatch, or `All` for the periodic sweep.
        overlay_scope: OverlayScope,
    },
}

/// Everything that wakes the event loop: filesystem events from notify, pass completions from
/// the worker thread, papertrail flight completions from the auto-sync worker (#592), and the
/// drop-time wake that makes `stop` observable immediately instead of at the next recv timeout
/// (idle waits stretch to the periodic-sweep interval).
#[derive(Debug)]
pub(crate) enum LoopMsg {
    Fs(notify::Result<Event>),
    PassDone,
    PapertrailDone,
    Wake,
}

/// At most one maintenance pass in flight per watcher (#506). A fire condition that arrives while
/// a pass runs is NOT queued: the caller leaves the debounce armed (see [`EventLoop::run`]) so the
/// follow-up dispatches as soon as the completion lands, coalescing any number of mid-pass fires
/// into one.
#[derive(Debug)]
pub(crate) struct PassScheduler {
    inflight: bool,
    passes: u64,
}

impl PassScheduler {
    pub(crate) fn new() -> Self {
        Self { inflight: false, passes: 0 }
    }

    /// The startup catch-up request; in flight like any pass, but outside the gc cadence.
    pub(crate) fn dispatch_startup(&mut self) -> PassRequest {
        self.inflight = true;
        PassRequest::StartupCatchup
    }

    /// The next maintenance request, or `None` while a pass is already in flight. The gc-cadence
    /// pass widens `overlay_scope` to `All`: gc's worktree-liveness sweep wants the full picture,
    /// and 1-in-`GC_EVERY_PASSES` keeps every overlay's missed-event exposure bounded even with
    /// the periodic sweep disabled (#577).
    pub(crate) fn dispatch(&mut self, overlay_scope: OverlayScope) -> Option<PassRequest> {
        if self.inflight {
            return None;
        }
        self.inflight = true;
        self.passes += 1;
        let run_gc = self.passes.is_multiple_of(GC_EVERY_PASSES);
        let overlay_scope = if run_gc { OverlayScope::All } else { overlay_scope };
        Some(PassRequest::Maintenance { run_gc, overlay_scope })
    }

    pub(crate) fn on_done(&mut self) {
        self.inflight = false;
    }

    pub(crate) fn in_flight(&self) -> bool {
        self.inflight
    }
}

/// Spawn the maintenance worker (#506): passes run here, OFF the event-loop thread, so debounced
/// events keep classifying and the fleet hot-upgrade trigger keeps firing while a pass crunches
/// (or sits blocked on the per-DB write lock). Cross-process collision discipline is unchanged —
/// each pass still takes the write lock itself, so concurrent writers (git-hook `maintenance`,
/// other servers' elected watchers, the CLI) serialize exactly as before; the single worker
/// thread is what keeps THIS watcher to one pass at a time. Each request is answered with
/// [`LoopMsg::PassDone`] so the loop can refresh worktree watch state and dispatch a coalesced
/// follow-up; the worker exits when the request channel closes.
pub(crate) fn spawn_pass_worker(
    pass_rx: Receiver<PassRequest>,
    done_tx: Sender<LoopMsg>,
    mut run_pass_request: impl FnMut(&PassRequest) + Send + 'static,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("rag-rat-watch-pass".to_string())
        .spawn(move || {
            while let Ok(request) = pass_rx.recv() {
                run_pass_request(&request);
                if done_tx.send(LoopMsg::PassDone).is_err() {
                    return;
                }
            }
        })
        .ok()
}

/// Run one maintenance pass, blocking on the per-DB write lock (watcher-to-watcher serializes).
pub fn maintenance_pass(config: &Config, run_gc: bool) -> anyhow::Result<()> {
    maintenance_pass_scoped(config, run_gc, &OverlayScope::All)
}

/// [`maintenance_pass`] with an explicit overlay scope — the watcher's event-driven passes name
/// the checkouts events came from instead of sweeping the whole worktree fleet (#577).
pub(crate) fn maintenance_pass_scoped(
    config: &Config,
    run_gc: bool,
    overlay_scope: &OverlayScope,
) -> anyhow::Result<()> {
    let lock_repo = locks::write_lock_repo_id(config);
    let _lock = locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
    run_pass(config, run_gc, false, overlay_scope)
}

/// Run one maintenance pass only if the write lock is free within `SKIP_TIMEOUT`; returns whether
/// it ran. Used by interactive / hook / shutdown callers so a held lock can't hang them.
pub fn maintenance_pass_or_skip(config: &Config, run_gc: bool) -> anyhow::Result<bool> {
    let lock_repo = locks::write_lock_repo_id(config);
    match locks::WriteLock::acquire_timeout(&config.database, &lock_repo, SKIP_TIMEOUT)? {
        Some(_lock) => {
            run_pass(config, run_gc, false, &OverlayScope::All)?;
            Ok(true)
        },
        None => Ok(false),
    }
}

pub(crate) fn startup_catchup_pass(config: &Config) -> anyhow::Result<()> {
    let lock_repo = locks::write_lock_repo_id(config);
    let _lock = locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
    run_pass(config, STARTUP_CATCHUP_RUN_GC, true, &OverlayScope::All)
}

fn run_pass(
    config: &Config,
    run_gc: bool,
    retry_base_embedding_backlog: bool,
    overlay_scope: &OverlayScope,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut timings = PassTimings::new(started);
    // #427: the core refuses a first-time-empty registration (`rag-rat mcp` / a hook on a config
    // with no `[target_bindings]` or no matching files). A maintenance/watch pass treats that as
    // "nothing to index yet" and DEFERS silently — a later pass registers once real content
    // appears. A recorded root going empty still prunes (not first-time → not refused). The
    // one-shot `index` command instead surfaces the same error to the operator.
    let discover = timings.stage("discover", || IndexDatabase::index_discover_reporting(config));
    let (mut db, content_changed) = match discover {
        Ok(result) => result,
        Err(err) if err.downcast_ref::<crate::index::EmptyIndexRefused>().is_some() => {
            return Ok(());
        },
        Err(err) => return Err(err),
    };
    let shutdown_reconcile_pending = db.watch_shutdown_reconcile_pending()?;
    let runtime = &config.llm.embedding.runtime;
    let options = ReconcileOptions {
        batch_size: Some(runtime.batch_size),
        changed_first: true,
        max_seconds: Some(PASS_RECONCILE_MAX_SECONDS),
        max_embedding_chars: runtime.max_embedding_chars,
        intra_threads: runtime.ort_threads.map(|n| n as usize),
        ..ReconcileOptions::default()
    };
    // One time budget shared by the per-overlay reconciles AND the base reconcile below, so a pass
    // with several changed overlays can't blow past `PASS_RECONCILE_MAX_SECONDS` (N+1)× over (#219
    // review). Measured from `started` so discovery time already counts against it.
    let budget = ReconcileBudget::new(options, started);
    // Keep every live linked worktree's branch overlay fresh (#219), so a `worktree`-scoped query
    // sees that branch's changes without a manual `index --worktree`. Delta-only and idle-safe (the
    // overlay pass writes nothing when a worktree is unchanged), so it can run every pass; a
    // worktree change counts toward running the tail below even when config.root itself didn't
    // change. Reconcile a CHANGED overlay's embeddings INLINE (while scoped to it) so a worktree
    // query isn't BM25-only for branch content — the base reconcile below can't see overlay chunks.
    let overlays_changed = timings.stage("overlays", || {
        refresh_worktree_overlays(&mut db, config, Some(&budget), overlay_scope)
    });
    let tail_already_forced = pass_tail_forced_by_state(
        content_changed,
        overlays_changed,
        run_gc,
        shutdown_reconcile_pending,
    );
    let base_embedding_backlog = base_embedding_backlog_needs_tail(
        tail_already_forced,
        retry_base_embedding_backlog,
        &budget,
        |options| {
            db.pending_embedding_jobs_with_available_incremental_embedder(options)
                .is_ok_and(|pending| pending > 0)
        },
    );
    // Clone-graph quiet gate (#472): one probe serves two roles. Inside the tail it REPLACES the
    // bare `pending_clone_graph` check — a pass during active editing arms/re-arms the quiet
    // window instead of discarding the in-flight generation and rebuilding the whole graph — and
    // on an otherwise-idle pass a quiet-elapsed backlog FORCES the tail (like the embedding
    // backlog above) so the owed rebuild lands right after the churn pauses rather than waiting
    // for a gc pass. Unlike the embedding probe it must also run whenever the tail will run —
    // including a startup embedding-backlog tail, which doesn't flip `tail_already_forced` — so
    // every tail pass can at least ARM an unarmed pending graph (else it rides until a content
    // change or gc pass). When nothing runs the tail the probe stays cheap: without an armed
    // candidate it skips the content-revision digest entirely.
    let clone_graph_due = db
        .clone_graph_rebuild_due(
            CLONE_GRAPH_QUIET_MS,
            tail_already_forced || base_embedding_backlog,
        )
        .unwrap_or(false);
    // Idle backstop (issue #63, facet 2): when the sweep changed no content, skip the reconcile /
    // gc / memory-validate tail — an idle server should do no work past discovery. `run_gc` (every
    // GC_EVERY_PASSES) still forces a full tail, so the cases that DON'T flip content_changed are
    // still caught within that bound: a freshly-installed embedder, an embedding backlog left by a
    // time-capped reconcile (PASS_RECONCILE_MAX_SECONDS), and drifted memory anchors. Any real
    // content change runs the full tail immediately. Startup has two discover-only exceptions:
    // a prior bounded shutdown discover that marked base reconcile owed, and an already-indexed
    // base embedding backlog left by a time-capped or blocked prior pass.
    // A pass with no content/overlay change is the quiet moment WAL hygiene waits for (#482) —
    // whether or not something else (gc, a backlog, the clone gate) still forces the tail below.
    let quiet_pass = !content_changed && !overlays_changed;
    if !should_run_pass_tail(
        content_changed,
        overlays_changed,
        run_gc,
        shutdown_reconcile_pending,
        base_embedding_backlog,
        clone_graph_due,
    ) {
        timings.stage("wal", || {
            maybe_checkpoint_wal(&db, quiet_pass, crate::index::WAL_CHECKPOINT_MIN_BYTES)
        });
        timings.emit(false, content_changed, overlays_changed);
        return Ok(());
    }
    // The base reconcile gets only the budget the overlays left behind; `None` → already exhausted,
    // so skip it (the embedding backlog rides the next pass) rather than spend a fresh full budget.
    let mut base_reconcile_status = None;
    if let Some(options) = budget.next_options() {
        let report =
            timings.stage("reconcile", || db.reconcile_with_options_progress(options, |_| {}))?;
        base_reconcile_status = Some(report.status);
    }
    // Clone-edge graph (#286/#473): try the cheap IN-PLACE delta first — it settles an ordinary
    // edit on this very pass (freshness is the point; no quiet window) and reports whether a FULL
    // rebuild is still owed (accumulated df drift). The full rebuild runs only when the delta
    // could not settle freshness (absent generation, normalizer bump, cap crossing, huge delta,
    // error) or a drift refresh is owed — and it stays behind the #472 quiet window
    // (`clone_graph_due`) so sustained editing defers it instead of treadmilling. Best-effort +
    // resumable, with whatever budget the embedding reconcile left (shared
    // PASS_RECONCILE_MAX_SECONDS so a pass can't overrun); `None` budget → rides the next pass.
    let clone_full_rebuild_owed = match timings
        .stage("clone_delta", || db.apply_clone_graph_delta(crate::index::CLONE_DELTA_MAX_FILES))
    {
        Ok(delta) if delta.status == "Applied" || delta.status == "Noop" => delta.full_rebuild_owed,
        _ => true,
    };
    if clone_full_rebuild_owed
        && clone_graph_due
        && let Some(options) = budget.next_options()
    {
        let _ = timings
            .stage("clone_rebuild", || db.reconcile_clone_edges_with_budget(options.max_seconds));
    }
    if run_gc {
        let _ = timings.stage("gc", || db.garbage_collect());
    }
    let _ = timings.stage("memory_validate", || db.memory_validate());
    if shutdown_reconcile_pending && base_reconcile_status.as_deref() == Some("Current") {
        db.clear_watch_shutdown_reconcile_pending()?;
    }
    timings.stage("wal", || {
        maybe_checkpoint_wal(&db, quiet_pass, crate::index::WAL_CHECKPOINT_MIN_BYTES)
    });
    timings.emit(true, content_changed, overlays_changed);
    Ok(())
}

/// Per-stage wall-clock for one maintenance pass, emitted as a single summary event (#502): the
/// dogfood investigation had to reconstruct where a long pass went from `reconcile_attempts`
/// timestamps because the pass logged no stage timings. One greppable line per pass fixes that.
/// Tail passes log at `info`; an idle discover-only sweep logs at `debug` so a quiet server does
/// not fill the log.
struct PassTimings {
    started: Instant,
    stages: Vec<(&'static str, Duration)>,
}

impl PassTimings {
    fn new(started: Instant) -> Self {
        Self { started, stages: Vec::new() }
    }

    fn stage<T>(&mut self, name: &'static str, work: impl FnOnce() -> T) -> T {
        let stage_started = Instant::now();
        let result = work();
        self.stages.push((name, stage_started.elapsed()));
        result
    }

    fn emit(&self, tail_ran: bool, content_changed: bool, overlays_changed: bool) {
        let stages = self
            .stages
            .iter()
            .map(|(name, elapsed)| format!("{name}={}ms", elapsed.as_millis()))
            .collect::<Vec<_>>()
            .join(" ");
        let total_ms = self.started.elapsed().as_millis() as u64;
        if tail_ran {
            tracing::info!(
                target: "rag_rat_core::maintenance",
                total_ms,
                content_changed,
                overlays_changed,
                %stages,
                "maintenance pass"
            );
        } else {
            tracing::debug!(
                target: "rag_rat_core::maintenance",
                total_ms,
                content_changed,
                overlays_changed,
                %stages,
                "maintenance pass (tail skipped)"
            );
        }
    }
}

/// Opportunistic WAL hygiene (#482), on QUIET passes only: nothing else truncates the shared
/// database's `-wal`, so it keeps its high-water mark forever without this. During sustained
/// editing every pass has content changes, so the truncate — which waits on concurrent readers up
/// to the busy timeout — defers exactly like the clone-graph rebuild and lands once churn pauses.
/// Under `min_bytes` the probe is a bare stat of the sidecar, free on idle passes. Best-effort: a
/// busy or failed checkpoint just rides the next quiet pass, and never fails the pass itself.
pub(crate) fn maybe_checkpoint_wal(db: &IndexDatabase, quiet_pass: bool, min_bytes: u64) {
    if !quiet_pass {
        return;
    }
    match db.checkpoint_wal_if_oversized(min_bytes) {
        Ok(report) if report.attempted => {
            tracing::debug!(
                target: "rag_rat_core::watch",
                wal_bytes_before = report.wal_bytes_before,
                truncated = report.truncated,
                "wal checkpoint attempted"
            );
        },
        Ok(_) => {},
        Err(err) => {
            tracing::debug!(target: "rag_rat_core::watch", error = %err, "wal checkpoint failed");
        },
    }
}

pub(crate) fn should_run_pass_tail(
    content_changed: bool,
    overlays_changed: bool,
    run_gc: bool,
    shutdown_reconcile_pending: bool,
    base_embedding_backlog: bool,
    clone_graph_due: bool,
) -> bool {
    content_changed
        || overlays_changed
        || run_gc
        || shutdown_reconcile_pending
        || base_embedding_backlog
        || clone_graph_due
}

pub(crate) fn pass_tail_forced_by_state(
    content_changed: bool,
    overlays_changed: bool,
    run_gc: bool,
    shutdown_reconcile_pending: bool,
) -> bool {
    content_changed || overlays_changed || run_gc || shutdown_reconcile_pending
}

pub(crate) fn base_embedding_backlog_needs_tail(
    tail_already_forced: bool,
    retry_base_embedding_backlog: bool,
    budget: &ReconcileBudget,
    pending_embedding_jobs: impl FnOnce(&ReconcileOptions) -> bool,
) -> bool {
    if tail_already_forced || !retry_base_embedding_backlog {
        return false;
    }
    budget.next_options().is_some_and(|options| pending_embedding_jobs(&options))
}
