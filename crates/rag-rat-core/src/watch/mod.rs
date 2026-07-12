//! Background file watcher: keeps the active index (and dirty-worktree overlay) fresh as files
//! change, so graph/symbol queries reflect uncommitted edits without waiting for a commit.
//!
//! - **One watcher per worktree** via the election lock; **one writer at a time per DB** via the
//!   write lock (see [`crate::locks`]).
//! - Watches the configured target *directories* and their non-ignored subtrees (so **new files**
//!   are seen) — placing a watch per non-ignored directory rather than one recursive watch, so a
//!   gitignored build/dependency tree can't exhaust `fs.inotify.max_user_watches` (issue #331).
//!   Classifies events through the target globs to decide whether to fire, and debounces bursts
//!   with a max-latency cap so sustained writes can't starve a pass.
//! - Each pass runs the existing pipeline: discover → reconcile → (rate-limited) gc →
//!   memory_validate. Discover handles additions/edits/deletions; the pass is idempotent.
//! - Passes execute on a dedicated worker thread, never on the event loop (#506): a long stage
//!   (cold embedding backlog, clone-graph rebuild) or a blocked write-lock acquisition must not
//!   stop events from classifying or the fleet hot-upgrade trigger from firing. One pass in flight
//!   at a time; fire conditions that arrive mid-pass coalesce into the armed debounce.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind, RenameMode};
use notify::{Event, RecursiveMode, recommended_watcher};

use crate::config::Config;
use crate::fleet;
use crate::index::ai::ReconcileOptions;
use crate::index::ignore_rules::{IgnoreMatcher, target_ancestor_dirs};
use crate::index::{IndexDatabase, target_for_path};
use crate::locks::{self, FileLock};

/// Run gc on every Nth watcher pass (deletion reconciliation is already handled by discover, so gc
/// — which shells to `git worktree list` + a liveness scan — need not run every keystroke burst).
const GC_EVERY_PASSES: u64 = 20;
/// Bound a single reconcile so a pass never holds the write lock indefinitely.
const PASS_RECONCILE_MAX_SECONDS: u64 = 60;
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
const STARTUP_CATCHUP_RUN_GC: bool = false;
/// Shutdown / interactive lock acquisition: skip rather than block forever.
const SKIP_TIMEOUT: Duration = Duration::from_secs(3);
/// Quiet window after a change to the installed binary before signaling the fleet to hot-upgrade.
/// `cargo install` writes a temp file then renames; the debounce lets the rename settle.
const FLEET_DEBOUNCE: Duration = Duration::from_millis(500);
/// Max-latency cap for the fleet-trigger debounce (sustained binary churn still fires).
const FLEET_MAX_LATENCY: Duration = Duration::from_millis(2000);
/// Event-loop wait when no deadline is armed — also bounds the stop-flag check while a pass is in
/// flight, since the loop must keep iterating for the fleet trigger during a long pass (#506).
const IDLE_WAIT: Duration = Duration::from_millis(500);

