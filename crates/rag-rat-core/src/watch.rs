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
use crate::index::ignore_rules::IgnoreMatcher;
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
    let (mut db, content_changed) = IndexDatabase::index_discover_reporting(config)?;
    // Keep every live linked worktree's branch overlay fresh (#219), so a `worktree`-scoped query
    // sees that branch's changes without a manual `index --worktree`. Delta-only and idle-safe (the
    // overlay pass writes nothing when a worktree is unchanged), so it can run every pass; a
    // worktree change counts toward running the tail below even when config.root itself didn't
    // change.
    let overlays_changed = refresh_worktree_overlays(&mut db, config);
    // Idle backstop (issue #63, facet 2): when the sweep changed no content, skip the reconcile /
    // gc / memory-validate tail — an idle server should do no work past discovery. `run_gc` (every
    // GC_EVERY_PASSES) still forces a full tail, so the cases that DON'T flip content_changed are
    // still caught within that bound: a freshly-installed embedder, an embedding backlog left by a
    // time-capped reconcile (PASS_RECONCILE_MAX_SECONDS), and drifted memory anchors. Any real
    // content change runs the full tail immediately.
    if !content_changed && !overlays_changed && !run_gc {
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
        let _ = db.garbage_collect();
    }
    let _ = db.memory_validate();
    Ok(())
}

/// Refresh the branch overlay of every live LINKED worktree of `config.root`'s repo (#219), so a
/// `worktree`-scoped query stays current without a manual `index --worktree`. Returns whether any
/// overlay actually changed. `index_worktree_overlay` is delta-only and idle-safe (a static
/// worktree writes nothing), and the connection is restored to the base scope afterward so the rest
/// of the pass (reconcile / gc / memory-validate) runs unscoped as before. Best-effort per worktree
/// — a failure on one worktree is logged and doesn't abort the pass.
fn refresh_worktree_overlays(db: &mut IndexDatabase, config: &Config) -> bool {
    let (_, worktrees) = crate::index::live_worktree_contexts(&config.root);
    let base_id = crate::index::worktree_id_of(&config.root);
    let mut changed = false;
    for worktree in worktrees {
        if worktree == base_id {
            continue; // the rooted checkout is the base scope, not an overlay
        }
        match db.index_worktree_overlay(config, Path::new(&worktree), &mut |_| {}) {
            Ok(report) => {
                changed |= report.indexed > 0 || report.tombstoned > 0 || report.pruned > 0;
            },
            Err(err) => eprintln!("watch: worktree overlay refresh failed for {worktree}: {err}"),
        }
    }
    // Restore the base scope for the rest of the pass (index_worktree_overlay leaves the connection
    // scoped to the last worktree it touched).
    let _ = db.use_worktree_scope(&config.root, None);
    changed
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
    let base = crate::index::git_history::worktree_root(root).filter(|wt| root.starts_with(wt));
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
    // Round-3 finding 1: when `config.root` is a subdirectory of a larger Git worktree, the target
    // dirs are below `config.root`, so a watch scoped to them never sees an edit to the
    // *worktree-root* (or any ancestor) `.gitignore` that lives ABOVE them — those root-rule
    // changes would never recompile the matcher. Subscribe (non-recursively, to avoid
    // re-watching the whole worktree) to every directory on the ancestor chain from the
    // worktree root down to `config.root`, so a `.gitignore` mutation there is delivered.
    // `gitignore_watch_dirs` returns the chain (always including `config.root`); the
    // non-recursive watch only delivers events for files directly in each dir, which is exactly
    // where each ancestor `.gitignore` sits.
    for dir in gitignore_watch_dirs(&config.root) {
        let _ = notify_watcher.watch(&dir, RecursiveMode::NonRecursive);
    }
    // Compile the repo's `.gitignore` rules for event classification — the same matcher the
    // discover walk uses (issue #62), so an ignored path never fires a pass. Recompiled whenever a
    // `.gitignore` mutation is observed (finding 3) so the running classifier never applies stale
    // rules; each pass's discover walk also compiles its own fresh matcher, so the index itself is
    // always current regardless.
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
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
                if event_is_relevant(&config, &ignore, &event) {
                    debounce.on_event(now);
                    // A `.gitignore` mutation changed the rules — recompile so subsequent events
                    // are classified against current rules, not the matcher
                    // this watcher booted with (finding 3). Cheap relative to a
                    // pass and only runs on actual gitignore edits.
                    if kind_is_mutation(&event.kind)
                        && event.paths.iter().any(|path| is_gitignore_path(path))
                    {
                        ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
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
    use notify::event::{CreateKind, ModifyKind};

    use super::*;
    use crate::config::{
        Config, EmbeddingConfig, LocalAiConfig, ResolvedTarget, TargetKind, WatchConfig,
    };
    use crate::language::Language;

    fn mutation_event(path: PathBuf) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path)
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
            local_ai: LocalAiConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
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
            local_ai: LocalAiConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
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
            local_ai: LocalAiConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
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
        // Subscribe exactly as watcher_main does: the (recursive) target dir + the gitignore chain.
        w.watch(&sub, RecursiveMode::Recursive).unwrap();
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
}
