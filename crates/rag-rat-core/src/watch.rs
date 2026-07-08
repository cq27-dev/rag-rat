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

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
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

/// Run one maintenance pass, blocking on the per-DB write lock (watcher-to-watcher serializes).
pub fn maintenance_pass(config: &Config, run_gc: bool) -> anyhow::Result<()> {
    let lock_repo = locks::write_lock_repo_id(config);
    let _lock = locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
    run_pass(config, run_gc, false)
}

/// Run one maintenance pass only if the write lock is free within `SKIP_TIMEOUT`; returns whether
/// it ran. Used by interactive / hook / shutdown callers so a held lock can't hang them.
pub fn maintenance_pass_or_skip(config: &Config, run_gc: bool) -> anyhow::Result<bool> {
    let lock_repo = locks::write_lock_repo_id(config);
    match locks::WriteLock::acquire_timeout(&config.database, &lock_repo, SKIP_TIMEOUT)? {
        Some(_lock) => {
            run_pass(config, run_gc, false)?;
            Ok(true)
        },
        None => Ok(false),
    }
}

fn startup_catchup_pass(config: &Config) -> anyhow::Result<()> {
    let lock_repo = locks::write_lock_repo_id(config);
    let _lock = locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
    run_pass(config, STARTUP_CATCHUP_RUN_GC, true)
}

fn run_pass(
    config: &Config,
    run_gc: bool,
    retry_base_embedding_backlog: bool,
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
    let overlays_changed =
        timings.stage("overlays", || refresh_worktree_overlays(&mut db, config, Some(&budget)));
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

/// Refresh the branch overlay of every live LINKED worktree of `config.root`'s repo (#219), so a
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
/// `pub` so the hook-driven CLI `maintenance` command shares this exact path: the git hooks invoke
/// `rag-rat maintenance` (not the foreground watcher), so without calling this a commit/checkout/
/// merge in a linked worktree would index the base `config.root` but leave that worktree's overlay
/// stale until a watcher pass or a manual `index --worktree` (#219 review).
pub fn refresh_worktree_overlays(
    db: &mut IndexDatabase,
    config: &Config,
    reconcile: Option<&ReconcileBudget>,
) -> bool {
    let (_, worktrees) = crate::index::live_worktree_contexts(&config.root);
    // The base id is the ENCLOSING worktree root, not `config.root` itself — see
    // `enclosing_worktree_id` (a repo-SUBDIR `config.root` would otherwise mis-classify the main
    // checkout as a linked overlay and re-index it as one) (#219 review).
    let base_id = enclosing_worktree_id(&config.root);
    let mut changed = false;
    for worktree in worktrees {
        if worktree == base_id {
            continue; // the rooted checkout is the base scope, not an overlay
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
                // Embed the overlay's chunks NOW, while the connection is still scoped to this
                // overlay (index_worktree_overlay left it there) — the trailing base reconcile
                // won't see them (#219 review). Run when the overlay CHANGED, OR
                // when it has a BACKLOG of un-embedded chunks: an earlier pass's
                // inline reconcile may have returned `Partial` (the shared time
                // budget ran out mid-pass), leaving overlay chunks un-embedded. The
                // next pass sees the overlay rows as unchanged and would skip the embed forever, so
                // a worktree-scoped `semantic_search` would stay BM25-only for that
                // branch content until an unrelated file change.
                // `pending_embedding_jobs` (active overlay scope) retries that
                // backlog (#219 review). `budget.next_options()` recomputes `max_seconds`
                // from the time left in the SHARED budget so overlays + base can't each spend the
                // full `--max-seconds`; `None` → budget exhausted, skip and let the NEXT pass
                // retry.
                let needs_embed = overlay_needs_embed(this_changed, reconcile, |options| {
                    db.pending_embedding_jobs_with_available_incremental_embedder(options)
                        .is_ok_and(|pending| pending > 0)
                });
                if needs_embed
                    && let Some(budget) = reconcile
                    && let Some(options) = budget.next_options()
                    && let Err(err) = db.reconcile_with_options_progress(options, |_| {})
                {
                    eprintln!("watch: worktree overlay reconcile failed for {worktree}: {err}");
                }
            },
            Err(err) => eprintln!("watch: worktree overlay refresh failed for {worktree}: {err}"),
        }
    }
    // Restore the base scope for the rest of the pass (index_worktree_overlay leaves the connection
    // scoped to the last worktree it touched).
    let _ = db.use_worktree_scope(&config.root, None);
    changed
}

