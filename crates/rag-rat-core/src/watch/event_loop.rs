use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::recommended_watcher;
use rag_rat_base::config::Config;
use rag_rat_base::locks::{self, FileLock};
use rag_rat_papertrail::AutosyncRequest;

use super::overlay::OverlayScope;
use super::papertrail::{self, PapertrailClock, PapertrailScheduler};
use super::pass::{
    Debounce, LoopMsg, PassCooldown, PassRequest, PassScheduler, SKIP_TIMEOUT, SweepClock,
    maintenance_pass_scoped, spawn_pass_worker, startup_catchup_pass,
};
use super::placement::{
    LinkedWorktreeWatches, WatchPlacementCounters, event_requests_maintenance,
    event_targets_binary, is_gitignore_path, kind_is_mutation, place_initial_watch_state,
    recompile_ignore_and_place_watches, sync_linked_worktrees_after_pass,
};
use crate::fleet;
use crate::index::ignore_rules::IgnoreMatcher;
use crate::index::{IndexDatabase, papertrail_autosync as autosync};

pub(crate) const FLEET_DEBOUNCE: Duration = Duration::from_millis(500);
pub(crate) const FLEET_MAX_LATENCY: Duration = Duration::from_millis(2000);
pub(crate) const IDLE_WAIT: Duration = Duration::from_millis(500);

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
    //
    // Watch-placement counters are OWNED by this watcher (not a process-global), so one repo
    // config's dropped watches are never attributed to another (#658 review). The event-loop thread
    // records placements into them; the pass worker reads them to persist the failure high-water
    // mark — hence the `Arc`, shared across the two threads.
    let watch_counters = Arc::new(WatchPlacementCounters::default());
    let (mut linked_worktrees, worktree_registry) = place_initial_watch_state(
        &mut notify_watcher,
        &watch_counters,
        &config,
        &target_dirs,
        &ignore,
        fleet_bin.as_deref(),
    );

    // Papertrail auto-sync (#592): its own worker + queue, deliberately separate from the pass
    // worker — a mirror flight waits on the network (pages, rate-governor sleeps) and must
    // neither delay index passes nor be delayed by them. `None` interval (no resolved tracker
    // bindings) disables the trigger entirely. The worker handle is dropped, never joined; see
    // `spawn_papertrail_worker` for why shutdown stays bounded anyway.
    let papertrail_interval = papertrail::papertrail_tick_interval(&config);
    let mut papertrail_tx = None;
    if papertrail_interval.is_some() {
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let spawned = papertrail::spawn_papertrail_worker(request_rx, tx.clone(), {
            let config = config.clone();
            move |request| {
                if let Err(error) = autosync::run(&config, request) {
                    tracing::warn!(
                        target: "rag_rat_core::papertrail",
                        error = %error,
                        "papertrail auto-sync failed; a later trigger retries"
                    );
                }
            }
        });
        if spawned.is_some() {
            papertrail_tx = Some(request_tx);
        }
    }

    // Maintenance passes run on the worker thread (#506); see `spawn_pass_worker` for why.
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let Some(pass_worker) = spawn_pass_worker(pass_rx, tx, {
        let config = config.clone();
        // This watcher's counters, shared with the event-loop thread that records into them; the
        // pass reads them to persist the placement-failure high-water mark for `index_status`.
        let watch_counters = Arc::clone(&watch_counters);
        move |request| {
            let _ = match request {
                PassRequest::StartupCatchup =>
                    startup_catchup_pass(&config, Some(watch_counters.as_ref())),
                PassRequest::Maintenance { run_gc, overlay_scope } => maintenance_pass_scoped(
                    &config,
                    *run_gc,
                    overlay_scope,
                    Some(watch_counters.as_ref()),
                ),
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
        counters: &watch_counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: worktree_registry.as_deref(),
        rx,
        pass_tx: &pass_tx,
        scheduler: &mut scheduler,
        papertrail_tx: papertrail_tx.as_ref(),
        papertrail_interval,
        stop,
        fleet_trigger: &mut fire_fleet_trigger,
    }
    .run();

    // Let an in-flight pass finish (bounded by the pass itself, exactly as when passes ran
    // inline), then release the worker. The papertrail worker is only released (its channel
    // closed), never joined — a mirror flight can be network-bound far past any shutdown grace.
    drop(papertrail_tx);
    drop(pass_tx);
    let _ = pass_worker.join();

    // Final pass for edits in the last debounce window — discover only (no embedding), timeout-and-
    // skip. The host may SIGKILL shortly after stdin EOF, so shutdown must be bounded; discover is
    // fast and keeps structure fresh, and the next content-changing or periodic pass does the
    // embedding.
    if final_refresh_owed {
        let _ = shutdown_discover(&config);
    }

    // Flush the watch-placement failure high-water mark on the way out, in case the event loop's
    // between-pass drain didn't catch the last resync-introduced drop before `stop`. Runs AFTER the
    // shutdown discover on purpose: a watcher that started with NO index has a drain that treated
    // the missing DB as settled, so if the discover above just CREATED and registered the index for
    // content that arrived in the last debounce window, this is the only chance to write the count
    // into it (#658 review). Best-effort and bounded; a no-op (no DB open at all) when there is
    // still no index or nothing ever failed, so a healthy watcher's shutdown pays nothing.
    let _ = flush_watch_placement_failures(&config, &watch_counters, SKIP_TIMEOUT);
}

/// Persist this watcher's watch-placement failure high-water mark out of band — from the event-loop
/// drain between passes (the post-pass linked-worktree resync places watches AFTER the pass worker
/// already persisted the counter) and at shutdown — so a drop introduced there is not lost on a
/// periodic-sweep-disabled watcher with no further events (#658 review). Takes the write lock with
/// `lock_timeout` (`Duration::ZERO` for a non-blocking try on the event loop — it must not stall
/// event classification; [`SKIP_TIMEOUT`] at shutdown). The persist goes through the LIGHTWEIGHT,
/// NON-CREATING, NON-BLOCKING config-scoped path
/// ([`rag_rat_db::meta::record_watch_placement_failures_scoped`]).
///
/// Returns whether the flush SETTLED: `true` = persisted, or nothing to persist (no failures, no
/// index yet, repo not registered) — the caller may advance its low-water mark; `false` = a
/// TRANSIENT skip (write lock held, or the DB busy) — the caller should retry on a later tick.
pub(crate) fn flush_watch_placement_failures(
    config: &Config,
    counters: &WatchPlacementCounters,
    lock_timeout: Duration,
) -> bool {
    let (_, failures) = counters.counts();
    if failures == 0 {
        return true;
    }
    let lock_repo = locks::write_lock_repo_id(config);
    let Ok(Some(_lock)) =
        locks::WriteLock::acquire_timeout(&config.database, &lock_repo, lock_timeout)
    else {
        // The flock is held (a pass mid-write, another process) — transient; retry next tick.
        return false;
    };
    match rag_rat_db::meta::record_watch_placement_failures_scoped(config, failures) {
        Ok(settled) => settled,
        Err(error) => {
            // A non-busy error (schema corruption, a vanished file) can't be fixed by retrying —
            // treat as settled so the drain doesn't spin/log every tick; the next full pass
            // persists via its own connection.
            tracing::debug!(
                target: "rag_rat_core::watch",
                error = %error,
                "watch-placement-failure flush failed; the count rides the next pass"
            );
            true
        },
    }
}

/// The watcher event loop, separated from [`watcher_main`] so tests can drive it with a recording
/// notify watcher and play the pass worker themselves. The invariant it exists to enforce (#506):
/// this thread NEVER runs a maintenance pass inline — it hands requests to the worker over
/// `pass_tx` and keeps classifying events and serving the fleet trigger while the pass runs.
pub(crate) struct EventLoop<'a, W: notify::Watcher> {
    pub(crate) config: &'a Config,
    pub(crate) target_dirs: &'a [PathBuf],
    pub(crate) fleet_bin: Option<&'a Path>,
    pub(crate) notify_watcher: &'a mut W,
    /// This watcher's watch-placement counters (not a process-global), recorded on every placement
    /// the loop performs and read by the pass worker to persist the failure high-water mark
    /// (#658).
    pub(crate) counters: &'a WatchPlacementCounters,
    pub(crate) ignore: &'a mut IgnoreMatcher,
    pub(crate) linked_worktrees: &'a mut LinkedWorktreeWatches,
    pub(crate) worktree_registry: Option<&'a Path>,
    pub(crate) rx: Receiver<LoopMsg>,
    pub(crate) pass_tx: &'a Sender<PassRequest>,
    pub(crate) scheduler: &'a mut PassScheduler,
    /// `None` disables the papertrail trigger (no tracker bindings, or no worker thread).
    pub(crate) papertrail_tx: Option<&'a Sender<AutosyncRequest>>,
    pub(crate) papertrail_interval: Option<Duration>,
    pub(crate) stop: &'a AtomicBool,
    /// Injected so tests can observe the fleet firing; production wires [`fleet::trigger`].
    pub(crate) fleet_trigger: &'a mut (dyn FnMut(&Path) + Send),
}

impl<W: notify::Watcher> EventLoop<'_, W> {
    /// Run until `stop`; returns whether a final shutdown refresh is owed (the debounce is still
    /// armed — events arrived after the last dispatched pass).
    pub(crate) fn run(self) -> bool {
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
        // Minimum inter-pass cooldown (#823): the next event-driven pass dispatches no sooner
        // than this long after the previous pass completed, so sustained editing can't run
        // passes back-to-back off a debounce that elapsed mid-pass. Watcher-event-loop-only:
        // the startup catch-up (dispatched before this loop) and the hook/CLI
        // `maintenance_pass*` entry points are not rate-limited.
        let mut cooldown = PassCooldown::new(
            (self.config.watch.pass_cooldown_secs > 0)
                .then(|| Duration::from_secs(self.config.watch.pass_cooldown_secs)),
        );
        // The papertrail evaluation deadline (#592): fires even on a filesystem-idle watcher, so
        // the freshness probe and the daily full-walk backstop run on time without any events.
        let mut papertrail_clock =
            PapertrailClock::new(self.papertrail_tx.and(self.papertrail_interval), Instant::now());
        let mut papertrail_scheduler = PapertrailScheduler::new();
        // The overlay scope accumulated while the debounce is armed (#577): every firing event
        // merges its contribution, and the union rides the next dispatched pass. Cleared ONLY on
        // dispatch, like the debounce itself — mid-pass events keep accumulating for the
        // coalesced follow-up.
        let mut pending_overlay_scope: Option<OverlayScope> = None;
        // The watch-placement failure count this loop has already flushed to `repo_meta`. The
        // between-pass drain at the loop tail compares the live counter against this and persists +
        // warns on any rise, so a drop the post-pass resync introduced (after the pass worker
        // persisted) still surfaces — in `index_status` AND the log — on a periodic-sweep-disabled
        // watcher with no further events (#658 review).
        let mut last_flushed_placement_failures = 0u64;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            let now = Instant::now();
            // While a pass is in flight its fire condition stays due (the debounce only resets on
            // dispatch), so recomputing those deadlines would spin the loop — the `PassDone`
            // message is what wakes it. The fleet debounce still shapes the wait: the trigger
            // must fire DURING a long pass, not after it (#506). The papertrail deadline shapes
            // BOTH branches for the same reason: an in-flight maintenance pass must not postpone
            // a due probe (#592).
            let wait = if self.scheduler.in_flight() {
                [fleet_debounce.due_in(now), papertrail_clock.due_in(now)]
                    .into_iter()
                    .flatten()
                    .min()
                    .unwrap_or(IDLE_WAIT)
            } else {
                [
                    // The debounce deadline is floored by the cooldown's remaining time (#823):
                    // a debounce that elapsed during the pass stays fireable while dispatch is
                    // held back, so an unfloored deadline would wake the loop every iteration —
                    // the same spin the in-flight branch above avoids. The sweep deadline is
                    // deliberately NOT floored: the periodic backstop overrides the cooldown, so
                    // its wake must arrive on time.
                    cooldown.gate_debounce_wait(&debounce, now),
                    fleet_debounce.due_in(now),
                    sweep.due_in(now),
                    papertrail_clock.due_in(now),
                ]
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
                        self.counters,
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
                                self.counters,
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
                    let done_at = Instant::now();
                    sweep.on_pass_done(done_at);
                    // The cooldown counts from pass COMPLETION (#823) — the armed debounce below
                    // is typically long-elapsed by now, and without this the next iteration
                    // would dispatch the follow-up immediately, back-to-back.
                    cooldown.on_pass_done(done_at);
                    // Refresh the live linked-worktree set after every pass. Existing checkout
                    // paths can switch branches and therefore branch-local target sets; rebuilding
                    // the state keeps classification, ignore matchers, and watch placement in one
                    // place. Removed checkout paths can still have stale backend watches
                    // (harmless; their overlay is GC-pruned), but no stale `LinkedWorktreeWatch`
                    // state remains active.
                    sync_linked_worktrees_after_pass(
                        self.notify_watcher,
                        self.counters,
                        self.config,
                        self.linked_worktrees,
                    );
                    // A resync-introduced watch-placement drop is picked up by the between-pass
                    // drain at the tail of this loop (it runs every iteration), not here — see
                    // there.
                },
                Ok(LoopMsg::PapertrailDone) => {
                    if let Some(request_tx) = self.papertrail_tx
                        && let Some(follow_up) = papertrail_scheduler.on_done()
                    {
                        let _ = request_tx.send(follow_up);
                    }
                },
                Ok(LoopMsg::Wake) => {},
                Err(RecvTimeoutError::Timeout) => {},
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let now = Instant::now();
            // The papertrail tick enqueues a schedule EVALUATION, never an unconditional mirror:
            // the per-binding policy decides skip / probe / incremental / full, and the daily
            // full-walk backstop rides the same evaluation. Independent of the debounce and of
            // `scheduler.in_flight()`, so ordinary maintenance never delays it; redundant ticks
            // coalesce in `PapertrailScheduler` (and cross-process in the flight's pending
            // marker).
            if papertrail_clock.due(now) {
                papertrail_clock.on_tick(now);
                if let Some(request_tx) = self.papertrail_tx
                    && let Some(request) = papertrail_scheduler.admit(AutosyncRequest::Evaluate)
                {
                    let _ = request_tx.send(request);
                }
            }
            let periodic_due = sweep.due(now);
            // The cooldown (#823) holds back only the DEBOUNCE-driven dispatch; a due periodic
            // sweep dispatches regardless — the missed-event backstop is never starved by the
            // cooldown.
            if periodic_due || (debounce.should_fire(now) && cooldown.ready(now)) {
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
            // Between-pass watch-placement-failure drain (#658 review). Runs every loop iteration —
            // the loop wakes at least every `IDLE_WAIT`, so even a periodic-sweep-disabled watcher
            // with no further filesystem events reaches here within one idle tick. Gated on a RISE
            // in the counter (two atomic loads on the healthy path — negligible), so it opens the
            // DB only when a placement newly failed. The pass worker persists at pass
            // start, but the post-pass linked-worktree resync places watches AFTER
            // that; this is what surfaces such a drop while the watcher keeps running.
            // The `warn!` is emitted here (once per new batch via the coalesced
            // counter) so the log signal fires even when the flush is deferred, and the
            // flush is NON-blocking: a busy DB / held write lock leaves `last_flushed` unadvanced
            // so the next tick retries. It never dispatches a pass, so it cannot re-drive the
            // resync and loop.
            let (_, current_placement_failures) = self.counters.counts();
            if current_placement_failures > last_flushed_placement_failures {
                if let Some(total) = self.counters.newly_warnable_failures() {
                    // The recovery path differs by config: with the periodic sweep ON, the
                    // unwatched subtree is re-scanned each interval; with it DISABLED
                    // (`periodic_sweep_secs = 0`) there is no backstop, so edits beneath the
                    // dropped watch can go unindexed until an event-driven pass happens to touch it
                    // or the operator reindexes — don't promise a sweep that won't run.
                    let recovery = if self.config.watch.periodic_sweep_secs > 0 {
                        "falling back to the periodic sweep"
                    } else {
                        "and the periodic sweep is DISABLED, so edits beneath an unwatched \
                         directory may go unindexed until the next event-driven pass or a reindex"
                    };
                    tracing::warn!(
                        target: "rag_rat_core::watch",
                        watch_placement_failures = total,
                        periodic_sweep_secs = self.config.watch.periodic_sweep_secs,
                        "watch placement failed for one or more directories ({recovery}); on Linux \
                         this is usually fs.inotify.max_user_watches exhaustion"
                    );
                }
                if flush_watch_placement_failures(self.config, self.counters, Duration::ZERO) {
                    last_flushed_placement_failures = current_placement_failures;
                }
            }
        }
        debounce.fire_at().is_some()
    }
}

/// A bounded shutdown refresh: take the write lock only if free, run discover (no reconcile/embed).
pub(crate) fn shutdown_discover(config: &Config) -> anyhow::Result<bool> {
    let lock_repo = locks::write_lock_repo_id(config);
    match locks::WriteLock::acquire_timeout(&config.database, &lock_repo, SKIP_TIMEOUT)? {
        Some(_lock) => {
            // #427: the core refuses a first-time-empty registration; a shutdown refresh defers on
            // it (same as `run_pass`) rather than creating an empty index on the way out. Otherwise
            // it preserves #460's shutdown-reconcile debt: a content change on the way out marks
            // the base reconcile owed so the next startup pays it down.
            match IndexDatabase::index_discover_reporting(config) {
                Ok((db, pass)) => {
                    if pass.content_changed {
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
