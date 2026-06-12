//! Background file watcher: keeps the active index (and dirty-worktree overlay) fresh as files
//! change, so graph/symbol queries reflect uncommitted edits without waiting for a commit.
//!
//! - **One watcher per worktree** via the election lock; **one writer at a time per DB** via the
//!   write lock (see [`crate::locks`]).
//! - Watches the configured target *directories* recursively (so **new files** are seen),
//!   classifies events through the target globs to decide whether to fire, and debounces bursts
//!   with a max-latency cap so sustained writes can't starve a pass.
//! - Each pass runs the existing pipeline: discover → reconcile → (rate-limited) gc →
//!   memory_validate. Discover handles additions/edits/deletions; the pass is idempotent.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{Event, RecursiveMode, Watcher as _, recommended_watcher};

use crate::config::Config;
use crate::fleet;
use crate::index::ai::ReconcileOptions;
use crate::index::{IndexDatabase, target_for_path};
use crate::locks::{self, FileLock};

/// Run gc on every Nth watcher pass (deletion reconciliation is already handled by discover, so gc
/// — which shells to `git worktree list` + a liveness scan — need not run every keystroke burst).
const GC_EVERY_PASSES: u64 = 20;
/// Bound a single reconcile so a pass never holds the write lock indefinitely.
const PASS_RECONCILE_MAX_SECONDS: u64 = 60;
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
    let lock_path = locks::write_lock_path(&config.database);
    let _lock = FileLock::acquire_blocking(&lock_path)?;
    run_pass(config, run_gc)
}

/// Run one maintenance pass only if the write lock is free within `SKIP_TIMEOUT`; returns whether
/// it ran. Used by interactive / hook / shutdown callers so a held lock can't hang them.
pub fn maintenance_pass_or_skip(config: &Config, run_gc: bool) -> anyhow::Result<bool> {
    let lock_path = locks::write_lock_path(&config.database);
    match FileLock::acquire_timeout(&lock_path, SKIP_TIMEOUT)? {
        Some(_lock) => {
            run_pass(config, run_gc)?;
            Ok(true)
        },
        None => Ok(false),
    }
}

fn run_pass(config: &Config, run_gc: bool) -> anyhow::Result<()> {
    let (db, content_changed) = IndexDatabase::index_discover_reporting(config)?;
    // Idle backstop (issue #63, facet 2): when the sweep changed no content, skip the reconcile /
    // gc / memory-validate tail — an idle server should do no work past discovery. `run_gc` (every
    // GC_EVERY_PASSES) still forces a full tail, so the cases that DON'T flip content_changed are
    // still caught within that bound: a freshly-installed embedder, an embedding backlog left by a
    // time-capped reconcile (PASS_RECONCILE_MAX_SECONDS), and drifted memory anchors. Any real
    // content change runs the full tail immediately.
    if !content_changed && !run_gc {
        return Ok(());
    }
    let runtime = &config.local_ai.embedding.runtime;
    let options = ReconcileOptions {
        batch_size: Some(runtime.batch_size),
        changed_first: true,
        max_seconds: Some(PASS_RECONCILE_MAX_SECONDS),
        max_embedding_chars: runtime.max_embedding_chars,
        intra_threads: runtime.ort_threads.map(|n| n as usize),
        ..ReconcileOptions::default()
    };
    db.reconcile_with_options_progress(options, |_| {})?;
    if run_gc {
        let _ = db.gc();
    }
    let _ = db.memory_validate();
    Ok(())
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

/// A relevant event is a rescan/overflow notice, or a *content-mutating* event whose path matches a
/// configured target — classification only decides *whether to fire a pass*, not what to index (the
/// discover sweep does that). Ignored paths (`.rag-rat/`, `target/`, …) never match a target, so
/// they never fire; read-only events never fire regardless of path (see [`kind_is_mutation`]).
fn event_is_relevant(config: &Config, event: &Event) -> bool {
    if event.need_rescan() {
        return true;
    }
    if !kind_is_mutation(&event.kind) {
        return false;
    }
    event.paths.iter().any(|path| {
        path.strip_prefix(&config.root)
            .ok()
            .is_some_and(|relative| target_for_path(config, relative).is_some())
    })
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
    let _ = maintenance_pass(&config, true);

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut notify_watcher) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        return;
    };
    for target in &config.targets {
        for dir in &target.directories {
            let _ = notify_watcher.watch(&config.root.join(dir), RecursiveMode::Recursive);
        }
    }
    // Fleet hot-upgrade: also watch the installed binary's directory so a new `cargo install`
    // rename triggers a fleet-wide upgrade. Watch the directory (not the file) so the atomic
    // rename — which replaces the inode — is still observed.
    let fleet_dir = fleet_bin.as_ref().and_then(|bin| bin.parent());
    if let Some(dir) = fleet_dir {
        let _ = notify_watcher.watch(dir, RecursiveMode::NonRecursive);
    }

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
                if event_is_relevant(&config, &event) {
                    debounce.on_event(now);
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
    // fast and keeps structure fresh, and the next session's startup catch-up does the embedding.
    if debounce.fire_at().is_some() {
        let _ = shutdown_discover(&config);
    }
}

/// A bounded shutdown refresh: take the write lock only if free, run discover (no reconcile/embed).
fn shutdown_discover(config: &Config) -> anyhow::Result<bool> {
    let lock_path = locks::write_lock_path(&config.database);
    match FileLock::acquire_timeout(&lock_path, SKIP_TIMEOUT)? {
        Some(_lock) => {
            IndexDatabase::index_discover(config)?;
            Ok(true)
        },
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
