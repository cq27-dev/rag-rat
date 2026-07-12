use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::recommended_watcher;

use super::overlay::OverlayScope;
use super::pass::{
    Debounce, LoopMsg, PassRequest, PassScheduler, SKIP_TIMEOUT, SweepClock,
    maintenance_pass_scoped, spawn_pass_worker, startup_catchup_pass,
};
use super::placement::{
    LinkedWorktreeWatches, event_requests_maintenance, event_targets_binary, is_gitignore_path,
    kind_is_mutation, place_initial_watch_state, recompile_ignore_and_place_watches,
    sync_linked_worktrees_after_pass,
};
use crate::config::Config;
use crate::fleet;
use crate::index::IndexDatabase;
use crate::index::ignore_rules::IgnoreMatcher;
use crate::locks::{self, FileLock};

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
pub(crate) struct EventLoop<'a, W: notify::Watcher> {
    pub(crate) config: &'a Config,
    pub(crate) target_dirs: &'a [PathBuf],
    pub(crate) fleet_bin: Option<&'a Path>,
    pub(crate) notify_watcher: &'a mut W,
    pub(crate) ignore: &'a mut IgnoreMatcher,
    pub(crate) linked_worktrees: &'a mut LinkedWorktreeWatches,
    pub(crate) worktree_registry: Option<&'a Path>,
    pub(crate) rx: Receiver<LoopMsg>,
    pub(crate) pass_tx: &'a Sender<PassRequest>,
    pub(crate) scheduler: &'a mut PassScheduler,
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
pub(crate) fn shutdown_discover(config: &Config) -> anyhow::Result<bool> {
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