/// Debounce state with a hard max-latency cap. Pure (clock injected) so it is unit-testable without
/// real filesystem events.
#[derive(Debug)]
struct Debounce {
    debounce: Duration,
    max_latency: Duration,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debounce {
    fn new(debounce: Duration, max_latency: Duration) -> Self {
        Self { debounce, max_latency, first: None, last: None }
    }

    fn on_event(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    fn reset(&mut self) {
        self.first = None;
        self.last = None;
    }

    /// When a pass should fire: the earlier of "quiet window since the last event" and "max latency
    /// since the first event". The cap is what guarantees progress under sustained writes.
    fn fire_at(&self) -> Option<Instant> {
        let (first, last) = (self.first?, self.last?);
        Some((last + self.debounce).min(first + self.max_latency))
    }

    fn due_in(&self, now: Instant) -> Option<Duration> {
        self.fire_at().map(|at| at.saturating_duration_since(now))
    }

    fn should_fire(&self, now: Instant) -> bool {
        self.fire_at().is_some_and(|at| now >= at)
    }
}

/// Clock for the periodic all-worktrees sweep backstop, pure like [`Debounce`] (clock injected).
/// It measures time since the last **`All`-scoped** pass COMPLETED — not since any pass (#577
/// review): an event-scoped pass doesn't perform the sweep's duties (refreshing unlisted
/// worktrees, retrying overlay embed backlogs), so a steady drip of scoped passes must escalate
/// the next pass to `All` once the interval elapses rather than keep postponing the backstop.
#[derive(Debug)]
struct SweepClock {
    /// `None` disables the periodic sweep (`periodic_sweep_secs = 0`) — never due.
    interval: Option<Duration>,
    last_sweep: Instant,
    /// Whether the pass currently in flight sweeps every worktree. Starts `true`: the startup
    /// catch-up (an `All` pass) is dispatched just before the event loop runs.
    in_flight_sweeps_all: bool,
}

impl SweepClock {
    fn new(interval: Option<Duration>, now: Instant) -> Self {
        Self { interval, last_sweep: now, in_flight_sweeps_all: true }
    }

    fn on_dispatch(&mut self, scope_is_all: bool) {
        self.in_flight_sweeps_all = scope_is_all;
    }

    /// A pass completed; only an `All`-scoped one resets the backstop interval.
    fn on_pass_done(&mut self, now: Instant) {
        if self.in_flight_sweeps_all {
            self.last_sweep = now;
        }
    }

    fn due(&self, now: Instant) -> bool {
        self.interval.is_some_and(|p| now >= self.last_sweep + p)
    }

    fn due_in(&self, now: Instant) -> Option<Duration> {
        self.interval.map(|p| (self.last_sweep + p).saturating_duration_since(now))
    }
}

/// What the event loop asks the pass worker to run (#506).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PassRequest {
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

/// Everything that wakes the event loop: filesystem events from notify, pass completions from the
/// worker thread, and the drop-time wake that makes `stop` observable immediately instead of at
/// the next recv timeout (idle waits stretch to the periodic-sweep interval).
#[derive(Debug)]
enum LoopMsg {
    Fs(notify::Result<Event>),
    PassDone,
    Wake,
}

/// At most one maintenance pass in flight per watcher (#506). A fire condition that arrives while
/// a pass runs is NOT queued: the caller leaves the debounce armed (see [`EventLoop::run`]) so the
/// follow-up dispatches as soon as the completion lands, coalescing any number of mid-pass fires
/// into one.
#[derive(Debug)]
struct PassScheduler {
    inflight: bool,
    passes: u64,
}

impl PassScheduler {
    fn new() -> Self {
        Self { inflight: false, passes: 0 }
    }

    /// The startup catch-up request; in flight like any pass, but outside the gc cadence.
    fn dispatch_startup(&mut self) -> PassRequest {
        self.inflight = true;
        PassRequest::StartupCatchup
    }

    /// The next maintenance request, or `None` while a pass is already in flight. The gc-cadence
    /// pass widens `overlay_scope` to `All`: gc's worktree-liveness sweep wants the full picture,
    /// and 1-in-`GC_EVERY_PASSES` keeps every overlay's missed-event exposure bounded even with
    /// the periodic sweep disabled (#577).
    fn dispatch(&mut self, overlay_scope: OverlayScope) -> Option<PassRequest> {
        if self.inflight {
            return None;
        }
        self.inflight = true;
        self.passes += 1;
        let run_gc = self.passes.is_multiple_of(GC_EVERY_PASSES);
        let overlay_scope = if run_gc { OverlayScope::All } else { overlay_scope };
        Some(PassRequest::Maintenance { run_gc, overlay_scope })
    }

    fn on_done(&mut self) {
        self.inflight = false;
    }

    fn in_flight(&self) -> bool {
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
fn spawn_pass_worker(
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
fn maintenance_pass_scoped(
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

fn startup_catchup_pass(config: &Config) -> anyhow::Result<()> {
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
fn maybe_checkpoint_wal(db: &IndexDatabase, quiet_pass: bool, min_bytes: u64) {
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

fn should_run_pass_tail(
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

fn pass_tail_forced_by_state(
    content_changed: bool,
    overlays_changed: bool,
    run_gc: bool,
    shutdown_reconcile_pending: bool,
) -> bool {
    content_changed || overlays_changed || run_gc || shutdown_reconcile_pending
}

fn base_embedding_backlog_needs_tail(
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

/// Which linked-worktree overlays a maintenance pass refreshes (#577). The per-worktree refresh
/// is write-idle on an unchanged worktree but never FREE: each one pays a base↔linked tree diff,
/// a full working-tree status walk, and an `IgnoreMatcher` compile. Sweeping every live worktree
/// on every pass made the pass cost scale with the whole worktree fleet instead of with what
/// changed — on a repo with several active agent worktrees that was ~3 s of overlay sweep per
/// otherwise-idle pass, all day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayScope {
    /// Refresh every live linked worktree: startup catch-up, the periodic sweep (the backstop for
    /// missed events), gc passes, and the CLI/hook `maintenance` command.
    All,
    /// Refresh the listed checkout roots (the ones the watcher attributed events to since the
    /// last dispatch) — plus any worktree whose recorded refresh basis no longer matches, so a
    /// base or linked commit is never missed just because no file event named the checkout. An
    /// empty set is a base-only pass: discovery covers the base scope regardless of this value.
    Linked(BTreeSet<PathBuf>),
}

impl OverlayScope {
    /// Whether `worktree_id` (the canonical id `live_worktree_contexts` reports) is listed.
    /// Scope roots are event/checkout paths; compare via `worktree_id_of` so the event spelling
    /// and the overlay key can't drift (the same canonicalization every scope consumer uses).
    fn lists(&self, worktree_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Linked(roots) =>
                roots.iter().any(|root| crate::index::worktree_id_of(root) == worktree_id),
        }
    }

    /// Fold another event's contribution into the scope accumulated while the debounce is armed:
    /// attributable roots union; an unattributable contribution widens the whole pass to `All`.
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::Linked(mut roots), Self::Linked(more)) => {
                roots.extend(more);
                Self::Linked(roots)
            },
        }
    }
}

/// Refresh the branch overlay of live LINKED worktrees of `config.root`'s repo (#219), so a
/// `worktree`-scoped query stays current without a manual `index --worktree`. Returns whether any
/// overlay actually changed. `index_worktree_overlay` is delta-only and idle-safe (a static
/// worktree writes nothing), and the connection is restored to the base scope afterward so the rest
/// of the pass (reconcile / gc / memory-validate) runs unscoped as before. Best-effort per worktree
/// — a failure on one worktree is logged and doesn't abort the pass.
///
/// `reconcile` (when `Some`): after a CHANGED overlay is indexed — while the connection is STILL
/// scoped to that overlay — reconcile its embeddings, so worktree-scoped `semantic_search` is not
/// BM25-only for branch content. The pass's trailing reconcile runs AFTER this returns, when the
/// connection is back on the base scope (the `files` view = base rows), so it never sees overlay
/// chunks; reconciling here is the only point the overlay scope is active (#219 review). Embeddings
/// for a NEW/MODIFIED overlay chunk are written keyed by chunk id (shared `chunk_embeddings`
/// table), which the overlay scope reads through its own `files` view. `None` skips overlay
/// reconcile (the caller has no embedder/options).
///
/// `scope` (#577): which worktrees to refresh. The watcher's event-driven passes name the
/// checkouts events came from; everything else (`All`) sweeps the fleet. A worktree outside the
/// scope is still refreshed when its recorded basis — the (base HEAD, linked HEAD) pair its
/// overlay was last computed against — no longer matches: a BASE commit moves the diff basis for
/// every worktree at once, and a LINKED commit arrives with no file event for the checkout (hooks,
/// or edits made while no watcher ran). Working-tree edits in a skipped worktree surface via
/// events, or at latest via the next `All` sweep (`periodic_sweep_secs`) — the same missed-event
/// backstop the watcher already relies on.
///
/// `pub` so the hook-driven CLI `maintenance` command shares this exact path: the git hooks invoke
/// `rag-rat maintenance` (not the foreground watcher), so without calling this a commit/checkout/
/// merge in a linked worktree would index the base `config.root` but leave that worktree's overlay
/// stale until a watcher pass or a manual `index --worktree` (#219 review).
pub fn refresh_worktree_overlays(
    db: &mut IndexDatabase,
    config: &Config,
    reconcile: Option<&ReconcileBudget>,
    scope: &OverlayScope,
) -> bool {
    let (_, worktrees) = crate::index::live_worktree_contexts(&config.root);
    // The base id is the ENCLOSING worktree root, not `config.root` itself — see
    // `enclosing_worktree_id` (a repo-SUBDIR `config.root` would otherwise mis-classify the main
    // checkout as a linked overlay and re-index it as one) (#219 review).
    let base_id = enclosing_worktree_id(&config.root);
    // The basis every refresh in this pass records: the base HEAD the overlay delta is computed
    // from. Read once — a base commit landing mid-pass records the pre-pass sha, which mismatches
    // on the next pass and re-refreshes (the safe direction).
    let base_sha = crate::index::head_sha(&config.root);
    let sweep = matches!(scope, OverlayScope::All);
    let mut changed = false;
    for worktree in worktrees {
        if worktree == base_id {
            continue; // the rooted checkout is the base scope, not an overlay
        }
        // The linked HEAD is read BEFORE the refresh, so a commit racing the refresh records the
        // pre-commit head — mismatching (and re-refreshing) next pass rather than skipping.
        let linked_head = crate::index::head_sha(Path::new(&worktree));
        if !scope.lists(&worktree)
            && db.worktree_overlay_basis(&worktree).ok().flatten()
                == Some((base_sha.clone(), linked_head.clone()))
        {
            continue; // not implicated by events and the diff basis is unchanged (#577)
        }
        // Refresh the overlay with the LINKED worktree's OWN config targets, not the sweeping
        // process's. A branch whose `rag-rat.toml` ADDS a target (e.g. `extra/`) would otherwise be
        // filtered against the sweeper's targets, and a complete-status pass would PRUNE the
        // overlay rows a branch-launched hook indexed for it. `for_linked_worktree_overlay`
        // keeps the shared base `root`/`database` but swaps in the branch's target set
        // (#219 review).
        let overlay_config = config.for_linked_worktree_overlay(Path::new(&worktree));
        match db.index_worktree_overlay(&overlay_config, Path::new(&worktree), &mut |_| {}) {
            Ok(report) => {
                let this_changed = report.indexed > 0 || report.tombstoned > 0 || report.pruned > 0;
                changed |= this_changed;
                match overlay_basis_action(&report) {
                    // Record the refresh basis so later scoped passes can prove "unchanged" from
                    // two head reads instead of re-computing the delta (#577). Best-effort: a
                    // failed write just means the next pass refreshes again.
                    OverlayBasisAction::Record => {
                        let _ =
                            db.record_worktree_overlay_basis(&worktree, &base_sha, &linked_head);
                    },
                    OverlayBasisAction::Clear => {
                        let _ = db.clear_worktree_overlay_basis(&worktree);
                    },
                    OverlayBasisAction::Keep => {},
                }
                // Embed the overlay's chunks NOW, while the connection is still scoped to this
                // overlay (index_worktree_overlay left it there) — the trailing base reconcile
                // won't see them (#219 review). Run when the overlay CHANGED, OR — on an `All`
                // sweep only — when it has a BACKLOG of un-embedded chunks: an earlier pass's
                // inline reconcile may have returned `Partial` (the shared time budget ran out
                // mid-pass), leaving overlay chunks un-embedded. The next pass sees the overlay
                // rows as unchanged and would skip the embed forever, so a worktree-scoped
                // `semantic_search` would stay BM25-only for that branch content until an
                // unrelated file change; the sweep's `pending_embedding_jobs_with_options` count
                // (active overlay scope, SQL-only — no embedder acquisition, so an idle pass makes
                // no embed request) retries it within one `periodic_sweep_secs` (#219 review,
                // #577). A backlogged reconcile with the embedder unavailable defers inside
                // `reconcile_with_options_progress` itself (`provision_remote=false`).
                // `budget.next_options()` recomputes `max_seconds` from the time left in the
                // SHARED budget so overlays + base can't each spend the full `--max-seconds`;
                // `None` → budget exhausted, skip and let the NEXT pass retry.
                let needs_embed = overlay_needs_embed(this_changed, sweep, reconcile, |options| {
                    db.pending_embedding_jobs_with_options(options).is_ok_and(|pending| pending > 0)
                });
                if needs_embed
                    && let Some(budget) = reconcile
                    && let Some(options) = budget.next_options()
                    && let Err(err) = db.reconcile_with_options_progress(options, |_| {})
                {
                    eprintln!("watch: worktree overlay reconcile failed for {worktree}: {err}");
                }
            },
            Err(err) => {
                // A failed refresh may have left the overlay stale while both heads still match
                // (a dirty edit moves no HEAD) — drop the skip proof so scoped passes keep
                // refreshing this worktree until a pass completes (#577 review).
                let _ = db.clear_worktree_overlay_basis(&worktree);
                eprintln!("watch: worktree overlay refresh failed for {worktree}: {err}");
            },
        }
    }
    // Restore the base scope for the rest of the pass (index_worktree_overlay leaves the connection
    // scoped to the last worktree it touched).
    let _ = db.use_worktree_scope(&config.root, None);
    changed
}

/// What a refresh outcome does to the worktree's skip-proof basis (#577).
#[derive(Debug, PartialEq, Eq)]
enum OverlayBasisAction {
    /// A COMPLETE refresh of a real linked sibling: the heads captured around it prove the
    /// overlay current.
    Record,
    /// A PARTIAL refresh (the working-tree status read failed midway): dirty/untracked/deleted
    /// paths may be missing while neither HEAD moved, so a previously recorded basis would keep
    /// matching and scoped passes would skip the stale overlay until an `All` pass. Drop it so
    /// they keep refreshing until a complete pass lands.
    Clear,
    /// Not a linked sibling — there is no overlay to prove anything about.
    Keep,
}

fn overlay_basis_action(report: &crate::index::WorktreeOverlayReport) -> OverlayBasisAction {
    if report.worktree_id.is_empty() {
        OverlayBasisAction::Keep
    } else if report.status_complete {
        OverlayBasisAction::Record
    } else {
        OverlayBasisAction::Clear
    }
}

fn overlay_needs_embed(
    this_changed: bool,
    sweep_backlog_probe: bool,
    reconcile: Option<&ReconcileBudget>,
    pending_embedding_jobs: impl FnOnce(&ReconcileOptions) -> bool,
) -> bool {
    if this_changed {
        return true;
    }
    // The backlog probe is an O(scope) candidate scan; it belongs to the `All` sweep, not to
    // every event-scoped pass over an unchanged worktree (#577). A `Partial` drain therefore
    // heals within one `periodic_sweep_secs` instead of being re-probed per pass.
    if !sweep_backlog_probe {
        return false;
    }
    reconcile
        .and_then(ReconcileBudget::next_options)
        .is_some_and(|options| pending_embedding_jobs(&options))
}

/// A time budget shared across the per-overlay embedding reconciles AND the trailing base reconcile
/// of one maintenance/watcher pass. Each reconcile call starts its own `Instant` timer against its
/// `max_seconds`, so handing every overlay (and the base) the same `ReconcileOptions` would let
/// each spend the FULL advertised budget — N overlays + base = (N+1)×`max_seconds` of held write
/// lock. `next_options` recomputes `max_seconds` from the time remaining since `start`, so the
/// whole pass stays within a single budget (#219 review). A budget with no `max_seconds` cap
/// (`None`) is unbounded and every `next_options` returns the base options unchanged.
pub struct ReconcileBudget {
    options: ReconcileOptions,
    start: Instant,
    total_seconds: Option<u64>,
}

impl ReconcileBudget {
    /// Build a shared budget. `start` is the pass's clock origin — pass the SAME instant the
    /// surrounding command measured its own setup against (so discovery time already spent counts
    /// toward the budget); the per-call `max_seconds` is `options.max_seconds` minus elapsed.
    pub fn new(options: ReconcileOptions, start: Instant) -> Self {
        let total_seconds = options.max_seconds;
        Self { options, start, total_seconds }
    }

    /// The options for the NEXT reconcile in this pass, with `max_seconds` reduced to the time left
    /// in the shared budget. `None` when the budget is exhausted (so the caller skips the reconcile
    /// entirely rather than running it with a zero budget). An uncapped budget always yields the
    /// base options.
    pub fn next_options(&self) -> Option<ReconcileOptions> {
        let Some(total) = self.total_seconds else {
            return Some(self.options.clone());
        };
        let remaining = total.saturating_sub(self.start.elapsed().as_secs());
        if remaining == 0 {
            return None;
        }
        let mut options = self.options.clone();
        options.max_seconds = Some(remaining);
        Some(options)
    }
}

/// Whether an event KIND should ever fire a pass. Only content mutations do — `Create`, `Remove`,
/// any `Modify` (data/metadata/rename), and a write-close. Reads must NOT: notify's inotify mask
/// includes `IN_OPEN`/`IN_CLOSE_NOWRITE`, so opening/reading a watched file emits `Access(Open)` /
/// `Access(Close(Read))` events. Treating those as relevant created a feedback loop — the index
/// pass's own file reads (and the MCP serving queries, and the grep-augment hook reading source)
/// re-fired the watcher endlessly, re-indexing every couple seconds with `content_revision`
/// unchanged. Those read events stack in the notify→watcher channel and keep the debounce armed.
fn kind_is_mutation(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => true,
        // A close-after-write signals a real write; open / read / read-close do not.
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) | EventKind::Other | EventKind::Any => false,
    }
}

/// Directories whose `.gitignore` the watcher must subscribe to so a root-rule edit is *delivered*
/// (round-3 finding 1). The target dirs the watcher already watches recursively are *below*
/// `config.root`; when `config.root` is itself a subdirectory of a larger Git worktree, the
/// worktree-root `.gitignore` (and any ancestor `.gitignore` down to `config.root`) sits ABOVE them
/// and would otherwise never produce an event. This returns the ancestor chain from the worktree
/// root down to and including `config.root`. In a non-git tree (or when `config.root` *is* the
/// worktree root) it returns just `config.root` — already covered by the recursive target watches,
/// so the extra non-recursive watch is a harmless no-op duplicate.
fn gitignore_watch_dirs(root: &Path) -> Vec<PathBuf> {
    // Decide the worktree-root ancestor on canonicalized forms but keep `base` in `root`'s own
    // representation — gix's workdir isn't canonicalized, so on Windows a raw `starts_with` fails
    // when `root` is verbatim (`\\?\C:\…`) and the workdir is plain (`C:\…`). See ignore_rules.
    let base = crate::index::git_history::worktree_root(root)
        .and_then(|wt| crate::index::ignore_rules::base_under_worktree(root, &wt));
    let mut dirs = Vec::new();
    if let Some(base) = base
        && let Ok(rel) = root.strip_prefix(&base)
    {
        let mut dir = base;
        dirs.push(dir.clone());
        for component in rel.components() {
            if let std::path::Component::Normal(name) = component {
                dir.push(name);
                dirs.push(dir.clone());
            }
        }
    } else {
        dirs.push(root.to_path_buf());
    }
    dirs
}

/// Whether `path`'s file name is `.gitignore`. A mutation to any `**/.gitignore` changes the
/// repo's ignore rules, so it is a relevant event (issue #62 / PR #66 finding 4) even though
/// `.gitignore` is not a configured target language — the pass it fires recompiles the matcher and
/// re-discovers (dropping newly-ignored files, adding newly-unignored ones).
fn is_gitignore_path(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == ".gitignore")
}

/// A repo-config (`rag-rat.toml`) path. A LINKED checkout's config edit re-targets its branch
/// overlay without moving either HEAD, so the linked classifier fires on it like a `.gitignore`
/// edit (#577 review).
fn is_repo_config_path(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "rag-rat.toml")
}

/// A relevant event is a rescan/overflow notice, a *content-mutating* `.gitignore` edit (the rules
/// themselves changed — finding 4), or a *content-mutating* event whose path matches a configured
/// target and is **not** ignored by the repo's compiled `.gitignore` rules + floor (issue #62).
/// Classification only decides *whether to fire a pass*, not what to index (the discover sweep does
/// that). The walker and watcher share one [`IgnoreMatcher`] so a path the walker won't index also
/// won't fire a re-index here — no drift. Read-only events never fire regardless of path (see
/// [`kind_is_mutation`]).
///
/// The event path may be a just-deleted file, so dir-ness can't be stat'd; we pass `is_dir = false`
/// to the gitignore match. That is sound for the floor (component-name check is dir-ness-agnostic)
/// and for file globs; the only gap is a `foo/`-dir-only gitignore rule on a removed directory,
/// which at worst fires one extra harmless idempotent pass.
fn event_is_relevant(config: &Config, ignore: &IgnoreMatcher, event: &Event) -> bool {
    if event.need_rescan() {
        return true;
    }
    if !kind_is_mutation(&event.kind) {
        return false;
    }
    event.paths.iter().any(|path| {
        // A `.gitignore` edit changes the rules; it must fire a pass even though it is not a
        // target file and may itself sit in a directory the *current* rules ignore.
        if is_gitignore_path(path) {
            return true;
        }
        if ignore.is_ignored(path, false) {
            return false;
        }
        path.strip_prefix(&config.root)
            .ok()
            .is_some_and(|relative| target_for_path(config, relative).is_some())
    })
}

#[derive(Debug)]
struct LinkedWorktreeWatch {
    checkout_root: PathBuf,
    config: Config,
    target_dirs: Vec<PathBuf>,
    ignore: IgnoreMatcher,
}

impl LinkedWorktreeWatch {
    fn new(base_config: &Config, checkout_root: PathBuf) -> Self {
        // Overlay indexing deliberately keeps the base root/database while swapping in the linked
        // branch's targets. Watch placement needs those branch targets, but paths must be matched
        // against the linked checkout's equivalent of `base_config.root`.
        let mut config = base_config.for_linked_worktree_overlay(&checkout_root);
        config.root = checkout_root.join(config_subdir_prefix(base_config));
        let target_dirs = config.target_directories();
        let ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
        Self { checkout_root, config, target_dirs, ignore }
    }

