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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind, RenameMode};
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
    let _lock = locks::WriteLock::acquire_blocking(&config.database)?;
    run_pass(config, run_gc)
}

/// Run one maintenance pass only if the write lock is free within `SKIP_TIMEOUT`; returns whether
/// it ran. Used by interactive / hook / shutdown callers so a held lock can't hang them.
pub fn maintenance_pass_or_skip(config: &Config, run_gc: bool) -> anyhow::Result<bool> {
    match locks::WriteLock::acquire_timeout(&config.database, SKIP_TIMEOUT)? {
        Some(_lock) => {
            run_pass(config, run_gc)?;
            Ok(true)
        },
        None => Ok(false),
    }
}

fn run_pass(config: &Config, run_gc: bool) -> anyhow::Result<()> {
    let started = Instant::now();
    let (mut db, content_changed) = IndexDatabase::index_discover_reporting(config)?;
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
    let overlays_changed = refresh_worktree_overlays(&mut db, config, Some(&budget));
    // Idle backstop (issue #63, facet 2): when the sweep changed no content, skip the reconcile /
    // gc / memory-validate tail — an idle server should do no work past discovery. `run_gc` (every
    // GC_EVERY_PASSES) still forces a full tail, so the cases that DON'T flip content_changed are
    // still caught within that bound: a freshly-installed embedder, an embedding backlog left by a
    // time-capped reconcile (PASS_RECONCILE_MAX_SECONDS), and drifted memory anchors. Any real
    // content change runs the full tail immediately.
    if !content_changed && !overlays_changed && !run_gc {
        return Ok(());
    }
    // The base reconcile gets only the budget the overlays left behind; `None` → already exhausted,
    // so skip it (the embedding backlog rides the next pass) rather than spend a fresh full budget.
    if let Some(options) = budget.next_options() {
        db.reconcile_with_options_progress(options, |_| {})?;
    }
    // Clone-edge graph (#286): refresh the persisted graph when it's ABSENT or STALE, with whatever
    // budget the embedding reconcile left — sharing the same PASS_RECONCILE_MAX_SECONDS so a pass
    // can't overrun. Best-effort + resumable: a bounded pass makes partial progress and the next
    // pass continues, so a large/dense repo's graph converges over several passes, entirely off
    // the query path. `None` budget → skip (rides the next pass), exactly like the base
    // reconcile.
    if db.pending_clone_graph().unwrap_or(false)
        && let Some(options) = budget.next_options()
    {
        let _ = db.reconcile_clone_edges_with_budget(options.max_seconds);
    }
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
                let needs_embed = this_changed
                    || reconcile.is_some()
                        && db.pending_embedding_jobs().is_ok_and(|pending| pending > 0);
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

/// Whether `event` should fire a pass for the LINKED-worktree layer (#219): a content mutation to a
/// configured target inside a linked worktree checkout (its overlay needs refreshing), or any
/// change in the worktree registry (`<common_dir>/worktrees`, i.e. a worktree add/remove). Separate
/// from [`event_is_relevant`] so the base-tree classification — and its tests — stay untouched. The
/// `config.root` ignore matcher doesn't govern paths outside `config.root`, so a worktree path is
/// matched by the target globs alone (the overlay pass does its own delta + target filtering).
fn event_touches_worktree(
    config: &Config,
    event: &Event,
    worktree_roots: &[PathBuf],
    registry: Option<&Path>,
) -> bool {
    if !kind_is_mutation(&event.kind) {
        return false;
    }
    // `config.root` may be a SUBDIR of the repo. A linked checkout mirrors that layout, so its
    // event paths are `<checkout_root>/<config_subdir>/<target>/…`. After stripping the
    // checkout root the remainder is still `<config_subdir>/<target>/…`, but `target_for_path`
    // expects a CONFIG-ROOT- relative path (`<target>/…`) — so a subdir-rooted config would
    // reject every linked edit and never debounce an overlay refresh. Strip the config subdir
    // too (#219 review).
    let config_subdir = config_subdir_prefix(config);
    event.paths.iter().any(|path| {
        if registry.is_some_and(|reg| path.starts_with(reg)) {
            return true;
        }
        // A `.gitignore` edit in the linked checkout changes its ignored/unignored set, so the
        // overlay must refresh — mirror the base classifier's `.gitignore` handling (#219 review).
        // Bound it to the watched checkout roots so an unrelated repo's `.gitignore` is ignored.
        if is_gitignore_path(path) && worktree_roots.iter().any(|root| path.starts_with(root)) {
            return true;
        }
        worktree_roots.iter().any(|root| {
            let Ok(rel) = path.strip_prefix(root) else {
                return false;
            };
            let rel = rel.strip_prefix(&config_subdir).unwrap_or(rel);
            target_for_path(config, rel).is_some()
        })
    })
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

/// Whether `path` is inside a configured target directory (so it could ever satisfy
/// `target_for_path`). `config.root` itself is watched non-recursively (the `gitignore_watch_dirs`
/// ancestor chain), so a top-level create OUTSIDE a target — `vendor/`, a sibling of `src/`, or an
/// ancestor of `config.root` — is also delivered to the event loop; without this gate
/// [`watch_created_dirs`] would watch it even though it can never be indexed, re-exhausting inotify
/// (#332). A whole-root target (`directories = ["."]`) matches any path under `config.root`.
fn dir_under_a_target(config: &Config, target_dirs: &[PathBuf], path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(&config.root) else {
        return false; // outside config.root entirely (an ancestor-chain event).
    };
    target_dirs.iter().any(|d| d == Path::new(".") || rel.starts_with(d))
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
/// 2. **under a target** — [`dir_under_a_target`]; a non-target top-level dir can never be indexed.
/// 3. **not ignored** — by the current matcher.
/// 4. **recompile + re-check** — a directory MOVED in can carry its own nested `.gitignore`, which
///    the long-lived matcher (compiled before that subtree existed) doesn't know about; recompiling
///    here picks the nested rules up so `watch_tree_pruned` prunes against them, not stale rules.
fn watch_created_dirs(
    watcher: &mut impl notify::Watcher,
    event: &Event,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &mut IgnoreMatcher,
) {
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
        return;
    }
    for path in &event.paths {
        // `symlink_metadata` does NOT follow links: a symlink pointing at a directory is reported
        // as a symlink (`is_dir() == false`) → skipped, so we never recurse through it (#332).
        let is_real_dir = std::fs::symlink_metadata(path).map(|m| m.is_dir()).unwrap_or(false);
        if !is_real_dir {
            continue;
        }
        if !dir_under_a_target(config, target_dirs, path) {
            continue;
        }
        if ignore.is_ignored(path, true) {
            continue;
        }
        // A moved-in dir may carry a nested `.gitignore` the long-lived matcher predates; recompile
        // so the subtree is pruned against current (incl. nested) rules, then re-check the root.
        *ignore = IgnoreMatcher::compile(&config.root, target_dirs);
        if ignore.is_ignored(path, true) {
            continue;
        }
        watch_tree_pruned(watcher, path, ignore);
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
    for target in &config.targets {
        for dir in &target.directories {
            watch_tree_pruned(&mut notify_watcher, &config.root.join(dir), &ignore);
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
    // Fleet hot-upgrade: also watch the installed binary's directory so a new `cargo install`
    // rename triggers a fleet-wide upgrade. Watch the directory (not the file) so the atomic
    // rename — which replaces the inode — is still observed.
    let fleet_dir = fleet_bin.as_ref().and_then(|bin| bin.parent());
    if let Some(dir) = fleet_dir {
        let _ = notify_watcher.watch(dir, RecursiveMode::NonRecursive);
    }
    // Linked worktrees (#219): watch each branch checkout recursively so its edits refresh the
    // overlay, and the worktree registry non-recursively so a `git worktree add`/`remove` fires a
    // pass. `linked_worktrees` is reconciled after each pass to pick up newly-added worktrees.
    // NOTE (#331): these remain `Recursive` — the secondary inotify consumer. Pruning them needs a
    // per-worktree `IgnoreMatcher` (the `config.root` matcher doesn't govern paths outside it, and
    // each branch carries its own targets/`.gitignore`), which #331 scopes out (target dirs first).
    let (mut linked_worktrees, worktree_registry) = worktree_watch_targets(&config);
    for worktree in &linked_worktrees {
        let _ = notify_watcher.watch(worktree, RecursiveMode::Recursive);
    }
    if let Some(registry) = &worktree_registry {
        let _ = notify_watcher.watch(registry, RecursiveMode::NonRecursive);
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
                // Place watches on newly-appeared, non-ignored directories REGARDLESS of relevance
                // (#332). Target dirs are watched NON-recursively (#331), so notify won't
                // auto-descend into a new subdir — and a bare `mkdir src/foo` is NOT
                // `event_is_relevant` (a directory is extensionless → not a target FILE), so gating
                // this on the relevance check below would leave the new dir unwatched and its files
                // invisible until the periodic sweep (or forever, if it is disabled). A directory
                // MOVED in (`mv pkg src/pkg`) arrives as a rename, not a Create —
                // `watch_created_dirs` handles both. Its own cheap filter
                // (Create/rename + is-dir + not-ignored) makes calling it on every
                // event correct; the maintenance pass a relevant event fires
                // re-discovers the dir's current files, and the watch keeps SUBSEQUENT edits
                // firing. Gates each path on real-dir + under-a-target + not-ignored, and
                // recompiles the matcher for a moved-in nested `.gitignore` (#332).
                watch_created_dirs(&mut notify_watcher, &event, &config, &target_dirs, &mut ignore);
                if event_is_relevant(&config, &ignore, &event)
                    || event_touches_worktree(
                        &config,
                        &event,
                        &linked_worktrees,
                        worktree_registry.as_deref(),
                    )
                {
                    debounce.on_event(now);
                    // A `.gitignore` mutation changed the rules — recompile so subsequent events
                    // are classified against current rules, not the matcher
                    // this watcher booted with.
                    if kind_is_mutation(&event.kind)
                        && event.paths.iter().any(|path| is_gitignore_path(path))
                    {
                        ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
                        // PLACEMENT, not just classification, must track the new rules (#332): a
                        // removed ignore rule UN-ignores a subtree the startup walk skipped, so
                        // re-walk and add watches for it now — otherwise edits inside it never
                        // fire. notify's `watch()` is idempotent for an
                        // already-watched path, so re-walking only ADDS the
                        // newly-eligible dirs. (A newly-IGNORED subtree keeps its
                        // now-stale watches — harmless wasted watches, same as the linked-worktree
                        // path; full unwatch bookkeeping is deferred.)
                        for target in &config.targets {
                            for dir in &target.directories {
                                watch_tree_pruned(
                                    &mut notify_watcher,
                                    &config.root.join(dir),
                                    &ignore,
                                );
                            }
                        }
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
            // Pick up newly-added worktrees (a registry event fired this pass): watch any live
            // linked checkout we aren't watching yet. Removed ones keep a now-stale watch
            // (harmless; their overlay is GC-pruned) — avoids unwatch bookkeeping.
            let (current, _) = worktree_watch_targets(&config);
            for worktree in current {
                if !linked_worktrees.contains(&worktree) {
                    let _ = notify_watcher.watch(&worktree, RecursiveMode::Recursive);
                    linked_worktrees.push(worktree);
                }
            }
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
    match locks::WriteLock::acquire_timeout(&config.database, SKIP_TIMEOUT)? {
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
        Config, EmbeddingConfig, LlmConfig, ResolvedTarget, TargetKind, WatchConfig,
    };
    use crate::language::Language;

    fn mutation_event(path: PathBuf) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path)
    }

    /// A single-Rust-target `Config` rooted at `root` watching `target_dirs` — the inline builder
    /// the real-watcher placement tests share so they can call `watch_created_dirs` (which needs a
    /// `&Config` for the under-a-target gate, #332).
    fn whole_root_config(root: &Path, target_dirs: &[PathBuf]) -> Config {
        Config {
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
            llm: LlmConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
        }
    }

    #[test]
    fn event_touches_worktree_matches_checkout_targets_and_registry() {
        let config = Config {
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
            llm: LlmConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
        };
        let worktree = PathBuf::from("/wt/feat");
        let registry = PathBuf::from("/main/.git/worktrees");
        let roots = [worktree.clone()];

        // A target file in a linked worktree fires (its overlay needs refreshing).
        assert!(event_touches_worktree(
            &config,
            &mutation_event(worktree.join("src/a.rs")),
            &roots,
            Some(&registry),
        ));
        // A non-target file in the worktree does not.
        assert!(!event_touches_worktree(
            &config,
            &mutation_event(worktree.join("README.md")),
            &roots,
            Some(&registry),
        ));
        // A change in the worktree registry (a `git worktree add`/`remove`) fires.
        assert!(event_touches_worktree(
            &config,
            &mutation_event(registry.join("feat/HEAD")),
            &roots,
            Some(&registry),
        ));
        // A `.gitignore` edit in the linked checkout fires (it changes the overlay's ignored set),
        // mirroring the base classifier (#219 review).
        assert!(event_touches_worktree(
            &config,
            &mutation_event(worktree.join(".gitignore")),
            &roots,
            Some(&registry),
        ));
        // A `.gitignore` OUTSIDE any watched checkout does not.
        assert!(!event_touches_worktree(
            &config,
            &mutation_event(PathBuf::from("/elsewhere/.gitignore")),
            &roots,
            Some(&registry),
        ));
        // A read event never fires (anti-feedback, same as the base watcher).
        let read = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
            .add_path(worktree.join("src/a.rs"));
        assert!(!event_touches_worktree(&config, &read, &roots, Some(&registry)));
        // No worktrees and no registry → nothing fires.
        assert!(!event_touches_worktree(
            &config,
            &mutation_event(worktree.join("src/a.rs")),
            &[],
            None,
        ));
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
            llm: LlmConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
        };
        // A linked checkout mirrors the layout: `<checkout>/crate/src/a.rs`.
        let checkout =
            std::env::temp_dir().join(format!("ragrat-wt-subdir-co-{}-{id}", std::process::id()));
        let roots = [checkout.clone()];
        assert!(
            event_touches_worktree(
                &config,
                &mutation_event(checkout.join("crate/src/a.rs")),
                &roots,
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
            llm: LlmConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
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
            llm: LlmConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
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
            llm: LlmConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
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
            llm: LlmConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
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
        std::fs::create_dir_all(root.join("ignored_dir")).unwrap();
        std::fs::create_dir_all(root.join("kept_dir")).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join(".gitignore"), "ignored_dir/\n").unwrap();
        // Seed a file in each so the dirs exist before the watch is placed.
        std::fs::write(root.join("ignored_dir/a.rs"), "// a\n").unwrap();
        std::fs::write(root.join("kept_dir/b.rs"), "// b\n").unwrap();

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

        // A write inside the gitignored subtree must NOT be delivered (the dir was never watched).
        std::fs::write(root.join("ignored_dir/a.rs"), "// a edited\n").unwrap();
        let ignored_seen = drain_until_path_under(&rx, &root.join("ignored_dir"), 2);

        // A write to a non-ignored sibling under the same target MUST be delivered.
        std::fs::write(root.join("kept_dir/b.rs"), "// b edited\n").unwrap();
        let kept_seen = drain_until_path_under(&rx, &root.join("kept_dir"), 3);

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
        watch_created_dirs(&mut w, &create, &config, &target_dirs, &mut ignore);

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
            llm: LlmConfig { embedding: EmbeddingConfig::default() },
            watch: WatchConfig::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
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
        watch_created_dirs(&mut w, &rename, &config, &target_dirs, &mut ignore);
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
        watch_created_dirs(&mut w, &create, &config, &target_dirs, &mut ignore);

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
        // `src` target) is delivered to the loop too. `watch_created_dirs` must gate on
        // `dir_under_a_target` so it never watches such a dir — it can't be indexed and would just
        // burn inotify watches. A new subdir UNDER the target still gets watched.
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
        watch_created_dirs(&mut w, &vendor_ev, &config, &target_dirs, &mut ignore);
        std::fs::write(vendor_sub.join("v.rs"), "// v\n").unwrap();
        let vendor_seen = drain_until_path_under(&rx, &vendor_sub, 2);

        // A new dir UNDER the target: must be watched.
        let pkg = root.join("src/pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let pkg_ev = Event::new(EventKind::Create(CreateKind::Folder)).add_path(pkg.clone());
        watch_created_dirs(&mut w, &pkg_ev, &config, &target_dirs, &mut ignore);
        std::fs::write(pkg.join("p.rs"), "// p\n").unwrap();
        let pkg_seen = drain_until_path_under(&rx, &pkg, 3);

        drop(w);
        std::fs::remove_dir_all(&root).ok();
        assert!(!vendor_seen, "a non-target top-level dir must not be watched (#332)");
        assert!(pkg_seen, "a new dir under the target must still be watched");
    }

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

        // Build the moved-in dir with a NESTED `.gitignore` ignoring `ignored_sub/`, plus a kept
        // sibling — all created BEFORE feeding the rename event (so the matcher was stale to them).
        let pkg = root.join("pkg");
        std::fs::create_dir_all(pkg.join("ignored_sub")).unwrap();
        std::fs::create_dir_all(pkg.join("kept_sub")).unwrap();
        std::fs::write(pkg.join(".gitignore"), "ignored_sub/\n").unwrap();
        std::fs::write(pkg.join("ignored_sub/x.rs"), "// x\n").unwrap();
        std::fs::write(pkg.join("kept_sub/y.rs"), "// y\n").unwrap();
        let rename =
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To))).add_path(pkg.clone());
        watch_created_dirs(&mut w, &rename, &config, &target_dirs, &mut ignore);

        // The nested-ignored subdir must NOT be watched; the kept sibling MUST be.
        std::fs::write(pkg.join("ignored_sub/x.rs"), "// x edited\n").unwrap();
        let ignored_seen = drain_until_path_under(&rx, &pkg.join("ignored_sub"), 2);
        std::fs::write(pkg.join("kept_sub/y.rs"), "// y edited\n").unwrap();
        let kept_seen = drain_until_path_under(&rx, &pkg.join("kept_sub"), 3);

        drop(w);
        std::fs::remove_dir_all(&root).ok();
        assert!(!ignored_seen, "a moved-in nested-.gitignore-ignored subdir must not be watched");
        assert!(kept_seen, "the kept sibling under the moved-in dir must be watched (#332)");
    }
}