fn overlay_needs_embed(
    this_changed: bool,
    reconcile: Option<&ReconcileBudget>,
    pending_embedding_jobs: impl FnOnce(&ReconcileOptions) -> bool,
) -> bool {
    if this_changed {
        return true;
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
        // ignored/unignored set, including ancestor rules above a subdir-rooted config.
        if is_gitignore_path(path) && path.starts_with(&self.checkout_root) {
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

    fn watch_created_dirs(&mut self, watcher: &mut impl notify::Watcher, event: &Event) -> bool {
        let mut placed_target_watch = false;
        for state in &mut self.states {
            placed_target_watch |= state.watch_created_dirs(watcher, event);
        }
        placed_target_watch
    }

    fn recompile_ignore_and_place_watches(&mut self, watcher: &mut impl notify::Watcher) {
        for state in &mut self.states {
            state.recompile_ignore_and_place_watches(watcher);
        }
    }

    fn event_touches(&self, event: &Event, registry: Option<&Path>) -> bool {
        if event.need_rescan() {
            return !self.states.is_empty() || registry.is_some();
        }
        if !kind_is_mutation(&event.kind) {
            return false;
        }
        event.paths.iter().any(|path| {
            registry.is_some_and(|reg| path.starts_with(reg))
                || self.states.iter().any(|state| state.touches_event(path))
        })
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
) -> bool {
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
        let handle = std::thread::Builder::new()
            .name("rag-rat-watch".to_string())
            .spawn({
                let stop = Arc::clone(&stop);
                move || watcher_main(config, fleet_bin, &stop)
            })
            .ok()?;
        Some(Watcher { stop, handle: Some(handle) })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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

fn event_requests_maintenance(
    watcher: &mut impl notify::Watcher,
    event: &Event,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &mut IgnoreMatcher,
    linked_worktrees: &mut LinkedWorktreeWatches,
    worktree_registry: Option<&Path>,
) -> bool {
    let created_dir_watch_placed =
        watch_created_dirs(watcher, event, config, target_dirs, ignore, None);
    let linked_created_dir_watch_placed = linked_worktrees.watch_created_dirs(watcher, event);
    created_dir_watch_placed
        || linked_created_dir_watch_placed
        || event_is_relevant(config, ignore, event)
        || event_touches_worktree(event, linked_worktrees, worktree_registry)
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

fn watcher_main(config: Config, fleet_bin: Option<PathBuf>, stop: &AtomicBool) {
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

    // Catch-up pass: covers edits made while no watcher was running (startup / election gap).
    // Do not force the reconcile/gc/memory tail on every process start; on an unchanged checkout,
    // startup must stay discover-only and write nothing past any cheap freshness repairs.
    let _ = startup_catchup_pass(&config);

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut notify_watcher) = recommended_watcher(move |res| {
        let _ = tx.send(res);
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

    let mut debounce = Debounce::new(
        Duration::from_millis(config.watch.debounce_ms),
        Duration::from_millis(config.watch.max_latency_ms),
    );
    let mut fleet_debounce = Debounce::new(FLEET_DEBOUNCE, FLEET_MAX_LATENCY);
    // Periodic backstop (covers event-blind filesystems + missed events). `None` disables it.
    let periodic = (config.watch.periodic_sweep_secs > 0)
        .then(|| Duration::from_secs(config.watch.periodic_sweep_secs));
    let mut passes: u64 = 0;
    let mut last_pass = Instant::now(); // the catch-up pass just ran
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        let periodic_wait = periodic.map(|p| (last_pass + p).saturating_duration_since(now));
        let wait = [debounce.due_in(now), fleet_debounce.due_in(now), periodic_wait]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(Duration::from_millis(500));
        match rx.recv_timeout(wait) {
            Ok(Ok(event)) => {
                let now = Instant::now();
                // Place watches on newly-appeared, non-ignored directories REGARDLESS of relevance
                // (#332). Target dirs are watched NON-recursively (#331), so notify won't
                // auto-descend into a new subdir — and a bare `mkdir src/foo` is NOT
                // `event_is_relevant` (a directory is extensionless → not a target FILE), so gating
                // this on the relevance check below would leave the new dir unwatched and its files
                // invisible until the periodic sweep (or forever, if it is disabled). A directory
                // MOVED in (`mv pkg src/pkg`) arrives as a rename, not a Create —
                // `watch_created_dirs` handles both. Its own cheap filter
                // (Create/rename + is-dir + not-ignored) makes calling it on every
                // event correct. When it places a watch, arm the debounce even if the directory
                // event itself is extensionless and therefore not relevant: the newly-created dir
                // can already contain files that need the pass to discover them. The watch keeps
                // SUBSEQUENT edits firing. Gates each path on real-dir + target relation +
                // not-ignored, and recompiles the matcher for a moved-in nested `.gitignore`
                // (#332).
                if event_requests_maintenance(
                    &mut notify_watcher,
                    &event,
                    &config,
                    &target_dirs,
                    &mut ignore,
                    &mut linked_worktrees,
                    worktree_registry.as_deref(),
                ) {
                    debounce.on_event(now);
                    // A `.gitignore` mutation changed the rules — recompile so subsequent events
                    // are classified against current rules, not the matcher
                    // this watcher booted with.
                    if kind_is_mutation(&event.kind)
                        && event.paths.iter().any(|path| is_gitignore_path(path))
                    {
                        // PLACEMENT, not just classification, must track the new rules (#332): a
                        // removed ignore rule UN-ignores a subtree the startup walk skipped, so
                        // re-walk and add watches for it now — otherwise edits inside it never
                        // fire. notify's `watch()` is idempotent for an
                        // already-watched path, so re-walking only ADDS the
                        // newly-eligible dirs. (A newly-IGNORED subtree keeps its
                        // now-stale watches — harmless wasted watches; full unwatch bookkeeping is
                        // deferred.)
                        recompile_ignore_and_place_watches(
                            &mut notify_watcher,
                            &config,
                            &target_dirs,
                            &mut ignore,
                            &mut linked_worktrees,
                        );
                    }
                }
                if event_targets_binary(fleet_bin.as_deref(), &event) {
                    fleet_debounce.on_event(now);
                }
            },
            Ok(_) => {},
            Err(RecvTimeoutError::Timeout) => {},
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let now = Instant::now();
        let periodic_due = periodic.is_some_and(|p| now >= last_pass + p);
        if debounce.should_fire(now) || periodic_due {
            passes += 1;
            let _ = maintenance_pass(&config, passes.is_multiple_of(GC_EVERY_PASSES));
            debounce.reset();
            last_pass = Instant::now();
            // Refresh the live linked-worktree set after every pass. Existing checkout paths can
            // switch branches and therefore branch-local target sets; rebuilding the state keeps
            // classification, ignore matchers, and watch placement in one place. Removed checkout
            // paths can still have stale backend watches (harmless; their overlay is GC-pruned),
            // but no stale `LinkedWorktreeWatch` state remains active.
            sync_linked_worktrees_after_pass(&mut notify_watcher, &config, &mut linked_worktrees);
        }
        if fleet_debounce.should_fire(now)
            && let Some(bin) = fleet_bin.as_deref()
        {
            // Signal the fleet (this process last) to hot-upgrade to the freshly installed binary.
            fleet::trigger(bin);
            fleet_debounce.reset();
        }
    }

    // Final pass for edits in the last debounce window — discover only (no embedding), timeout-and-
    // skip. The host may SIGKILL shortly after stdin EOF, so shutdown must be bounded; discover is
    // fast and keeps structure fresh, and the next content-changing or periodic pass does the
    // embedding.
    if debounce.fire_at().is_some() {
        let _ = shutdown_discover(&config);
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
mod tests {
    use notify::Watcher as _;
    use notify::event::{CreateKind, Flag, ModifyKind};

    use super::*;
    use crate::config::{
        Config, LlmConfig, RemoteBackend, RemoteEmbeddingConfig, ResolvedTarget, TargetKind,
        WatchConfig,
    };
    use crate::embedding_models::{FASTEMBED_MODEL_ID, HASH_MODEL_ID, spec};
    use crate::language::Language;

    fn mutation_event(path: PathBuf) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path)
    }

    /// A single-Rust-target `Config` rooted at `root` watching `target_dirs` — the inline builder
    /// the real-watcher placement tests share so they can call `watch_created_dirs` (which needs a
    /// `&Config` for the target-relation gate, #332).
    fn whole_root_config(root: &Path, target_dirs: &[PathBuf]) -> Config {
        Config {
            repo_id_override: None,
            database_key_pinned: true,
            root: root.to_path_buf(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: target_dirs.to_vec(),
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
        }
    }

    /// #427: a maintenance/watch pass on a first-time-empty config DEFERS — the core refuses the
    /// empty registration and `run_pass` swallows `EmptyIndexRefused`, returning `Ok(())` and
    /// registering nothing, rather than erroring into the watcher loop. Covers the in-process defer
    /// path the subprocess CLI guards exercise out-of-process (so it doesn't count toward
    /// coverage).
    #[test]
    fn maintenance_pass_defers_on_a_first_time_empty_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A single rust target with NO directories → discovers nothing → first-time-empty.
        let config = whole_root_config(tmp.path(), &[]);
        let result = maintenance_pass(&config, false);
        assert!(result.is_ok(), "an empty first-time config must defer, not error: {result:?}");
        assert!(!config.database.exists(), "deferring must register no empty index");
    }

    fn ephemeral_remote(query_endpoint: Option<&str>) -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: RemoteBackend::Ollama,
            endpoint: None,
            cookbook: Some("@rag-rat/cookbook/modal".to_string()),
            query_endpoint: query_endpoint.map(str::to_string),
            auth_env: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            request_timeout_s: 5,
        }
    }

    fn activate_ephemeral_model(config: &Config, repo_id: &str, query_endpoint: Option<&str>) {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        let remote = ephemeral_remote(query_endpoint);
        let model_spec = spec(FASTEMBED_MODEL_ID).unwrap();
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
             'ollama', last_error = NULL
             WHERE model_id = ?1",
            rusqlite::params![FASTEMBED_MODEL_ID, i64::try_from(model_spec.dim).unwrap()],
        )
        .unwrap();
        crate::index::set_repo_meta(&conn, repo_id, "active_embedding_model", FASTEMBED_MODEL_ID)
            .unwrap();
        crate::index::set_repo_meta(
            &conn,
            repo_id,
            "active_embedding_remote_config",
            &serde_json::to_string(&remote).unwrap(),
        )
        .unwrap();
        crate::index::set_repo_meta(
            &conn,
            repo_id,
            "embedding_active_model_version",
            &crate::index::ai::remote_freshness_version(model_spec, &remote),
        )
        .unwrap();
    }

    #[derive(Debug, Default)]
    struct RecordingWatcher {
        watched: Vec<(PathBuf, RecursiveMode)>,
    }

    impl notify::Watcher for RecordingWatcher {
        fn new<F: notify::EventHandler>(
            _event_handler: F,
            _config: notify::Config,
        ) -> notify::Result<Self>
        where
            Self: Sized,
        {
            Ok(Self::default())
        }

        fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
            self.watched.push((path.to_path_buf(), recursive_mode));
            Ok(())
        }

        fn unwatch(&mut self, _path: &Path) -> notify::Result<()> {
            Ok(())
        }

        fn kind() -> notify::WatcherKind
        where
            Self: Sized,
        {
            notify::WatcherKind::NullWatcher
        }
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
    fn startup_catchup_does_not_force_the_expensive_tail() {
        assert!(
            !should_run_pass_tail(false, false, STARTUP_CATCHUP_RUN_GC, false, false, false),
            "an unchanged startup catch-up must not run reconcile/gc/memory validation",
        );
        assert!(
            should_run_pass_tail(true, false, STARTUP_CATCHUP_RUN_GC, false, false, false),
            "real base content changes still run the maintenance tail",
        );
        assert!(
            should_run_pass_tail(false, true, STARTUP_CATCHUP_RUN_GC, false, false, false),
            "linked-worktree overlay changes still run the maintenance tail",
        );
        assert!(
            should_run_pass_tail(false, false, true, false, false, false),
            "scheduled GC passes still force the maintenance tail",
        );
        assert!(
            should_run_pass_tail(false, false, STARTUP_CATCHUP_RUN_GC, true, false, false),
            "a bounded shutdown discover marks base reconcile owed for the next startup pass",
        );
        assert!(
            should_run_pass_tail(false, false, STARTUP_CATCHUP_RUN_GC, false, true, false),
            "startup catch-up retries an already-indexed base embedding backlog",
        );
        assert!(
            should_run_pass_tail(false, false, STARTUP_CATCHUP_RUN_GC, false, false, true),
            "a quiet-elapsed clone-graph backlog forces the otherwise-idle tail (#472)",
        );
    }

    #[test]
    fn changed_overlay_skips_backlog_probe() {
        let budget = ReconcileBudget::new(
            ReconcileOptions::default(),
            Instant::now() - Duration::from_secs(1),
        );
        let needs_embed = overlay_needs_embed(true, Some(&budget), |_| false);

        assert!(needs_embed, "a changed overlay still embeds inline");
    }

    #[test]
    fn forced_tail_skips_base_backlog_probe() {
        let budget = ReconcileBudget::new(
            ReconcileOptions::default(),
            Instant::now() - Duration::from_secs(1),
        );
        let needs_tail = base_embedding_backlog_needs_tail(true, true, &budget, |_| true);

        assert!(!needs_tail, "another tail trigger already guarantees reconcile");
    }

    #[test]
    fn maintenance_pass_or_skip_runs_when_lock_is_available() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-maintenance-skip-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn maintenance_target() {}\n").unwrap();
        let root = root.canonicalize().unwrap();
        let config = whole_root_config(&root, &[PathBuf::from("src")]);

        assert!(
            maintenance_pass_or_skip(&config, false).unwrap(),
            "an available writer lock should run the maintenance pass"
        );
        let db = IndexDatabase::open_config(&config).unwrap();
        assert!(
            db.status(&config.database).unwrap().file_count_by_language.values().sum::<u64>() > 0
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn startup_catchup_retries_existing_base_embedding_backlog() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-startup-backlog-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn pending_startup_embedding(input: i32) -> i32 {
    let doubled = input * 2;
    let shifted = doubled + 13;
    shifted + 7
}
",
        )
        .unwrap();
        let root = root.canonicalize().unwrap();

        let mut config = whole_root_config(&root, &[PathBuf::from("src")]);
        config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();

        let db = IndexDatabase::rebuild(&config).unwrap();
        db.install_model(HASH_MODEL_ID, None).unwrap();
        assert!(
            db.pending_embedding_jobs().unwrap() > 0,
            "fixture starts with indexed chunks but no embeddings"
        );
        drop(db);

        startup_catchup_pass(&config).unwrap();
        let db = IndexDatabase::open_config(&config).unwrap();
        assert_eq!(
            db.pending_embedding_jobs().unwrap(),
            0,
            "unchanged startup catch-up retried and embedded the existing base backlog"
        );
        assert!(
            db.current_embedding_count(HASH_MODEL_ID).unwrap() > 0,
            "startup retry wrote hash embeddings"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn startup_catchup_skips_ephemeral_backlog_scan_without_query_endpoint() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-ephemeral-backlog-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn pending_ephemeral_startup(input: i32) -> i32 {
    let doubled = input * 2;
    let shifted = doubled + 13;
    shifted + 7
}
",
        )
        .unwrap();
        let root = root.canonicalize().unwrap();

        let mut config = whole_root_config(&root, &[PathBuf::from("src")]);
        config.llm.embedding.backend = FASTEMBED_MODEL_ID.parse().unwrap();

        let db = IndexDatabase::rebuild(&config).unwrap();
        let repo_id = db.active_repo_id.clone();
        drop(db);
        activate_ephemeral_model(&config, &repo_id, None);

        let db = IndexDatabase::open_config(&config).unwrap();
        assert!(
            db.pending_embedding_jobs().unwrap() > 0,
            "fixture has indexed chunks missing embeddings for the active ephemeral model"
        );
        drop(db);

        crate::index::ai::reset_estimated_reconcile_job_calls();
        startup_catchup_pass(&config).unwrap();
        assert_eq!(
            crate::index::ai::estimated_reconcile_job_calls(),
            0,
            "startup must not scan chunks before the ephemeral light endpoint is known usable"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn startup_catchup_reconciles_shutdown_discovered_content() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-shutdown-reconcile-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn initial_value(input: i32) -> i32 {
    let doubled = input * 2;
    let shifted = doubled + 13;
    shifted + 7
}
",
        )
        .unwrap();
        let root = root.canonicalize().unwrap();

        let mut config = whole_root_config(&root, &[PathBuf::from("src")]);
        config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();

        let db = IndexDatabase::rebuild(&config).unwrap();
        db.install_model(HASH_MODEL_ID, None).unwrap();
        db.reconcile_with_options_progress(ReconcileOptions::default(), |_| {}).unwrap();
        assert!(
            db.current_embedding_count(HASH_MODEL_ID).unwrap() > 0,
            "fixture must produce at least one embeddable chunk"
        );
        assert_eq!(db.pending_embedding_jobs().unwrap(), 0, "fixture starts fully reconciled");
        assert!(
            !shutdown_discover(&config).unwrap(),
            "shutdown discover without source edits has no reconcile marker to set"
        );
        drop(db);

        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn changed_value(input: i32) -> i32 {
    let tripled = input * 3;
    let shifted = tripled + 21;
    shifted - 4
}
",
        )
        .unwrap();
        assert!(shutdown_discover(&config).unwrap(), "shutdown discover indexed the edit");

        let db = IndexDatabase::open_config(&config).unwrap();
        assert!(
            db.watch_shutdown_reconcile_pending().unwrap(),
            "shutdown-discovered content leaves a startup reconcile marker"
        );
        assert!(
            db.pending_embedding_jobs().unwrap() > 0,
            "the discover-only shutdown pass leaves changed chunks without embeddings"
        );
        drop(db);

        maintenance_pass(&config, STARTUP_CATCHUP_RUN_GC).unwrap();
        let db = IndexDatabase::open_config(&config).unwrap();
        assert!(
            !db.watch_shutdown_reconcile_pending().unwrap(),
            "successful startup reconcile clears the shutdown marker"
        );
        assert_eq!(
            db.pending_embedding_jobs().unwrap(),
            0,
            "startup catch-up embedded the backlog"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn startup_catchup_keeps_shutdown_marker_when_reconcile_is_blocked() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-shutdown-blocked-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn pending_embedding(input: i32) -> i32 {
    let doubled = input * 2;
    let shifted = doubled + 13;
    shifted + 7
}
",
        )
        .unwrap();
        let root = root.canonicalize().unwrap();

        let config = whole_root_config(&root, &[PathBuf::from("src")]);
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.mark_watch_shutdown_reconcile_pending().unwrap();
        drop(db);

        maintenance_pass(&config, STARTUP_CATCHUP_RUN_GC).unwrap();
        let db = IndexDatabase::open_config(&config).unwrap();
        assert!(
            db.watch_shutdown_reconcile_pending().unwrap(),
            "a blocked startup reconcile must keep the shutdown marker for a later retry"
        );
        assert_eq!(
            db.pending_embedding_jobs().unwrap(),
            0,
            "not-ready models report no pending jobs, so marker clearing must key off status"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn wal_checkpoint_runs_on_quiet_passes_only() {
        // #482: the TRUNCATE checkpoint waits on concurrent readers up to the busy timeout, so a
        // churn pass (content changed) must never attempt it; it lands on the first quiet pass
        // after editing pauses — the same deferral posture as the clone-graph rebuild.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-wal-checkpoint-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn wal_probe() -> i32 { 1 }\n").unwrap();
        let config = whole_root_config(&root, &[PathBuf::from("src")]);
        let db = IndexDatabase::rebuild(&config).unwrap();

        // Put frames in the WAL so there is something to truncate (any meta write serves).
        db.mark_watch_shutdown_reconcile_pending().unwrap();
        db.clear_watch_shutdown_reconcile_pending().unwrap();
        assert!(db.database_file_health().unwrap().wal_bytes > 0);

        maybe_checkpoint_wal(&db, false, 1);
        assert!(
            db.database_file_health().unwrap().wal_bytes > 0,
            "a churn pass must leave the WAL alone"
        );

        maybe_checkpoint_wal(&db, true, 1);
        assert_eq!(
            db.database_file_health().unwrap().wal_bytes,
            0,
            "a quiet pass truncates the oversized WAL"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn initial_watch_state_places_base_gitignore_and_fleet_surfaces() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-initial-state-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/kept")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let root = root.canonicalize().unwrap();

        let config = whole_root_config(&root, &[PathBuf::from("src")]);
        let target_dirs = config.target_directories();
        let ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
        let mut watcher = RecordingWatcher::default();
        let (linked_worktrees, registry) = place_initial_watch_state(
            &mut watcher,
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
                .any(|(path, mode)| path == &root.join("src")
                    && *mode == RecursiveMode::NonRecursive),
            "configured target roots are placed through the initial state helper",
        );
        assert!(
            watcher
                .watched
                .iter()
                .any(|(path, mode)| path == &root && *mode == RecursiveMode::NonRecursive),
            "the config root is watched for root .gitignore edits",
        );
        assert!(
            watcher
                .watched
                .iter()
                .any(|(path, mode)| path == &root.join("bin")
                    && *mode == RecursiveMode::NonRecursive),
            "fleet hot-upgrade watches the installed binary directory",
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn event_maintenance_helpers_place_dirs_recompile_and_refresh_linked_state() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-maint-helper-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/fresh")).unwrap();
        let root = root.canonicalize().unwrap();

        let config = whole_root_config(&root, &[PathBuf::from("src")]);
        let target_dirs = config.target_directories();
        let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
        let mut linked_worktrees = LinkedWorktreeWatches::default();
        let mut watcher = RecordingWatcher::default();
        let create =
            Event::new(EventKind::Create(CreateKind::Folder)).add_path(root.join("src/fresh"));

        assert!(
            event_requests_maintenance(
                &mut watcher,
                &create,
                &config,
                &target_dirs,
                &mut ignore,
                &mut linked_worktrees,
                None,
            ),
            "placing a newly-created target dir must request a maintenance pass",
        );
        assert!(
            watcher.watched.iter().any(|(path, mode)| path == &root.join("src/fresh")
                && *mode == RecursiveMode::NonRecursive),
            "event helper delegates created-directory placement",
        );

        let before_recompile = watcher.watched.len();
        recompile_ignore_and_place_watches(
            &mut watcher,
            &config,
            &target_dirs,
            &mut ignore,
            &mut linked_worktrees,
        );
        assert!(
            watcher.watched.len() > before_recompile,
            "gitignore recompiles also re-place base target watches",
        );

        sync_linked_worktrees_after_pass(&mut watcher, &config, &mut linked_worktrees);
        assert!(linked_worktrees.states.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn event_maintenance_helper_requests_pass_for_relevant_and_registry_events() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-maint-branches-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let root = root.canonicalize().unwrap();

        let config = whole_root_config(&root, &[PathBuf::from("src")]);
        let target_dirs = config.target_directories();
        let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
        let mut linked_worktrees = LinkedWorktreeWatches::default();
        let mut watcher = RecordingWatcher::default();
        let relevant_file = mutation_event(root.join("src/lib.rs"));

        assert!(event_requests_maintenance(
            &mut watcher,
            &relevant_file,
            &config,
            &target_dirs,
            &mut ignore,
            &mut linked_worktrees,
            None,
        ));

        let registry = root.join(".git/worktrees");
        let registry_event = mutation_event(registry.join("feature/HEAD"));
        assert!(event_requests_maintenance(
            &mut watcher,
            &registry_event,
            &config,
            &target_dirs,
            &mut ignore,
            &mut linked_worktrees,
            Some(&registry),
        ));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn initial_watch_state_places_worktree_registry() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let main =
            std::env::temp_dir().join(format!("ragrat-watch-registry-{}-{id}", std::process::id()));
        let linked = std::env::temp_dir()
            .join(format!("ragrat-watch-registry-linked-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&main);
        let _ = std::fs::remove_dir_all(&linked);
        std::fs::create_dir_all(main.join("src")).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let status =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
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
        let config = whole_root_config(&main, &[PathBuf::from("src")]);
        let target_dirs = config.target_directories();
        let ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
        let mut watcher = RecordingWatcher::default();
        let (linked_worktrees, registry) =
            place_initial_watch_state(&mut watcher, &config, &target_dirs, &ignore, None);
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
        let config = whole_root_config(&root, &target_dirs);
        let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
        let mut watcher = RecordingWatcher::default();
        let access =
            Event::new(EventKind::Access(AccessKind::Any)).add_path(root.join("src/fresh"));

        assert!(!watch_created_dirs(
            &mut watcher,
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
        let checkout = std::env::temp_dir()
            .join(format!("ragrat-bootstrap-chain-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&checkout);
        std::fs::create_dir_all(checkout.join("packages")).unwrap();
        let checkout = checkout.canonicalize().unwrap();
        let packages = checkout.join("packages");
        let config_root = packages.join("crate");

        assert_eq!(
            missing_config_root_bootstrap_dirs(&config_root, &checkout),
            vec![checkout.clone(), packages.clone()],
            "the deepest existing ancestor must be watched so its missing child creation is \
             delivered",
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
        let config = whole_root_config(&root, &nested);

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
        let whole_config = whole_root_config(&root, &whole_root);
        assert_eq!(
            created_dir_placement(&whole_config, &whole_root, &root.join("anything"), None),
            CreatedDirPlacement::TargetSubtree,
        );

        let checkout = PathBuf::from("/checkout");
        let subdir_root = checkout.join("packages/crate");
        let subdir_config = whole_root_config(&subdir_root, &nested);
        assert_eq!(
            created_dir_placement(
                &subdir_config,
                &nested,
                &checkout.join("packages"),
                Some(&checkout)
            ),
            CreatedDirPlacement::TargetAncestor,
        );
        assert_eq!(
            created_dir_placement(&subdir_config, &nested, &subdir_root, Some(&checkout)),
            CreatedDirPlacement::TargetAncestor,
        );
        assert_eq!(
            created_dir_placement(
                &subdir_config,
                &nested,
                &checkout.join("vendor"),
                Some(&checkout)
            ),
            CreatedDirPlacement::OutsideTargets,
        );
    }

    #[test]
    fn event_touches_worktree_matches_checkout_targets_and_registry() {
        let config = Config {
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
        let worktrees = watch_linked_worktrees(&mut watcher, &config, vec![worktree.clone()]);

        // A target file in a linked worktree fires (its overlay needs refreshing).
        assert!(event_touches_worktree(
            &mutation_event(worktree.join("src/a.rs")),
            &worktrees,
            Some(&registry),
        ));
        // A non-target file in the worktree does not.
        assert!(!event_touches_worktree(
            &mutation_event(worktree.join("README.md")),
            &worktrees,
            Some(&registry),
        ));
        // A change in the worktree registry (a `git worktree add`/`remove`) fires.
        assert!(event_touches_worktree(
            &mutation_event(registry.join("feat/HEAD")),
            &worktrees,
            Some(&registry),
        ));
        // A `.gitignore` edit in the linked checkout fires (it changes the overlay's ignored set),
        // mirroring the base classifier (#219 review).
        assert!(event_touches_worktree(
            &mutation_event(worktree.join(".gitignore")),
            &worktrees,
            Some(&registry),
        ));
        // A `.gitignore` OUTSIDE any watched checkout does not.
        assert!(!event_touches_worktree(
            &mutation_event(PathBuf::from("/elsewhere/.gitignore")),
            &worktrees,
            Some(&registry),
        ));
        // A read event never fires (anti-feedback, same as the base watcher).
        let read = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
            .add_path(worktree.join("src/a.rs"));
        assert!(!event_touches_worktree(&read, &worktrees, Some(&registry)));
        // A backend rescan fires when there is linked-worktree or registry state to refresh.
        let rescan = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(event_touches_worktree(&rescan, &worktrees, None));
        assert!(event_touches_worktree(
            &rescan,
            &LinkedWorktreeWatches::default(),
            Some(&registry),
        ));
        assert!(!event_touches_worktree(&rescan, &LinkedWorktreeWatches::default(), None,));
        // No worktrees and no registry → nothing fires.
        assert!(!event_touches_worktree(
            &mutation_event(worktree.join("src/a.rs")),
            &LinkedWorktreeWatches::default(),
            None,
        ));
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
        let worktree =
            std::env::temp_dir().join(format!("ragrat-wt-ign-{}-{id}", std::process::id()));
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::create_dir_all(worktree.join("ignored_dir")).unwrap();
        std::fs::create_dir_all(worktree.join("target/debug")).unwrap();
        let worktree = worktree.canonicalize().unwrap();
        std::fs::write(worktree.join(".gitignore"), "ignored_dir/\ntarget/\n").unwrap();

        let config = whole_root_config(&worktree, &[PathBuf::from(".")]);
        let mut watcher = RecordingWatcher::default();
        let worktrees = watch_linked_worktrees(&mut watcher, &config, vec![worktree.clone()]);

        assert!(
            event_touches_worktree(&mutation_event(worktree.join("src/lib.rs")), &worktrees, None),
            "an unignored linked target file still fires",
        );
        assert!(
            !event_touches_worktree(
                &mutation_event(worktree.join("ignored_dir/out.rs")),
                &worktrees,
                None
            ),
            "a linked worktree gitignored source-looking path must not fire",
        );
        assert!(
            !event_touches_worktree(
                &mutation_event(worktree.join("target/debug/build.rs")),
                &worktrees,
                None
            ),
            "a linked worktree floor/gitignored build path must not fire",
        );
        assert!(
            event_touches_worktree(&mutation_event(worktree.join(".gitignore")), &worktrees, None),
            "a linked worktree .gitignore edit still fires so rules can be recompiled",
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
        let worktree =
            std::env::temp_dir().join(format!("ragrat-wt-place-{}-{id}", std::process::id()));
        std::fs::create_dir_all(worktree.join("src/kept")).unwrap();
        std::fs::create_dir_all(worktree.join("src/ignored_dir")).unwrap();
        std::fs::create_dir_all(worktree.join("target/debug")).unwrap();
        let worktree = worktree.canonicalize().unwrap();
        std::fs::write(worktree.join(".gitignore"), "src/ignored_dir/\ntarget/\n").unwrap();

        let config = whole_root_config(&worktree, &[PathBuf::from("src")]);
        let mut watcher = RecordingWatcher::default();
        let worktrees = watch_linked_worktrees(&mut watcher, &config, vec![worktree.clone()]);
        let state = &worktrees.states[0];

        assert_eq!(state.config.root, worktree);
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
            watcher
                .watched
                .iter()
                .all(|(path, _)| !path.starts_with(state.config.root.join("target"))
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
        let worktree =
            std::env::temp_dir().join(format!("ragrat-wt-sync-{}-{id}", std::process::id()));
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::create_dir_all(worktree.join("extra")).unwrap();
        let worktree = worktree.canonicalize().unwrap();

        let src_config = whole_root_config(&worktree, &[PathBuf::from("src")]);
        let extra_config = whole_root_config(&worktree, &[PathBuf::from("extra")]);
        let mut watcher = RecordingWatcher::default();
        let mut worktrees = LinkedWorktreeWatches::default();
        worktrees.sync(&mut watcher, &src_config, vec![worktree.clone()]);
        assert_eq!(worktrees.states[0].target_dirs, vec![PathBuf::from("src")]);

        worktrees.sync(&mut watcher, &extra_config, vec![worktree.clone()]);
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
        let worktree =
            std::env::temp_dir().join(format!("ragrat-wt-set-{}-{id}", std::process::id()));
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        let worktree = worktree.canonicalize().unwrap();
        std::fs::write(worktree.join(".gitignore"), "").unwrap();

        let config = whole_root_config(&worktree, &[PathBuf::from("src")]);
        let mut watcher = RecordingWatcher::default();
        let mut worktrees = watch_linked_worktrees(&mut watcher, &config, vec![worktree.clone()]);

        let fresh = worktree.join("src/fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(fresh.clone());
        assert!(
            worktrees.watch_created_dirs(&mut watcher, &create),
            "created target dirs should request a maintenance pass",
        );
        assert!(
            watcher.watched.iter().any(|(path, _)| path == &fresh),
            "created target dirs are watched through the centralized linked-worktree state",
        );

        std::fs::write(worktree.join(".gitignore"), "src/fresh/\n").unwrap();
        worktrees.recompile_ignore_and_place_watches(&mut watcher);
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
        let worktree =
            std::env::temp_dir().join(format!("ragrat-wt-ancestor-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&worktree).unwrap();
        let worktree = worktree.canonicalize().unwrap();
        std::fs::write(worktree.join(".gitignore"), "").unwrap();

        let target_dirs = vec![PathBuf::from("src/generated")];
        let config = whole_root_config(&worktree, &target_dirs);
        let mut watcher = RecordingWatcher::default();
        let mut worktrees = watch_linked_worktrees(&mut watcher, &config, vec![worktree.clone()]);

        watcher.watched.clear();
        let ancestor = worktree.join("src");
        std::fs::create_dir_all(&ancestor).unwrap();
        let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(ancestor.clone());
        assert!(
            worktrees.watch_created_dirs(&mut watcher, &create),
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
            "created ancestors should re-place configured target watches in case the target \
             already exists",
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
        let worktree = std::env::temp_dir()
            .join(format!("ragrat-wt-ancestor-ignore-{}-{id}", std::process::id()));
        std::fs::create_dir_all(worktree.join("src/generated")).unwrap();
        std::fs::create_dir_all(worktree.join("src/sibling")).unwrap();
        let worktree = worktree.canonicalize().unwrap();
        std::fs::write(worktree.join("src/.gitignore"), "generated/\n").unwrap();
        std::fs::write(worktree.join("src/sibling/.gitignore"), "marker.rs\n").unwrap();

        let target_dirs = vec![PathBuf::from("src/generated")];
        let config = whole_root_config(&worktree, &target_dirs);
        let mut watcher = RecordingWatcher::default();
        let worktrees = watch_linked_worktrees(&mut watcher, &config, vec![worktree.clone()]);
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
        use std::process::Command;
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let repo = std::env::temp_dir()
            .join(format!("ragrat-wt-missing-root-main-{}-{id}", std::process::id()));
        let checkout = std::env::temp_dir()
            .join(format!("ragrat-wt-missing-root-linked-{}-{id}", std::process::id()));
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
            let output = Command::new("git").args(&args).current_dir(&repo).output().unwrap();
            assert!(output.status.success(), "git {args:?} failed: {output:?}");
        }
        std::fs::create_dir_all(checkout.join("packages")).unwrap();
        let checkout = checkout.canonicalize().unwrap();
        let config_root = repo.join("packages/crate").canonicalize().unwrap();
        let target_dirs = vec![PathBuf::from("src")];
        let config = whole_root_config(&config_root, &target_dirs);

        let mut watcher = RecordingWatcher::default();
        let worktrees = watch_linked_worktrees(&mut watcher, &config, vec![checkout.clone()]);
        let linked_root = checkout.join("packages/crate");

        assert_eq!(worktrees.states[0].config.root, linked_root);
        assert!(!linked_root.exists(), "the linked branch has not created the configured root yet");
        assert!(
            watcher
                .watched
                .iter()
                .any(|(path, mode)| path == &checkout && *mode == RecursiveMode::NonRecursive),
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
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-recreated-root-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".gitignore"), "").unwrap();
        let root = root.canonicalize().unwrap();
        let target_dirs = vec![PathBuf::from("src")];
        let config = whole_root_config(&root, &target_dirs);
        let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
        let mut watcher = RecordingWatcher::default();
        let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(root.clone());

        assert!(
            watch_created_dirs(&mut watcher, &create, &config, &target_dirs, &mut ignore, None),
            "recreated config roots should re-place target watches and request maintenance",
        );
        assert!(
            watcher
                .watched
                .iter()
                .any(|(path, mode)| path == &root && *mode == RecursiveMode::NonRecursive),
            "the recreated config root itself should stay watched non-recursively",
        );
        assert!(
            watcher
                .watched
                .iter()
                .any(|(path, mode)| path == &root.join("src")
                    && *mode == RecursiveMode::NonRecursive),
            "configured targets below the recreated root should be watched again",
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn watch_created_dirs_bootstraps_missing_linked_subdir_root_ancestors() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let checkout = std::env::temp_dir()
            .join(format!("ragrat-watch-linked-ancestor-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&checkout);
        std::fs::create_dir_all(checkout.join("packages")).unwrap();
        std::fs::create_dir_all(checkout.join("vendor")).unwrap();
        let checkout = checkout.canonicalize().unwrap();
        let packages = checkout.join("packages");
        let target_dirs = vec![PathBuf::from("src")];
        let config = whole_root_config(&packages.join("crate"), &target_dirs);
        let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
        let mut watcher = RecordingWatcher::default();
        let create_packages =
            Event::new(EventKind::Create(CreateKind::Folder)).add_path(packages.clone());

        assert!(
            watch_created_dirs(
                &mut watcher,
                &create_packages,
                &config,
                &target_dirs,
                &mut ignore,
                Some(&checkout),
            ),
            "an intermediate ancestor of a missing linked config root must keep the bootstrap \
             moving",
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
        let create_vendor =
            Event::new(EventKind::Create(CreateKind::Folder)).add_path(vendor.clone());
        assert!(
            !watch_created_dirs(
                &mut watcher,
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
        let worktree =
            std::env::temp_dir().join(format!("ragrat-wt-create-pass-{}-{id}", std::process::id()));
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        let worktree = worktree.canonicalize().unwrap();
        std::fs::write(worktree.join(".gitignore"), "").unwrap();

        let target_dirs = vec![PathBuf::from("src")];
        let config = whole_root_config(&worktree, &target_dirs);
        let mut watcher = RecordingWatcher::default();
        let mut worktrees = watch_linked_worktrees(&mut watcher, &config, vec![worktree.clone()]);

        watcher.watched.clear();
        let pkg = worktree.join("src/pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("lib.rs"), "fn pkg() {}\n").unwrap();
        let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(pkg.clone());

        assert!(
            !event_touches_worktree(&create, &worktrees, None),
            "extensionless directory events are not target-file events",
        );
        assert!(
            worktrees.watch_created_dirs(&mut watcher, &create),
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
        let first = std::env::temp_dir()
            .join(format!("ragrat-wt-create-all-a-{}-{id}", std::process::id()));
        let second = std::env::temp_dir()
            .join(format!("ragrat-wt-create-all-b-{}-{id}", std::process::id()));
        for root in [&first, &second] {
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join(".gitignore"), "").unwrap();
        }
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();

        let target_dirs = vec![PathBuf::from("src")];
        let config = whole_root_config(&first, &target_dirs);
        let mut watcher = RecordingWatcher::default();
        let mut worktrees =
            watch_linked_worktrees(&mut watcher, &config, vec![first.clone(), second.clone()]);

        watcher.watched.clear();
        let first_pkg = first.join("src/pkg");
        let second_pkg = second.join("src/pkg");
        std::fs::create_dir_all(&first_pkg).unwrap();
        std::fs::create_dir_all(&second_pkg).unwrap();
        let create = Event::new(EventKind::Create(CreateKind::Folder))
            .add_path(first_pkg.clone())
            .add_path(second_pkg.clone());

        assert!(
            worktrees.watch_created_dirs(&mut watcher, &create),
            "at least one linked target dir was watched",
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
        let root = std::env::temp_dir()
            .join(format!("ragrat-watch-created-ignore-{}-{id}", std::process::id()));
        std::fs::create_dir_all(root.join("src/already_ignored")).unwrap();
        std::fs::create_dir_all(root.join("src/newly_ignored")).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join(".gitignore"), "src/already_ignored/\n").unwrap();

        let target_dirs = vec![PathBuf::from("src")];
        let config = whole_root_config(&root, &target_dirs);
        let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
        let mut watcher = RecordingWatcher::default();

        let already = root.join("src/already_ignored");
        let create_already =
            Event::new(EventKind::Create(CreateKind::Folder)).add_path(already.clone());
        watch_created_dirs(&mut watcher, &create_already, &config, &target_dirs, &mut ignore, None);
        assert!(
            watcher.watched.iter().all(|(path, _)| path != &already),
            "a dir ignored before recompile should not be watched",
        );

        std::fs::write(root.join(".gitignore"), "src/already_ignored/\nsrc/newly_ignored/\n")
            .unwrap();
        let newly = root.join("src/newly_ignored");
        let create_newly =
            Event::new(EventKind::Create(CreateKind::Folder)).add_path(newly.clone());
        watch_created_dirs(&mut watcher, &create_newly, &config, &target_dirs, &mut ignore, None);
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
        use std::process::Command;
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let repo =
            std::env::temp_dir().join(format!("ragrat-wt-subdir-{}-{id}", std::process::id()));
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
            Command::new("git").args(&args).current_dir(&repo).output().unwrap();
        }
        // `config.root` is the `crate` SUBDIR of the repo.
        let config_root = repo.join("crate").canonicalize().unwrap();
        let config = Config {
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
        let checkout =
            std::env::temp_dir().join(format!("ragrat-wt-subdir-co-{}-{id}", std::process::id()));
        let mut watcher = RecordingWatcher::default();
        let worktrees = watch_linked_worktrees(&mut watcher, &config, vec![checkout.clone()]);
        assert!(
            event_touches_worktree(
                &mutation_event(checkout.join("crate/src/a.rs")),
                &worktrees,
                None,
            ),
            "a subdir-rooted config must fire on a linked edit under <checkout>/<subdir>/<target>"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn event_is_relevant_skips_gitignored_paths_consistently_with_walker() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("ragrat-watchev-{}-{id}", std::process::id()));
        std::fs::create_dir_all(root.join("crates")).unwrap();
        std::fs::write(root.join(".gitignore"), "gen/\n").unwrap();

        let config = Config {
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
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
        let ignore = IgnoreMatcher::compile(&root, &[]);

        // A real source edit under the target fires.
        let src = root.join("crates/lib.rs");
        assert!(event_is_relevant(&config, &ignore, &mutation_event(src)), "source edit fires");

        // A floor dir (target/) never fires, even though it would be language-matched.
        let built = root.join("target/debug/foo.rs");
        assert!(!event_is_relevant(&config, &ignore, &mutation_event(built)), "floor dir skipped");

        // A gitignored dir under root never fires.
        let generated = root.join("gen/out.rs");
        assert!(
            !event_is_relevant(&config, &ignore, &mutation_event(generated)),
            "gitignored skipped",
        );

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
        let root = std::env::temp_dir().join(format!("ragrat-watchgi-{}-{id}", std::process::id()));
        std::fs::create_dir_all(root.join("crates")).unwrap();
        // Initially nothing is gitignored.
        std::fs::write(root.join(".gitignore"), "").unwrap();

        let config = Config {
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
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
        assert!(
            event_is_relevant(&config, &ignore, &mutation_event(other)),
            "unignored still fires"
        );

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
        let wt = std::env::temp_dir().join(format!("ragrat-wtgi-{}-{id}", std::process::id()));
        std::fs::create_dir_all(wt.join("crates")).unwrap();
        let wt = wt.canonicalize().unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&wt)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git init failed (git must be on PATH)");
        std::fs::write(wt.join(".gitignore"), "").unwrap();

        let sub = wt.join("crates"); // config.root is the subdirectory.
        let target_dirs = vec![PathBuf::from(".")];
        let config = Config {
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
    fn debounce_fires_after_quiet_window() {
        let mut d = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
        let t0 = Instant::now();
        d.on_event(t0);
        assert!(!d.should_fire(t0 + Duration::from_millis(399)), "fires before quiet window");
        assert!(d.should_fire(t0 + Duration::from_millis(400)), "fires at quiet window");
    }

    #[test]
    fn debounce_max_latency_cap_beats_sustained_events() {
        let debounce = Duration::from_millis(400);
        let max = Duration::from_millis(2500);
        let mut d = Debounce::new(debounce, max);
        let t0 = Instant::now();
        d.on_event(t0);
        // A steady stream of events every 200ms keeps the quiet window from ever elapsing...
        let mut now = t0;
        for _ in 0..100 {
            now += Duration::from_millis(200);
            d.on_event(now);
            if now >= t0 + max {
                break;
            }
            assert!(!d.should_fire(now), "should not fire mid-stream before the cap");
        }
        // ...but the max-latency cap forces a fire at first + max_latency regardless.
        assert!(
            d.should_fire(t0 + max),
            "max-latency cap must force a pass under sustained writes"
        );
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
    fn debounce_idle_has_no_deadline() {
        let d = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
        assert!(d.due_in(Instant::now()).is_none());
        assert!(!d.should_fire(Instant::now()));
    }

    #[test]
    fn reconcile_budget_is_shared_across_overlays_and_base() {
        // #219 review: each overlay reconcile (and the base) starts its OWN `max_seconds` timer, so
        // handing every one the same options lets the pass spend (N+1)× the advertised budget.
        // `next_options` recomputes `max_seconds` from the time remaining in the shared budget.
        let options = ReconcileOptions { max_seconds: Some(30), ..ReconcileOptions::default() };
        // A budget whose clock STARTED 30s ago is already exhausted → skip the reconcile.
        let spent = ReconcileBudget::new(
            options.clone(),
            Instant::now() - std::time::Duration::from_secs(30),
        );
        assert!(spent.next_options().is_none(), "an exhausted budget yields no reconcile");

        // A fresh budget yields options whose `max_seconds` is at most the total (the remaining
        // time), never a fresh full budget per call.
        let fresh = ReconcileBudget::new(options, Instant::now());
        let next = fresh.next_options().expect("a fresh budget has time left");
        assert!(
            next.max_seconds.is_some_and(|s| s <= 30),
            "the per-call budget is bounded by the time remaining, not a fresh full budget: {:?}",
            next.max_seconds,
        );

        // An uncapped budget (`max_seconds: None`) always yields the base options.
        let uncapped = ReconcileBudget::new(ReconcileOptions::default(), Instant::now());
        assert_eq!(uncapped.next_options().and_then(|o| o.max_seconds), None);
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
        let main = std::env::temp_dir().join(format!("ragrat-wwt-{}-{id}", std::process::id()));
        std::fs::create_dir_all(main.join("crate/src")).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap()
        };
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@e"]);
        git(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("crate/src/lib.rs"), "pub fn f() {}\n").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);

        let sub = main.join("crate").canonicalize().unwrap(); // config.root is the subdir.
        let config = Config {
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
        let main_id = crate::index::worktree_id_of(&main.canonicalize().unwrap());
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
        let wt = std::env::temp_dir().join(format!("ragrat-wdirs-{}-{id}", std::process::id()));
        std::fs::create_dir_all(wt.join("crates/app")).unwrap();
        let wt = wt.canonicalize().unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&wt)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git init failed (git must be on PATH)");

        let sub = wt.join("crates/app");
        let dirs = gitignore_watch_dirs(&sub);
        // The chain from the worktree root down to config.root, inclusive.
        assert_eq!(dirs.first(), Some(&wt), "worktree root is watched (finding 1)");
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
        let root =
            std::env::temp_dir().join(format!("ragrat-wdirs-ng-{}-{id}", std::process::id()));
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
        let wt = std::env::temp_dir().join(format!("ragrat-deliv-{}-{id}", std::process::id()));
        std::fs::create_dir_all(wt.join("crates")).unwrap();
        let wt = wt.canonicalize().unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&wt)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git init failed (git must be on PATH)");
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
        watch_tree_pruned(&mut w, &sub, &ignore);
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
        let root =
            std::env::temp_dir().join(format!("ragrat-watch-loop-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let status =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        };
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "t@e"]);
        git(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join(".gitignore"), "").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "seed"]);
        let root = root.canonicalize().unwrap();

        let mut config = whole_root_config(&root, &[PathBuf::from("src")]);
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

    /// Drain notify events for up to `secs` seconds; return whether any event references a path
    /// under `needle`. Shared by the issue-#331 placement tests below.
    fn drain_until_path_under(
        rx: &std::sync::mpsc::Receiver<notify::Result<Event>>,
        needle: &Path,
        secs: u64,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Ok(event)) =>
                    if event.paths.iter().any(|p| p.starts_with(needle)) {
                        return true;
                    },
                Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {},
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
        false
    }

    /// Drain real-watcher setup noise until the channel stays quiet for `quiet_ms`, capped by
    /// `max_ms`, so negative placement probes only observe events from the mutation under test.
    #[cfg(target_os = "linux")]
    fn drain_until_quiet(
        rx: &std::sync::mpsc::Receiver<notify::Result<Event>>,
        quiet_ms: u64,
        max_ms: u64,
    ) {
        let quiet = Duration::from_millis(quiet_ms);
        let deadline = Instant::now() + Duration::from_millis(max_ms);
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(quiet.min(remaining)) {
                Ok(_) => {},
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
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
        let root = std::env::temp_dir().join(format!("ragrat-331ign-{}-{id}", std::process::id()));
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
        watch_tree_pruned(&mut w, &root, &ignore);
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
        let root = std::env::temp_dir().join(format!("ragrat-331new-{}-{id}", std::process::id()));
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
        let config = whole_root_config(&root, &target_dirs);
        let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
        watch_tree_pruned(&mut w, &root, &ignore);

        // Create a NEW non-ignored directory after the initial placement.
        let fresh = root.join("fresh_dir");
        std::fs::create_dir_all(&fresh).unwrap();
        // Feed the create event through the same handler watcher_main runs, which places the watch.
        let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(fresh.clone());
        watch_created_dirs(&mut w, &create, &config, &target_dirs, &mut ignore, None);

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
        let root = std::env::temp_dir().join(format!("ragrat-332rel-{}-{id}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let root = root.canonicalize().unwrap();
        let config = Config {
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
            "the FILE under it IS relevant — but its event only arrives if src/foo was watched \
             first",
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
        let root = std::env::temp_dir().join(format!("ragrat-332mv-{}-{id}", std::process::id()));
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
        let config = whole_root_config(&root, &target_dirs);
        let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
        watch_tree_pruned(&mut w, &root, &ignore);
        // Simulate `mv` landing a directory into the target: create it, then feed a rename-To
        // event.
        let moved = root.join("moved_pkg");
        std::fs::create_dir_all(&moved).unwrap();
        let rename =
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To))).add_path(moved.clone());
        watch_created_dirs(&mut w, &rename, &config, &target_dirs, &mut ignore, None);
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
        let root = std::env::temp_dir().join(format!("ragrat-332re-{}-{id}", std::process::id()));
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
        watch_tree_pruned(&mut w, &root, &ignore);
        // Relax the rule, then recompile + RE-PLACE (what the loop now does on a `.gitignore`
        // edit).
        std::fs::write(root.join(".gitignore"), "").unwrap();
        let ignore = IgnoreMatcher::compile(&root, &[PathBuf::from(".")]);
        watch_tree_pruned(&mut w, &root, &ignore);
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
        let root = std::env::temp_dir().join(format!("ragrat-332sym-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join(".gitignore"), "").unwrap();
        // A real directory OUTSIDE the target, with a file in it — the symlink's target.
        let outside = std::env::temp_dir().join(format!("ragrat-332symtgt-{}-{id}", id));
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
        let config = whole_root_config(&root, &target_dirs);
        let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
        watch_tree_pruned(&mut w, &root, &ignore);

        // Symlink the outside dir UNDER the target, then feed its create event.
        let link = root.join("linked");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(link.clone());
        watch_created_dirs(&mut w, &create, &config, &target_dirs, &mut ignore, None);

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
        let root = std::env::temp_dir().join(format!("ragrat-332nt-{}-{id}", std::process::id()));
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
        let config = whole_root_config(&root, &target_dirs);
        let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
        // Watch the target subtree + (mirroring watcher_main) config.root itself non-recursively,
        // so a top-level create is delivered here exactly as it would be in production.
        watch_tree_pruned(&mut w, &root.join("src"), &ignore);
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
        watch_created_dirs(&mut w, &vendor_ev, &config, &target_dirs, &mut ignore, None);
        std::fs::write(vendor_sub.join("v.rs"), "// v\n").unwrap();
        let vendor_seen = drain_until_path_under(&rx, &vendor_sub, 2);

        // A new dir UNDER the target: must be watched.
        let pkg = root.join("src/pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let pkg_ev = Event::new(EventKind::Create(CreateKind::Folder)).add_path(pkg.clone());
        watch_created_dirs(&mut w, &pkg_ev, &config, &target_dirs, &mut ignore, None);
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
        let root = std::env::temp_dir().join(format!("ragrat-332nest-{}-{id}", std::process::id()));
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
        let config = whole_root_config(&root, &target_dirs);
        let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
        watch_tree_pruned(&mut w, &root, &ignore);
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
        watch_created_dirs(&mut w, &rename, &config, &target_dirs, &mut ignore, None);
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
}