    fn place_watches(&self, watcher: &mut impl notify::Watcher) {
        watch_configured_trees(watcher, &self.config, &self.target_dirs, &self.ignore);
        watch_gitignore_rule_dirs(watcher, &self.config.root, &self.target_dirs);
        for root in missing_config_root_bootstrap_dirs(&self.config.root, &self.checkout_root) {
            // The linked checkout may not have this subdir on the current branch yet. Keep narrow
            // bootstraps on the existing ancestor chain so the next recreated config-root component
            // is observed without returning to a recursive whole-checkout watch.
            let _ = watcher.watch(&root, RecursiveMode::NonRecursive);
        }
    }

    fn recompile_ignore_and_place_watches(&mut self, watcher: &mut impl notify::Watcher) {
        self.ignore = IgnoreMatcher::compile(&self.config.root, &self.target_dirs);
        self.place_watches(watcher);
    }

    fn watch_created_dirs(&mut self, watcher: &mut impl notify::Watcher, event: &Event) -> bool {
        watch_created_dirs(
            watcher,
            event,
            &self.config,
            &self.target_dirs,
            &mut self.ignore,
            Some(&self.checkout_root),
        )
    }

    fn touches_event(&self, path: &Path) -> bool {
        // A `.gitignore` edit anywhere in the linked checkout can affect the branch overlay's
        // ignored/unignored set, including ancestor rules above a subdir-rooted config. A
        // `rag-rat.toml` edit likewise changes the branch's TARGET SET without moving either HEAD
        // (#577 review) — the overlay refresh re-reads the branch config each pass, so firing the
        // pass is what picks the new targets up.
        if (is_gitignore_path(path) || is_repo_config_path(path))
            && path.starts_with(&self.checkout_root)
        {
            return true;
        }
        if self.ignore.is_ignored(path, false) {
            return false;
        }
        path.strip_prefix(&self.config.root)
            .ok()
            .is_some_and(|rel| target_for_path(&self.config, rel).is_some())
    }
}

#[derive(Debug, Default)]
struct LinkedWorktreeWatches {
    states: Vec<LinkedWorktreeWatch>,
}

impl LinkedWorktreeWatches {
    fn sync(
        &mut self,
        watcher: &mut impl notify::Watcher,
        base_config: &Config,
        checkout_roots: Vec<PathBuf>,
    ) {
        let mut states = Vec::with_capacity(checkout_roots.len());
        for root in checkout_roots {
            let state = LinkedWorktreeWatch::new(base_config, root);
            state.place_watches(watcher);
            states.push(state);
        }
        self.states = states;
    }

