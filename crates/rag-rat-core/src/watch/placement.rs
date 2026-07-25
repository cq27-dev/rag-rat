use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, RecursiveMode};
use rag_rat_base::config::Config;

use super::overlay::{OverlayScope, enclosing_worktree_id};
use crate::index::ignore_rules::{IgnoreMatcher, target_ancestor_dirs};
use crate::index::target_for_path;

/// Per-watcher counters for watch PLACEMENT outcomes, OWNED by the watcher that created them (not a
/// process-global). `watcher_main` builds one and shares it — via `Arc` — between the event-loop
/// thread (which records a placement on every `.watch()`) and the pass worker (which reads it to
/// persist the failure high-water mark into `repo_meta`). Scoping to the watcher INSTANCE is what
/// keeps one repo config's placement outcomes from ever being attributed to another's — even if a
/// single process ran more than one `Watcher` (the public `spawn` API does not forbid it) — and the
/// counts do not outlive the watcher's own shutdown the way a static would.
///
/// A failure means `notify::Watcher::watch` returned `Err` — on Linux, `ENOSPC` once
/// `fs.inotify.max_user_watches` is exhausted; on any OS, a transient failure. The directory (and,
/// in [`watch_tree_pruned`], its whole unwalked subtree) then silently falls back to the periodic
/// sweep. These counters make that fallback *observable*. Interior atomics so the shared `&self`
/// can be read on the pass thread while the event-loop thread records placements concurrently.
#[derive(Debug, Default)]
pub(crate) struct WatchPlacementCounters {
    attempts: AtomicU64,
    failures: AtomicU64,
    /// The failure count this watcher last emitted a warning for. Warning coalescing keys on THIS
    /// (instance-local), NOT on the persisted high-water mark — a restarted watcher whose fresh
    /// count is below a prior high-water still has real new failures to log.
    last_warned: AtomicU64,
}

impl WatchPlacementCounters {
    fn record_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    fn record_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    /// `(attempts, failures)` watch placements this watcher has seen — read by the watcher's pass
    /// to persist into `repo_meta` (`watch_placement_failures`), surfaced by `index_status`.
    pub(crate) fn counts(&self) -> (u64, u64) {
        (self.attempts.load(Ordering::Relaxed), self.failures.load(Ordering::Relaxed))
    }

    /// This watcher's total watch-placement failures, but only if they have RISEN since the last
    /// call — so the pass warns once per new batch of drops and never per directory. Returns `None`
    /// when nothing new (or none ever) failed.
    pub(crate) fn newly_warnable_failures(&self) -> Option<u64> {
        let current = self.failures.load(Ordering::Relaxed);
        if current == 0 {
            return None;
        }
        // fetch_max stores the max and returns the PRIOR value atomically, so the pass thread can't
        // double-warn for the same increment even against a concurrent placement.
        let prior = self.last_warned.fetch_max(current, Ordering::Relaxed);
        (current > prior).then_some(current)
    }
}

/// Place one non-recursive watch, counting the outcome into `counters`; returns `true` on success.
/// EVERY `.watch()` in this module goes through here so a swallowed failure is still counted.
/// Behavior is unchanged — a failed watch still falls back to the sweep exactly as before; only its
/// visibility changes.
fn place_watch(
    watcher: &mut impl notify::Watcher,
    counters: &WatchPlacementCounters,
    path: &Path,
    mode: RecursiveMode,
) -> bool {
    counters.record_attempt();
    match watcher.watch(path, mode) {
        Ok(()) => true,
        Err(_) => {
            // Only a watch that failed on a directory that EXISTS is a silently-dropped watch (the
            // ENOSPC / inotify-exhaustion case this signal is for). A watch that failed because the
            // path is absent is an EXPECTED miss — a not-yet-created configured target, a
            // branch-specific subdir, or a worktree registry that appears later — all of which the
            // ancestor / bootstrap / created-dir watches already cover. Counting those would peg
            // the never-lowered high-water mark forever on a config with an optional
            // target dir and emit a spurious ENOSPC warning. `try_exists` errors (e.g.
            // a permission wall) count, so a genuine drop is never under-reported.
            if path.try_exists().unwrap_or(true) {
                counters.record_failure();
            }
            false
        },
    }
}