    /// Place watches for created/moved-in dirs across EVERY state (no short-circuit — placement
    /// is a side effect each state needs), returning the checkout roots that placed one, so the
    /// armed pass can scope its overlay refresh to them (#577).
    fn watch_created_dirs(
        &mut self,
        watcher: &mut impl notify::Watcher,
        event: &Event,
    ) -> BTreeSet<PathBuf> {
        let mut placed = BTreeSet::new();
        for state in &mut self.states {
            if state.watch_created_dirs(watcher, event) {
                placed.insert(state.checkout_root.clone());
            }
        }
        placed
    }

    fn recompile_ignore_and_place_watches(&mut self, watcher: &mut impl notify::Watcher) {
        for state in &mut self.states {
            state.recompile_ignore_and_place_watches(watcher);
        }
    }

    fn event_touches(&self, event: &Event, registry: Option<&Path>) -> WorktreeEventHint {
        if event.need_rescan() {
            // Events were dropped — anything could have changed anywhere.
            return if !self.states.is_empty() || registry.is_some() {
                WorktreeEventHint::AllWorktrees
            } else {
                WorktreeEventHint::None
            };
        }
        if !kind_is_mutation(&event.kind) {
            return WorktreeEventHint::None;
        }
        let mut roots = BTreeSet::new();
        for path in &event.paths {
            if registry.is_some_and(|reg| path.starts_with(reg)) {
                // A worktree add/remove: the live set itself changed — unattributable.
                return WorktreeEventHint::AllWorktrees;
            }
            for state in self.states.iter().filter(|state| state.touches_event(path)) {
                roots.insert(state.checkout_root.clone());
            }
        }
        if roots.is_empty() { WorktreeEventHint::None } else { WorktreeEventHint::Roots(roots) }
    }
}

/// Overlay implication of one event for the LINKED-worktree layer (#577): which checkouts the
/// armed pass must refresh — or `AllWorktrees` when the event can't be attributed (a backend
/// rescan, a worktree-registry change).
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeEventHint {
    /// No linked checkout is touched.
    None,
    /// The event touches these linked checkout roots.
    Roots(BTreeSet<PathBuf>),
    /// Unattributable — refresh every overlay.
    AllWorktrees,
}

impl WorktreeEventHint {
    /// Whether the linked-worktree layer wants a pass at all.
    fn fires(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Whether `event` should fire a pass for the LINKED-worktree layer (#219): a content mutation to a
/// configured target inside a linked worktree checkout (its overlay needs refreshing), or any
/// change in the worktree registry (`<common_dir>/worktrees`, i.e. a worktree add/remove). Separate
/// from [`event_is_relevant`] so the base-tree classification — and its tests — stay untouched.
/// Each linked checkout has its own target set and ignore matcher, so the watcher does not fire on
/// paths the overlay walker would drop (including `target/` or branch-local gitignored dirs).
fn event_touches_worktree(
    event: &Event,
    worktrees: &LinkedWorktreeWatches,
    registry: Option<&Path>,
) -> WorktreeEventHint {
    worktrees.event_touches(event, registry)
}

/// `config.root` relative to its own checkout's worktree root — the SUBDIR prefix to strip off a
/// linked checkout's event paths before applying `target_for_path` (config-root-relative). Empty
/// when `config.root` IS the worktree root (the common case) or in a non-git tree.
fn config_subdir_prefix(config: &Config) -> PathBuf {
    crate::index::git_history::worktree_root(&config.root)
        // base_under_worktree returns the worktree root in config.root's representation, so the
        // strip_prefix below succeeds on Windows (verbatim vs plain prefix) — see ignore_rules.
        .and_then(|wt| crate::index::ignore_rules::base_under_worktree(&config.root, &wt))
        .and_then(|base| config.root.strip_prefix(&base).ok().map(Path::to_path_buf))
        .unwrap_or_default()
}

fn path_between_bootstrap_and_config_root(
    path: &Path,
    bootstrap_root: &Path,
    config_root: &Path,
) -> bool {
    path.starts_with(bootstrap_root) && config_root.starts_with(path)
}

fn missing_config_root_bootstrap_dirs(config_root: &Path, checkout_root: &Path) -> Vec<PathBuf> {
    if config_root == checkout_root
        || !path_between_bootstrap_and_config_root(checkout_root, checkout_root, config_root)
        || config_root.exists()
        || !checkout_root.is_dir()
    {
        return Vec::new();
    }
    let Ok(rel) = config_root.strip_prefix(checkout_root) else {
        return Vec::new();
    };

    let mut dirs = vec![checkout_root.to_path_buf()];
    let mut dir = checkout_root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(name) => {
                dir.push(name);
                if dir == config_root || !dir.is_dir() {
                    break;
                }
                dirs.push(dir.clone());
            },
            Component::CurDir => {},
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => break,
        }
    }
    dirs
}

/// The `worktree_id` of the worktree that ENCLOSES `root`, canonicalized to match the ids
/// `live_worktree_contexts` reports. When `root` is a repo SUBDIR (`<repo>/crate`), the enclosing
/// worktree root is `<repo>` — which is the spelling the main checkout contributes to
/// `live_worktree_contexts`. Filtering live worktrees by `worktree_id_of(root)` (the subdir path)
/// instead would never match that entry, so the main checkout would be misread as a LINKED overlay.
/// Falls back to `root`'s own id outside a git worktree (#219 review).
fn enclosing_worktree_id(root: &Path) -> String {
    crate::index::git_history::worktree_root(root)
        .map_or_else(|| crate::index::worktree_id_of(root), |wt| crate::index::worktree_id_of(&wt))
}

/// Live linked-worktree checkout roots (excluding the base `config.root`) plus the worktree
/// registry dir (`<common_dir>/worktrees`), for the watcher to subscribe to — branch checkouts for
/// edits, the registry for add/remove.
fn worktree_watch_targets(config: &Config) -> (Vec<PathBuf>, Option<PathBuf>) {
    let (_, worktrees) = crate::index::live_worktree_contexts(&config.root);
    // The base id is the ENCLOSING worktree root, not `config.root` itself. When `config.root` is a
    // repo SUBDIR (`<repo>/crate`), `live_worktree_contexts` reports the main checkout as `<repo>`
    // (its workdir), but `worktree_id_of(config.root)` is `<repo>/crate` — they wouldn't match, so
    // the main checkout would be treated as a linked worktree and the watcher would recursively
    // subscribe to the whole repo root outside the configured target (#219 review).
    let base = enclosing_worktree_id(&config.root);
    let roots = worktrees.into_iter().filter(|w| *w != base).map(PathBuf::from).collect();
    let registry = crate::index::discover_repo(&config.root)
        .ok()
        .map(|repo| repo.common_dir().join("worktrees"));
    (roots, registry)
}

fn watch_configured_trees(
    watcher: &mut impl notify::Watcher,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &IgnoreMatcher,
) {
    for dir in target_dirs {
        watch_tree_pruned(watcher, &config.root.join(dir), ignore);
    }
}

fn gitignore_rule_watch_dirs(root: &Path, target_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut dirs = Vec::new();
    for dir in gitignore_watch_dirs(root) {
        if seen.insert(dir.clone()) {
            dirs.push(dir);
        }
    }
    for dir in target_ancestor_dirs(root, target_dirs) {
        if seen.insert(dir.clone()) {
            dirs.push(dir);
        }
    }
    dirs
}

fn watch_gitignore_rule_dirs(
    watcher: &mut impl notify::Watcher,
    root: &Path,
    target_dirs: &[PathBuf],
) {
    for dir in gitignore_rule_watch_dirs(root, target_dirs) {
        let _ = watcher.watch(&dir, RecursiveMode::NonRecursive);
    }
}

fn watch_linked_worktrees(
    watcher: &mut impl notify::Watcher,
    base_config: &Config,
    checkout_roots: Vec<PathBuf>,
) -> LinkedWorktreeWatches {
    let mut worktrees = LinkedWorktreeWatches::default();
    worktrees.sync(watcher, base_config, checkout_roots);
    worktrees
}

/// Whether `event` touches the installed binary path — the fleet hot-upgrade trigger. Matches by
/// full path (`cargo install` renames its temp file to exactly this path) so unrelated churn in
/// the same directory is ignored.
fn event_targets_binary(fleet_bin: Option<&Path>, event: &Event) -> bool {
    let Some(bin) = fleet_bin else {
        return false;
    };
    event.paths.iter().any(|path| path == bin)
}

/// A running watcher. Dropping it signals the thread to stop and joins it.
#[derive(Debug)]
pub struct Watcher {
    stop: Arc<AtomicBool>,
    wake: Sender<LoopMsg>,
    handle: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Start the watcher unless disabled by config or `RAG_RAT_NO_WATCH`. The returned watcher must
    /// be kept alive; dropping it stops the thread. Returns `None` when watching is disabled.
    pub fn spawn(config: Config) -> Option<Watcher> {
        Self::spawn_with_fleet(config, None)
    }

    /// Like [`Watcher::spawn`], but when `fleet_bin` is the installed-binary path, the elected
    /// watcher also watches that file's directory and signals the hot-upgrade fleet (see
    /// [`crate::fleet`]) when a new binary lands. Only the MCP server — which has a `SIGUSR1`
    /// handler — passes `Some`.
    pub fn spawn_with_fleet(config: Config, fleet_bin: Option<PathBuf>) -> Option<Watcher> {
        if !config.watch.enabled || std::env::var_os("RAG_RAT_NO_WATCH").is_some() {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        // The loop channel is created here (not in `watcher_main`) so Drop holds a sender and can
        // wake the loop out of its recv wait the moment `stop` is set.
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("rag-rat-watch".to_string())
            .spawn({
                let stop = Arc::clone(&stop);
                let tx = tx.clone();
                move || watcher_main(config, fleet_bin, &stop, tx, rx)
            })
            .ok()?;
        Some(Watcher { stop, wake: tx, handle: Some(handle) })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Without the wake, an idle loop observes `stop` only at its next recv timeout — up to
        // the periodic-sweep interval — long past the host's post-EOF kill grace.
        let _ = self.wake.send(LoopMsg::Wake);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn sleep_checking_stop(total: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(200);
    let mut waited = Duration::ZERO;
    while waited < total {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(step.min(total - waited));
        waited += step;
    }
}

/// Place a NON-recursive watch on `dir` and every non-ignored directory beneath it, pruning ignored
/// subtrees (issue #331). Replaces a single `RecursiveMode::Recursive` target watch, which descends
/// into gitignored build/dependency dirs (`node_modules`, `target`, …) and can exhaust
/// `fs.inotify.max_user_watches`. `ignore` is the same matcher `event_is_relevant` classifies with,
/// so the watched set matches the indexed set — a directory whose events the watcher would discard
/// never gets a watch in the first place. Best-effort: a dir that fails to watch (e.g. removed
/// mid-walk, or the watch budget is already exhausted) is skipped, not propagated.
fn watch_tree_pruned(watcher: &mut impl notify::Watcher, dir: &Path, ignore: &IgnoreMatcher) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if watcher.watch(&d, RecursiveMode::NonRecursive).is_err() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            // `DirEntry::file_type` does NOT follow symlinks (unlike `fs::metadata`): a
            // symlink-to-dir reports `is_dir() == false` here, so the recursion never descends
            // through a link and can't place watches outside `config.root` (#332). Matches the
            // index walker, which also skips symlinks.
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && !ignore.is_ignored(&p, true)
            {
                stack.push(p);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreatedDirPlacement {
    OutsideTargets,
    TargetAncestor,
    TargetSubtree,
}

/// How a newly appeared directory relates to the configured target set. `config.root` itself is
/// watched non-recursively (the `gitignore_watch_dirs` ancestor chain), so a top-level create
/// OUTSIDE a target — `vendor/` or a sibling of `src/` — is also delivered to the event loop;
/// without this gate [`watch_created_dirs`] would watch it even though it can never be indexed,
/// re-exhausting inotify (#332). A linked checkout can also bootstrap an absent subdir-rooted
/// `config.root`: the checkout root and any newly-created ancestors between it and `config.root`
/// are target ancestors, not indexable subtrees. A whole-root target (`directories = ["."]`) treats
/// every directory below `config.root` as a target subtree.
fn created_dir_placement(
    config: &Config,
    target_dirs: &[PathBuf],
    path: &Path,
    bootstrap_root: Option<&Path>,
) -> CreatedDirPlacement {
    let Ok(rel) = path.strip_prefix(&config.root) else {
        if let Some(root) = bootstrap_root
            && path_between_bootstrap_and_config_root(path, root, &config.root)
        {
            return CreatedDirPlacement::TargetAncestor;
        }
        return CreatedDirPlacement::OutsideTargets;
    };
    if rel.as_os_str().is_empty() {
        return CreatedDirPlacement::TargetAncestor;
    }
    if target_dirs.iter().any(|d| d == Path::new(".") || rel.starts_with(d)) {
        return CreatedDirPlacement::TargetSubtree;
    }
    if !rel.as_os_str().is_empty()
        && target_dirs.iter().any(|d| d != Path::new(".") && d.starts_with(rel))
    {
        return CreatedDirPlacement::TargetAncestor;
    }
    CreatedDirPlacement::OutsideTargets
}

/// Place a pruned watch on any newly-appeared, in-target, non-ignored *real* directory among an
/// event's paths (issue #331/#332). Target dirs are watched non-recursively, so notify will not
/// auto-watch a freshly-created subdir; without this, edits inside it would never fire a pass.
///
/// Each path is gated, in order, by:
/// 1. **real dir** — `symlink_metadata` (which does NOT follow links) so a symlink-to-dir is
///    `is_dir() == false` and skipped. Following it would let `watch_tree_pruned` recurse OUTSIDE
///    `config.root` (e.g. a link to a dep cache) and re-exhaust inotify; the index walker likewise
///    skips symlinks. Dir-ness comes from the filesystem, not the notify `CreateKind`, because the
///    inotify backend commonly reports `CreateKind::Any`.
/// 2. **target relation** — [`created_dir_placement`]; a non-target top-level dir can never be
///    indexed, while a newly-created ancestor of a nested target (`src/` for `src/generated`) must
///    be watched non-recursively so the later target creation is delivered.
/// 3. **not ignored for target subtrees** — by the current matcher.
/// 4. **recompile + re-check** — a directory MOVED in can carry its own nested `.gitignore`, which
///    the long-lived matcher (compiled before that subtree existed) doesn't know about; recompiling
///    here picks the nested rules up so `watch_tree_pruned` prunes against them, not stale rules.
fn watch_created_dirs(
    watcher: &mut impl notify::Watcher,
    event: &Event,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &mut IgnoreMatcher,
    bootstrap_root: Option<&Path>,
) -> bool {
    // A directory APPEARS either as a Create or — when moved/renamed INTO a watched target
    // (`mv /tmp/pkg src/pkg`) — as a name Modify (`RenameMode::To`/`Both`) (#332). Both need a
    // fresh pruned watch, because the parent is watched NonRecursive (#331) so notify won't
    // auto-descend.
    let dir_appeared = matches!(event.kind, EventKind::Create(_))
        || matches!(
            event.kind,
            EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both))
        );
    if !dir_appeared {
        return false;
    }
    let mut placed_target_watch = false;
    for path in &event.paths {
        // `symlink_metadata` does NOT follow links: a symlink pointing at a directory is reported
        // as a symlink (`is_dir() == false`) → skipped, so we never recurse through it (#332).
        let is_real_dir = std::fs::symlink_metadata(path).map(|m| m.is_dir()).unwrap_or(false);
        if !is_real_dir {
            continue;
        }
        match created_dir_placement(config, target_dirs, path, bootstrap_root) {
            CreatedDirPlacement::OutsideTargets => continue,
            CreatedDirPlacement::TargetAncestor => {
                // A nested target's leading directory can appear after startup. Watch the ancestor
                // non-recursively so the eventual target dir creation is delivered, and re-place
                // target watches too in case a fast `mkdir -p target/path` already created it.
                let _ = watcher.watch(path, RecursiveMode::NonRecursive);
                watch_gitignore_rule_dirs(watcher, &config.root, target_dirs);
                *ignore = IgnoreMatcher::compile(&config.root, target_dirs);
                watch_configured_trees(watcher, config, target_dirs, ignore);
                placed_target_watch = true;
            },
            CreatedDirPlacement::TargetSubtree => {
                if ignore.is_ignored(path, true) {
                    continue;
                }
                // A moved-in dir may carry a nested `.gitignore` the long-lived matcher predates;
                // recompile so the subtree is pruned against current (incl. nested) rules, then
                // re-check the root.
                *ignore = IgnoreMatcher::compile(&config.root, target_dirs);
                if ignore.is_ignored(path, true) {
                    continue;
                }
                watch_tree_pruned(watcher, path, ignore);
                placed_target_watch = true;
            },
        }
    }
    placed_target_watch
}

fn place_initial_watch_state(
    watcher: &mut impl notify::Watcher,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &IgnoreMatcher,
    fleet_bin: Option<&Path>,
) -> (LinkedWorktreeWatches, Option<PathBuf>) {
    watch_configured_trees(watcher, config, target_dirs, ignore);
    watch_gitignore_rule_dirs(watcher, &config.root, target_dirs);

    if let Some(dir) = fleet_bin.and_then(Path::parent) {
        let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
    }

    let (linked_worktree_roots, worktree_registry) = worktree_watch_targets(config);
    let linked_worktrees = watch_linked_worktrees(watcher, config, linked_worktree_roots);
    if let Some(registry) = &worktree_registry {
        let _ = watcher.watch(registry, RecursiveMode::NonRecursive);
    }
    (linked_worktrees, worktree_registry)
}

/// Whether `event` should arm a maintenance pass — and if so, which linked-worktree overlays it
/// implicates (#577): `Some(scope)` to fire (an empty `Linked` set is a base-only event — the
/// pass's discover covers the base scope regardless), `None` to ignore. Every sub-check still
/// RUNS unconditionally (no short-circuit): watch placement is a side effect both the base and
/// every linked state need regardless of what else already fired.
fn event_requests_maintenance(
    watcher: &mut impl notify::Watcher,
    event: &Event,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &mut IgnoreMatcher,
    linked_worktrees: &mut LinkedWorktreeWatches,
    worktree_registry: Option<&Path>,
) -> Option<OverlayScope> {
    let created_dir_watch_placed =
        watch_created_dirs(watcher, event, config, target_dirs, ignore, None);
    let linked_created_dir_roots = linked_worktrees.watch_created_dirs(watcher, event);
    let base_relevant = event_is_relevant(config, ignore, event);
    let worktree_hint = event_touches_worktree(event, linked_worktrees, worktree_registry);
    let fires = created_dir_watch_placed
        || !linked_created_dir_roots.is_empty()
        || base_relevant
        || worktree_hint.fires();
    if !fires {
        return None;
    }
    // A rescan means the backend dropped events — `event_is_relevant` fires for the base and the
    // overlay side can't attribute anything, so the pass sweeps everything.
    if event.need_rescan() {
        return Some(OverlayScope::All);
    }
    let scope = match worktree_hint {
        WorktreeEventHint::AllWorktrees => OverlayScope::All,
        WorktreeEventHint::Roots(roots) => OverlayScope::Linked(roots),
        WorktreeEventHint::None => OverlayScope::Linked(BTreeSet::new()),
    };
    Some(scope.merge(OverlayScope::Linked(linked_created_dir_roots)))
}

fn recompile_ignore_and_place_watches(
    watcher: &mut impl notify::Watcher,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &mut IgnoreMatcher,
    linked_worktrees: &mut LinkedWorktreeWatches,
) {
    *ignore = IgnoreMatcher::compile(&config.root, target_dirs);
    watch_configured_trees(watcher, config, target_dirs, ignore);
    linked_worktrees.recompile_ignore_and_place_watches(watcher);
}

fn sync_linked_worktrees_after_pass(
    watcher: &mut impl notify::Watcher,
    config: &Config,
    linked_worktrees: &mut LinkedWorktreeWatches,
) {
    let (current, _) = worktree_watch_targets(config);
    linked_worktrees.sync(watcher, config, current);
}

fn watcher_main(
    config: Config,
    fleet_bin: Option<PathBuf>,
    stop: &AtomicBool,
    tx: Sender<LoopMsg>,
    rx: Receiver<LoopMsg>,
) {
    let base_dir =
        config.database.parent().map(Path::to_path_buf).unwrap_or_else(|| config.root.clone());
    let election_path = locks::election_lock_path(&base_dir, &config.root);

    // Win election (one watcher per worktree); retry so a new watcher takes over if a holder dies.
    let _election = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match FileLock::try_acquire(&election_path) {
            Ok(Some(lock)) => break lock,
            _ => sleep_checking_stop(Duration::from_secs(5), stop),
        }
    };

    let Ok(mut notify_watcher) = recommended_watcher({
        let tx = tx.clone();
        move |res| {
            let _ = tx.send(LoopMsg::Fs(res));
        }
    }) else {
        return;
    };
    // Compile the repo's `.gitignore` rules for event classification — the same matcher the
    // discover walk uses (issue #62), so an ignored path never fires a pass. Recompiled whenever a
    // `.gitignore` mutation is observed (finding 3) so the running classifier never applies stale
    // rules; each pass's discover walk also compiles its own fresh matcher, so the index itself is
    // always current regardless. Compiled BEFORE the target watches so placement can prune ignored
    // subtrees (issue #331) — not just classification.
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    // Watch each target dir and its non-ignored subtree NON-recursively (issue #331). notify's
    // `RecursiveMode::Recursive` places an inotify watch on EVERY subdirectory — including
    // gitignored build/dependency trees (`node_modules`, `target`, …) — which on a large repo can
    // exhaust `fs.inotify.max_user_watches` (~65k observed on a held checkout), after which no new
    // watcher can start. `event_is_relevant` already drops events from ignored paths; placing the
    // watches the same way keeps the watch count proportional to the *indexed* tree, not the whole
    // working directory.
    // Round-3 finding 1: when `config.root` is a subdirectory of a larger Git worktree, the target
    // dirs are below `config.root`, so a watch scoped to them never sees an edit to the
    // *worktree-root* (or any ancestor) `.gitignore` that lives ABOVE them — those root-rule
    // changes would never recompile the matcher. Subscribe (non-recursively, to avoid
    // re-watching the whole worktree) to every directory on the ancestor chain from the
    // worktree root down to `config.root`, so a `.gitignore` mutation there is delivered.
    // `gitignore_watch_dirs` returns the chain (always including `config.root`); the
    // non-recursive watch only delivers events for files directly in each dir, which is exactly
    // where each ancestor `.gitignore` sits.
    //
    // Fleet hot-upgrade: also watch the installed binary's directory so a new `cargo install`
    // rename triggers a fleet-wide upgrade. Watch the directory (not the file) so the atomic
    // rename — which replaces the inode — is still observed.
    //
    // Linked worktrees (#219): watch each branch checkout's configured targets, pruned by that
    // checkout's own gitignore rules, so overlay refreshes do not subscribe to ignored build trees.
    // The worktree registry is still watched non-recursively so `git worktree add/remove` fires a
    // pass. `linked_worktrees` is reconciled after each pass to pick up newly-added worktrees.
    let (mut linked_worktrees, worktree_registry) = place_initial_watch_state(
        &mut notify_watcher,
        &config,
        &target_dirs,
        &ignore,
        fleet_bin.as_deref(),
    );

    // Maintenance passes run on the worker thread (#506); see `spawn_pass_worker` for why.
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let Some(pass_worker) = spawn_pass_worker(pass_rx, tx, {
        let config = config.clone();
        move |request| {
            let _ = match request {
                PassRequest::StartupCatchup => startup_catchup_pass(&config),
                PassRequest::Maintenance { run_gc, overlay_scope } =>
                    maintenance_pass_scoped(&config, *run_gc, overlay_scope),
            };
        }
    }) else {
        return;
    };

    // Catch-up pass: covers edits made while no watcher was running (startup / election gap).
    // Dispatched to the worker AFTER watch placement, so a change landing mid-catch-up is caught
    // by an event instead of falling into the old catch-up→placement blind window — including a
    // `cargo install` into the just-watched binary dir (#506). Do not force the reconcile/gc/
    // memory tail on every process start; on an unchanged checkout, startup must stay
    // discover-only and write nothing past any cheap freshness repairs.
    let mut scheduler = PassScheduler::new();
    let _ = pass_tx.send(scheduler.dispatch_startup());

    let mut fire_fleet_trigger = |bin: &Path| fleet::trigger(bin);
    let final_refresh_owed = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: fleet_bin.as_deref(),
        notify_watcher: &mut notify_watcher,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: worktree_registry.as_deref(),
        rx,
        pass_tx: &pass_tx,
        scheduler: &mut scheduler,
        stop,
        fleet_trigger: &mut fire_fleet_trigger,
    }
    .run();