/// Whether an event KIND should ever fire a pass. Only content mutations do — `Create`, `Remove`,
/// any `Modify` (data/metadata/rename), and a write-close. Reads must NOT: notify's inotify mask
/// includes `IN_OPEN`/`IN_CLOSE_NOWRITE`, so opening/reading a watched file emits `Access(Open)` /
/// `Access(Close(Read))` events. Treating those as relevant created a feedback loop — the index
/// pass's own file reads (and the MCP serving queries, and the grep-augment hook reading source)
/// re-fired the watcher endlessly, re-indexing every couple seconds with `content_revision`
/// unchanged. Those read events stack in the notify→watcher channel and keep the debounce armed.
pub(crate) fn kind_is_mutation(kind: &EventKind) -> bool {
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
pub(crate) fn gitignore_watch_dirs(root: &Path) -> Vec<PathBuf> {
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
pub(crate) fn is_gitignore_path(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == ".gitignore")
}

/// A repo-config (`rag-rat.toml`) path. A LINKED checkout's config edit re-targets its branch
/// overlay without moving either HEAD, so the linked classifier fires on it like a `.gitignore`
/// edit (#577 review).
pub(crate) fn is_repo_config_path(path: &Path) -> bool {
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
pub(crate) fn event_is_relevant(config: &Config, ignore: &IgnoreMatcher, event: &Event) -> bool {
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
pub(crate) struct LinkedWorktreeWatch {
    pub(crate) checkout_root: PathBuf,
    pub(crate) config: Config,
    pub(crate) target_dirs: Vec<PathBuf>,
    pub(crate) ignore: IgnoreMatcher,
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

    fn place_watches(&self, watcher: &mut impl notify::Watcher, counters: &WatchPlacementCounters) {
        watch_configured_trees(watcher, counters, &self.config, &self.target_dirs, &self.ignore);
        watch_gitignore_rule_dirs(watcher, counters, &self.config.root, &self.target_dirs);
        for root in missing_config_root_bootstrap_dirs(&self.config.root, &self.checkout_root) {
            // The linked checkout may not have this subdir on the current branch yet. Keep narrow
            // bootstraps on the existing ancestor chain so the next recreated config-root component
            // is observed without returning to a recursive whole-checkout watch.
            place_watch(watcher, counters, &root, RecursiveMode::NonRecursive);
        }
    }

    pub(crate) fn recompile_ignore_and_place_watches(
        &mut self,
        watcher: &mut impl notify::Watcher,
        counters: &WatchPlacementCounters,
    ) {
        self.ignore = IgnoreMatcher::compile(&self.config.root, &self.target_dirs);
        self.place_watches(watcher, counters);
    }

    pub(crate) fn watch_created_dirs(
        &mut self,
        watcher: &mut impl notify::Watcher,
        counters: &WatchPlacementCounters,
        event: &Event,
    ) -> bool {
        watch_created_dirs(
            watcher,
            counters,
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

    fn touches_tree_event(&self, path: &Path) -> bool {
        self.touches_event(path)
            || path.strip_prefix(&self.config.root).ok().is_some_and(|relative| {
                self.target_dirs
                    .iter()
                    .any(|target| relative.starts_with(target) || target.starts_with(relative))
            })
    }
}

#[derive(Debug, Default)]
pub(crate) struct LinkedWorktreeWatches {
    pub(crate) states: Vec<LinkedWorktreeWatch>,
}

impl LinkedWorktreeWatches {
    pub(crate) fn sync(
        &mut self,
        watcher: &mut impl notify::Watcher,
        counters: &WatchPlacementCounters,
        base_config: &Config,
        checkout_roots: Vec<PathBuf>,
    ) {
        let mut states = Vec::with_capacity(checkout_roots.len());
        for root in checkout_roots {
            let state = LinkedWorktreeWatch::new(base_config, root);
            state.place_watches(watcher, counters);
            states.push(state);
        }
        self.states = states;
    }

    /// Place watches for created/moved-in dirs across EVERY state (no short-circuit — placement
    /// is a side effect each state needs), returning the checkout roots that placed one, so the
    /// armed pass can scope its overlay refresh to them (#577).
    pub(crate) fn watch_created_dirs(
        &mut self,
        watcher: &mut impl notify::Watcher,
        counters: &WatchPlacementCounters,
        event: &Event,
    ) -> BTreeSet<PathBuf> {
        let mut placed = BTreeSet::new();
        for state in &mut self.states {
            if state.watch_created_dirs(watcher, counters, event) {
                placed.insert(state.checkout_root.clone());
            }
        }
        placed
    }

    pub(crate) fn recompile_ignore_and_place_watches(
        &mut self,
        watcher: &mut impl notify::Watcher,
        counters: &WatchPlacementCounters,
    ) {
        for state in &mut self.states {
            state.recompile_ignore_and_place_watches(watcher, counters);
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
        // The path primitive can tombstone an explicit file, but it cannot enumerate stale
        // descendants after a directory disappears. Widen removal-side directory/ambiguous rename
        // events to a whole-checkout refresh; paired renames stay path-scoped only when the
        // surviving destination proves this was a regular file.
        let whole_checkout = match event.kind {
            EventKind::Remove(RemoveKind::File) => false,
            EventKind::Remove(_) => true,
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) =>
                event.paths.iter().any(|path| path.is_dir()),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) =>
                event.paths.last().is_none_or(|path| !path.is_file()),
            EventKind::Modify(ModifyKind::Name(
                RenameMode::From | RenameMode::Any | RenameMode::Other,
            )) => true,
            _ => false,
        };
        let mut paths = BTreeMap::<PathBuf, BTreeSet<PathBuf>>::new();
        for path in &event.paths {
            if registry.is_some_and(|reg| path.starts_with(reg)) {
                // A worktree add/remove: the live set itself changed — unattributable.
                return WorktreeEventHint::AllWorktrees;
            }
            for state in self.states.iter().filter(|state| {
                if whole_checkout {
                    state.touches_tree_event(path)
                } else {
                    state.touches_event(path)
                }
            }) {
                if whole_checkout {
                    paths.entry(state.checkout_root.clone()).or_default().clear();
                } else {
                    match paths.entry(state.checkout_root.clone()) {
                        Entry::Vacant(entry) => {
                            entry.insert(BTreeSet::from([path.clone()]));
                        },
                        Entry::Occupied(mut entry) if !entry.get().is_empty() => {
                            entry.get_mut().insert(path.clone());
                        },
                        Entry::Occupied(_) => {},
                    }
                }
            }
        }
        if paths.is_empty() { WorktreeEventHint::None } else { WorktreeEventHint::Paths(paths) }
    }
}

/// Overlay implication of one event for the LINKED-worktree layer (#577): which checkouts the
/// armed pass must refresh — or `AllWorktrees` when the event can't be attributed (a backend
/// rescan, a worktree-registry change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorktreeEventHint {
    /// No linked checkout is touched.
    None,
    /// The event touches these paths, grouped by linked checkout root.
    Paths(BTreeMap<PathBuf, BTreeSet<PathBuf>>),
    /// Unattributable — refresh every overlay.
    AllWorktrees,
}

impl WorktreeEventHint {
    /// Whether the linked-worktree layer wants a pass at all.
    pub(crate) fn fires(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Whether `event` should fire a pass for the LINKED-worktree layer (#219): a content mutation to a
/// configured target inside a linked worktree checkout (its overlay needs refreshing), or any
/// change in the worktree registry (`<common_dir>/worktrees`, i.e. a worktree add/remove). Separate
/// from [`event_is_relevant`] so the base-tree classification — and its tests — stay untouched.
/// Each linked checkout has its own target set and ignore matcher, so the watcher does not fire on
/// paths the overlay walker would drop (including `target/` or branch-local gitignored dirs).
pub(crate) fn event_touches_worktree(
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

pub(crate) fn missing_config_root_bootstrap_dirs(
    config_root: &Path,
    checkout_root: &Path,
) -> Vec<PathBuf> {
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

pub(crate) fn watch_tree_pruned(
    watcher: &mut impl notify::Watcher,
    counters: &WatchPlacementCounters,
    dir: &Path,
    ignore: &IgnoreMatcher,
) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if !place_watch(watcher, counters, &d, RecursiveMode::NonRecursive) {
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
pub(crate) enum CreatedDirPlacement {
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
pub(crate) fn created_dir_placement(
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
pub(crate) fn watch_created_dirs(
    watcher: &mut impl notify::Watcher,
    counters: &WatchPlacementCounters,
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
                place_watch(watcher, counters, path, RecursiveMode::NonRecursive);
                watch_gitignore_rule_dirs(watcher, counters, &config.root, target_dirs);
                *ignore = IgnoreMatcher::compile(&config.root, target_dirs);
                watch_configured_trees(watcher, counters, config, target_dirs, ignore);
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
                watch_tree_pruned(watcher, counters, path, ignore);
                placed_target_watch = true;
            },
        }
    }
    placed_target_watch
}

pub(crate) fn place_initial_watch_state(
    watcher: &mut impl notify::Watcher,
    counters: &WatchPlacementCounters,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &IgnoreMatcher,
    fleet_bin: Option<&Path>,
) -> (LinkedWorktreeWatches, Option<PathBuf>) {
    watch_configured_trees(watcher, counters, config, target_dirs, ignore);
    watch_gitignore_rule_dirs(watcher, counters, &config.root, target_dirs);

    if let Some(dir) = fleet_bin.and_then(Path::parent) {
        place_watch(watcher, counters, dir, RecursiveMode::NonRecursive);
    }

    let (linked_worktree_roots, worktree_registry) = worktree_watch_targets(config);
    let linked_worktrees = watch_linked_worktrees(watcher, counters, config, linked_worktree_roots);
    if let Some(registry) = &worktree_registry {
        place_watch(watcher, counters, registry, RecursiveMode::NonRecursive);
    }
    (linked_worktrees, worktree_registry)
}

/// Whether `event` should arm a maintenance pass — and if so, which linked-worktree overlays it
/// implicates (#577): `Some(scope)` to fire (an empty `Linked` set is a base-only event — the
/// pass's discover covers the base scope regardless), `None` to ignore. Every sub-check still
/// RUNS unconditionally (no short-circuit): watch placement is a side effect both the base and
/// every linked state need regardless of what else already fired.
#[expect(
    clippy::too_many_arguments,
    reason = "the per-watcher placement counters thread alongside the watcher through every \
              placement entry point (#658); this one was already at the limit"
)]
pub(crate) fn event_requests_maintenance(
    watcher: &mut impl notify::Watcher,
    counters: &WatchPlacementCounters,
    event: &Event,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &mut IgnoreMatcher,
    linked_worktrees: &mut LinkedWorktreeWatches,
    worktree_registry: Option<&Path>,
) -> Option<OverlayScope> {
    let created_dir_watch_placed =
        watch_created_dirs(watcher, counters, event, config, target_dirs, ignore, None);
    let linked_created_dir_roots = linked_worktrees.watch_created_dirs(watcher, counters, event);
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
        WorktreeEventHint::Paths(paths) => OverlayScope::Paths(paths),
        WorktreeEventHint::None => OverlayScope::Linked(BTreeSet::new()),
    };
    Some(scope.merge(OverlayScope::Linked(linked_created_dir_roots)))
}

pub(crate) fn recompile_ignore_and_place_watches(
    watcher: &mut impl notify::Watcher,
    counters: &WatchPlacementCounters,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &mut IgnoreMatcher,
    linked_worktrees: &mut LinkedWorktreeWatches,
) {
    *ignore = IgnoreMatcher::compile(&config.root, target_dirs);
    watch_configured_trees(watcher, counters, config, target_dirs, ignore);
    linked_worktrees.recompile_ignore_and_place_watches(watcher, counters);
}

pub(crate) fn sync_linked_worktrees_after_pass(
    watcher: &mut impl notify::Watcher,
    counters: &WatchPlacementCounters,
    config: &Config,
    linked_worktrees: &mut LinkedWorktreeWatches,
) {
    let (current, _) = worktree_watch_targets(config);
    linked_worktrees.sync(watcher, counters, config, current);
}

/// Live linked-worktree checkout roots (excluding the base `config.root`) plus the worktree
/// registry dir (`<common_dir>/worktrees`), for the watcher to subscribe to — branch checkouts for
/// edits, the registry for add/remove.
pub(crate) fn worktree_watch_targets(config: &Config) -> (Vec<PathBuf>, Option<PathBuf>) {
    let (_, worktrees) = crate::index::live_worktree_contexts(&config.root);
    // The base id is the ENCLOSING worktree root, not `config.root` itself. When `config.root` is a
    // repo SUBDIR (`<repo>/crate`), `live_worktree_contexts` reports the main checkout as `<repo>`
    // (its workdir), but `worktree_id_of(config.root)` is `<repo>/crate` — they wouldn't match, so
    // the main checkout would be treated as a linked worktree and the watcher would recursively
    // subscribe to the whole repo root outside the configured target (#219 review).
    let base = enclosing_worktree_id(&config.root);
    let roots = worktrees.into_iter().filter(|w| *w != base).map(PathBuf::from).collect();
    let registry = rag_rat_base::repo_discover::discover_repo(&config.root)
        .ok()
        .map(|repo| repo.common_dir().join("worktrees"));
    (roots, registry)
}

pub(crate) fn watch_configured_trees(
    watcher: &mut impl notify::Watcher,
    counters: &WatchPlacementCounters,
    config: &Config,
    target_dirs: &[PathBuf],
    ignore: &IgnoreMatcher,
) {
    for dir in target_dirs {
        watch_tree_pruned(watcher, counters, &config.root.join(dir), ignore);
    }
}

pub(crate) fn gitignore_rule_watch_dirs(root: &Path, target_dirs: &[PathBuf]) -> Vec<PathBuf> {
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

pub(crate) fn watch_gitignore_rule_dirs(
    watcher: &mut impl notify::Watcher,
    counters: &WatchPlacementCounters,
    root: &Path,
    target_dirs: &[PathBuf],
) {
    for dir in gitignore_rule_watch_dirs(root, target_dirs) {
        place_watch(watcher, counters, &dir, RecursiveMode::NonRecursive);
    }
}

pub(crate) fn watch_linked_worktrees(
    watcher: &mut impl notify::Watcher,
    counters: &WatchPlacementCounters,
    base_config: &Config,
    checkout_roots: Vec<PathBuf>,
) -> LinkedWorktreeWatches {
    let mut worktrees = LinkedWorktreeWatches::default();
    worktrees.sync(watcher, counters, base_config, checkout_roots);
    worktrees
}

/// Whether `event` touches the installed binary path — the fleet hot-upgrade trigger. Matches by
/// full path (`cargo install` renames its temp file to exactly this path) so unrelated churn in
/// the same directory is ignored.
pub(crate) fn event_targets_binary(fleet_bin: Option<&Path>, event: &Event) -> bool {
    let Some(bin) = fleet_bin else {
        return false;
    };
    event.paths.iter().any(|path| path == bin)
}