    // Let an in-flight pass finish (bounded by the pass itself, exactly as when passes ran
    // inline), then release the worker.
    drop(pass_tx);
    let _ = pass_worker.join();

    // Final pass for edits in the last debounce window — discover only (no embedding), timeout-and-
    // skip. The host may SIGKILL shortly after stdin EOF, so shutdown must be bounded; discover is
    // fast and keeps structure fresh, and the next content-changing or periodic pass does the
    // embedding.
    if final_refresh_owed {
        let _ = shutdown_discover(&config);
    }
}

/// The watcher event loop, separated from [`watcher_main`] so tests can drive it with a recording
/// notify watcher and play the pass worker themselves. The invariant it exists to enforce (#506):
/// this thread NEVER runs a maintenance pass inline — it hands requests to the worker over
/// `pass_tx` and keeps classifying events and serving the fleet trigger while the pass runs.
struct EventLoop<'a, W: notify::Watcher> {
    config: &'a Config,
    target_dirs: &'a [PathBuf],
    fleet_bin: Option<&'a Path>,
    notify_watcher: &'a mut W,
    ignore: &'a mut IgnoreMatcher,
    linked_worktrees: &'a mut LinkedWorktreeWatches,
    worktree_registry: Option<&'a Path>,
    rx: Receiver<LoopMsg>,
    pass_tx: &'a Sender<PassRequest>,
    scheduler: &'a mut PassScheduler,
    stop: &'a AtomicBool,
    /// Injected so tests can observe the fleet firing; production wires [`fleet::trigger`].
    fleet_trigger: &'a mut (dyn FnMut(&Path) + Send),
}

impl<W: notify::Watcher> EventLoop<'_, W> {
    /// Run until `stop`; returns whether a final shutdown refresh is owed (the debounce is still
    /// armed — events arrived after the last dispatched pass).
    fn run(self) -> bool {
        let mut debounce = Debounce::new(
            Duration::from_millis(self.config.watch.debounce_ms),
            Duration::from_millis(self.config.watch.max_latency_ms),
        );
        let mut fleet_debounce = Debounce::new(FLEET_DEBOUNCE, FLEET_MAX_LATENCY);
        // Periodic backstop (covers event-blind filesystems + missed events). Counts from the
        // last ALL-scoped pass, so event-scoped passes can't postpone it (#577 review); `new`
        // assumes the startup catch-up (an `All` pass) was just dispatched.
        let periodic = (self.config.watch.periodic_sweep_secs > 0)
            .then(|| Duration::from_secs(self.config.watch.periodic_sweep_secs));
        let mut sweep = SweepClock::new(periodic, Instant::now());
        // The overlay scope accumulated while the debounce is armed (#577): every firing event
        // merges its contribution, and the union rides the next dispatched pass. Cleared ONLY on
        // dispatch, like the debounce itself — mid-pass events keep accumulating for the
        // coalesced follow-up.
        let mut pending_overlay_scope: Option<OverlayScope> = None;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            let now = Instant::now();
            // While a pass is in flight its fire condition stays due (the debounce only resets on
            // dispatch), so recomputing those deadlines would spin the loop — the `PassDone`
            // message is what wakes it. The fleet debounce still shapes the wait: the trigger
            // must fire DURING a long pass, not after it (#506).
            let wait = if self.scheduler.in_flight() {
                fleet_debounce.due_in(now).unwrap_or(IDLE_WAIT)
            } else {
                [debounce.due_in(now), fleet_debounce.due_in(now), sweep.due_in(now)]
                    .into_iter()
                    .flatten()
                    .min()
                    .unwrap_or(IDLE_WAIT)
            };
            match self.rx.recv_timeout(wait) {
                Ok(LoopMsg::Fs(Ok(event))) => {
                    let now = Instant::now();
                    // Place watches on newly-appeared, non-ignored directories REGARDLESS of
                    // relevance (#332). Target dirs are watched NON-recursively (#331), so notify
                    // won't auto-descend into a new subdir — and a bare `mkdir src/foo` is NOT
                    // `event_is_relevant` (a directory is extensionless → not a target FILE), so
                    // gating this on the relevance check below would leave the new dir unwatched
                    // and its files invisible until the periodic sweep (or forever, if it is
                    // disabled). A directory MOVED in (`mv pkg src/pkg`) arrives as a rename, not
                    // a Create — `watch_created_dirs` handles both. Its own cheap filter
                    // (Create/rename + is-dir + not-ignored) makes calling it on every
                    // event correct. When it places a watch, arm the debounce even if the
                    // directory event itself is extensionless and therefore not relevant: the
                    // newly-created dir can already contain files that need the pass to discover
                    // them. The watch keeps SUBSEQUENT edits firing. Gates each path on real-dir +
                    // target relation + not-ignored, and recompiles the matcher for a moved-in
                    // nested `.gitignore` (#332).
                    if let Some(scope) = event_requests_maintenance(
                        self.notify_watcher,
                        &event,
                        self.config,
                        self.target_dirs,
                        self.ignore,
                        self.linked_worktrees,
                        self.worktree_registry,
                    ) {
                        debounce.on_event(now);
                        pending_overlay_scope = Some(match pending_overlay_scope.take() {
                            Some(pending) => pending.merge(scope),
                            None => scope,
                        });
                        // A `.gitignore` mutation changed the rules — recompile so subsequent
                        // events are classified against current rules, not the matcher
                        // this watcher booted with.
                        if kind_is_mutation(&event.kind)
                            && event.paths.iter().any(|path| is_gitignore_path(path))
                        {
                            // PLACEMENT, not just classification, must track the new rules (#332):
                            // a removed ignore rule UN-ignores a subtree the startup walk skipped,
                            // so re-walk and add watches for it now — otherwise edits inside it
                            // never fire. notify's `watch()` is idempotent for an
                            // already-watched path, so re-walking only ADDS the
                            // newly-eligible dirs. (A newly-IGNORED subtree keeps its
                            // now-stale watches — harmless wasted watches; full unwatch
                            // bookkeeping is deferred.)
                            recompile_ignore_and_place_watches(
                                self.notify_watcher,
                                self.config,
                                self.target_dirs,
                                self.ignore,
                                self.linked_worktrees,
                            );
                        }
                    }
                    if event_targets_binary(self.fleet_bin, &event) {
                        fleet_debounce.on_event(now);
                    }
                },
                Ok(LoopMsg::Fs(Err(_))) => {},
                Ok(LoopMsg::PassDone) => {
                    self.scheduler.on_done();
                    sweep.on_pass_done(Instant::now());
                    // Refresh the live linked-worktree set after every pass. Existing checkout
                    // paths can switch branches and therefore branch-local target sets; rebuilding
                    // the state keeps classification, ignore matchers, and watch placement in one
                    // place. Removed checkout paths can still have stale backend watches
                    // (harmless; their overlay is GC-pruned), but no stale `LinkedWorktreeWatch`
                    // state remains active.
                    sync_linked_worktrees_after_pass(
                        self.notify_watcher,
                        self.config,
                        self.linked_worktrees,
                    );
                },
                Ok(LoopMsg::Wake) => {},
                Err(RecvTimeoutError::Timeout) => {},
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let now = Instant::now();
            let periodic_due = sweep.due(now);
            if debounce.should_fire(now) || periodic_due {
                // The periodic sweep is the missed-event backstop, so it refreshes every overlay;
                // an event-driven pass carries the roots accumulated above (#577). When both are
                // due at once, `All` is the superset — and because the sweep clock counts from
                // the last All COMPLETION, sustained event churn escalates a pass to `All` every
                // interval instead of postponing the backstop.
                let overlay_scope = if periodic_due {
                    OverlayScope::All
                } else {
                    pending_overlay_scope
                        .clone()
                        .unwrap_or_else(|| OverlayScope::Linked(BTreeSet::new()))
                };
                if let Some(request) = self.scheduler.dispatch(overlay_scope) {
                    // The scheduler may widen the scope (gc-cadence passes force `All`), so read
                    // the DISPATCHED request's scope, not the one handed in.
                    if let PassRequest::Maintenance { overlay_scope, .. } = &request {
                        sweep.on_dispatch(matches!(overlay_scope, OverlayScope::All));
                    }
                    let _ = self.pass_tx.send(request);
                    // Reset ONLY on dispatch: while a pass is in flight the armed debounce (and
                    // the scope accumulated with it) is the record that a follow-up is owed, and
                    // it fires as soon as `PassDone` lands.
                    debounce.reset();
                    pending_overlay_scope = None;
                }
            }
            if fleet_debounce.should_fire(now)
                && let Some(bin) = self.fleet_bin
            {
                // Signal the fleet (this process last) to hot-upgrade to the freshly installed
                // binary.
                (self.fleet_trigger)(bin);
                fleet_debounce.reset();
            }
        }
        debounce.fire_at().is_some()
    }
}

/// A bounded shutdown refresh: take the write lock only if free, run discover (no reconcile/embed).
fn shutdown_discover(config: &Config) -> anyhow::Result<bool> {
    let lock_repo = locks::write_lock_repo_id(config);
    match locks::WriteLock::acquire_timeout(&config.database, &lock_repo, SKIP_TIMEOUT)? {
        Some(_lock) => {
            // #427: the core refuses a first-time-empty registration; a shutdown refresh defers on
            // it (same as `run_pass`) rather than creating an empty index on the way out. Otherwise
            // it preserves #460's shutdown-reconcile debt: a content change on the way out marks
            // the base reconcile owed so the next startup pays it down.
            match IndexDatabase::index_discover_reporting(config) {
                Ok((db, content_changed)) => {
                    if content_changed {
                        db.mark_watch_shutdown_reconcile_pending()?;
                        return Ok(true);
                    }
                    Ok(false)
                },
                Err(err) if err.downcast_ref::<crate::index::EmptyIndexRefused>().is_some() =>
                    Ok(false),
                Err(err) => Err(err),
            }
        },
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests;
