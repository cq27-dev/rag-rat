//! Shared ignore matcher for the walker (index discovery) and the watcher (event classification).
//!
//! Issue #62: discovery and the watcher both used to skip directories by a *hardcoded* name list
//! (`.git`, `.rag-rat`, `target`, `node_modules`, `dist`, `build`, `coverage`). That missed
//! repo-specific `.gitignore` entries — generated dirs, build outputs, vendored code in
//! non-standard locations — so they could get indexed and (recursively) watched.
//!
//! [`IgnoreMatcher`] compiles the repo's real `.gitignore` rules and both the walker and the
//! watcher consult it, so a path one ignores the other also ignores — no drift. The hardcoded names
//! are kept as a **floor** ([`FLOOR_DIRS`]): they apply even in a non-git tree with no
//! `.gitignore`, and they cover rag-rat's own index dir (`.rag-rat/`), which must never be indexed
//! regardless of gitignore contents.
//!
//! **Git semantics, correct by construction (issue #62 / PR #66, three rounds of P2 findings).**
//! We build the matcher on the [`ignore`] crate's native [`Gitignore`] machinery — each
//! `.gitignore` is compiled by [`GitignoreBuilder`] *anchored at its own directory*, so a
//! non-anchored pattern (`skip.rs`) scopes to that subtree exactly as Git does. We do **not**
//! flatten everything into one builder (that would `**/`-prefix nested patterns and leak them
//! repo-wide). The matcher is a small ancestor-anchored *stack* of these per-directory
//! `Gitignore`s; precedence and parent-exclusion then fall out of walking a path's ancestors
//! top-down (see [`IgnoreMatcher::is_ignored`]).
//!
//! The three properties that earned their own findings:
//!
//! 1. **Repo-relative checks, never absolute-ancestor.** The walker and watcher feed absolute
//!    paths. A repo can live *under* a directory named like a floor entry (`/tmp/build/repo`), so
//!    the floor check runs on the path relative to `config.root`, not on its absolute ancestors.
//! 2. **Parent exclusion.** A file under an ignored directory is *not* re-included by a deeper
//!    negation unless the *parent directory itself* is re-included. Git stops descending once a
//!    directory is excluded; a nested `gen/.gitignore !keep.rs` under a root-ignored `gen/` does
//!    not resurrect `gen/keep.rs`. [`is_ignored`] walks ancestors root→leaf and short-circuits at
//!    the first ignored directory, and discovery prunes excluded subtrees so their nested
//!    `.gitignore` is never even read.
//! 3. **Ancestor `.gitignore` files above a subdirectory `config.root`.** When `config.root` is a
//!    subdirectory of a larger Git worktree, the worktree-root `.gitignore` (and every `.gitignore`
//!    on the chain down to `config.root`) still governs paths under `config.root` — Git applies
//!    them. The matcher resolves the worktree root (via
//!    [`crate::index::git_history::worktree_root`], the one place we shell `git rev-parse
//!    --show-toplevel`) and seeds the stack with that ancestor chain before the in-tree gitignores.
//!    In a non-git tree the base is just `config.root`.
//!
//! **Discovery is scoped to the target trees** (finding from round 3): nested `.gitignore` files
//! are collected only along the configured `target.directories` and their ancestor chain, never by
//! recursing the whole `config.root` into large unindexed sibling directories.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::index::git_history;

/// Directory names always skipped, gitignore or not — the floor under the compiled `.gitignore`
/// rules. `.rag-rat` is rag-rat's own index dir (indexing it would be a feedback loop); `.git` is
/// never source; the rest are conventional build/dependency/output dirs that a non-git tree (no
/// `.gitignore` to compile) still must not index. In a git tree these usually also appear in
/// `.gitignore`, but the floor guarantees them even when they don't.
const FLOOR_DIRS: &[&str] = &[
    ".git",
    rag_rat_base::data_dir::WORKSPACE_DIR,
    ".claude",
    ".codex",
    ".omx",
    ".omc",
    "node_modules",
    "target",
    "dist",
    "build",
    // SwiftPM's build tree contains generated output and dependency checkouts whose nested
    // `Sources/` directories otherwise look indistinguishable from first-party Swift packages.
    ".build",
    // CocoaPods' vendored dependency tree — the same hazard as `.build/checkouts`, and worse for
    // an app with no `Sources/`-style layout (root `AppDelegate.swift`), where the Swift
    // target can fall back to `.` and swallow every pod's source as first-party. Flooring it
    // here covers BOTH the `init` scan and the indexer, which is why the floor is the right
    // seam: excluding it in only one of them leaves the other ingesting vendored code.
    "Pods",
    "coverage",
    // Python virtualenv / dependency / cache trees: never project source. `site-packages` is the
    // load-bearing one — installed deps live there regardless of the venv dir's name (`.venv`,
    // `venv`, `env`, …), so flooring it stops a `python = ["."]` config from ingesting the whole
    // dependency set even when the venv dir itself isn't named conventionally. Only names that can
    // NEVER be a first-party import package are floored: `.venv`/`.tox`/`.nox` start with a dot (a
    // Python package dir can't), and `site-packages`/`__pycache__` are reserved. `venv` is the one
    // bare name kept (overwhelmingly a virtualenv). `virtualenv`/`env`/`.env` are deliberately NOT
    // floored — `virtualenv` is itself a real package (`src/virtualenv/…` is first-party source,
    // #181 review), and `env`/`.env` are too generic; their installed deps are still caught by
    // `site-packages`, and the init scanner refuses to auto-bind `.` when any of them sit at the
    // root.
    ".venv",
    "venv",
    ".tox",
    ".nox",
    "site-packages",
    "__pycache__",
];

/// Floors that are a PATH, not a bare directory name — matched as consecutive components anywhere
/// under `config.root`.
///
/// `.cache` alone is far too broad to floor: the floor is unconditional and cannot be whitelisted
/// back, so flooring the name would silently drop a tracked `.cache/` that a repo genuinely uses
/// for sources. What has to be excluded is `.cache/clangd/` — the index the live clangd oracle
/// makes the checkout write to itself, which no clangd flag or environment variable can relocate.
///
/// This is the same category as `.rag-rat` above: a tool's own index, living inside the checkout,
/// which must never be walked or indexed as if it were source. Its `.idx` artifacts carry no
/// target extension and so would not arm the watcher on their own, but the tree is large and
/// entirely machine-written, and anything source-shaped appearing there would otherwise be
/// classified as first-party code.
const FLOOR_PATHS: &[&[&str]] = &[&[".cache", "clangd"]];

/// Whether a single path component matches a floor directory name (see [`FLOOR_DIRS`]).
fn is_floor_dir(name: &str) -> bool {
    FLOOR_DIRS.contains(&name)
}

/// Whether `dir` is a Python virtualenv — detected by CONTENT, not name: every venv created by
/// `python -m venv` / `virtualenv` (Python 3.3+) writes a `pyvenv.cfg` at its root. This is the
/// name-independent test that distinguishes an ambiguously-named venv (`env/`, `virtualenv/`) from
/// a first-party package of the same name (the `virtualenv` PyPI package's `src/virtualenv/` has no
/// `pyvenv.cfg`). Used to keep a venv out of init's binding candidates without flooring real
/// package names (#181). `FLOOR_DIRS` still covers the conventional names (`.venv`/`venv`) as a
/// fast path and for legacy venvs lacking the marker.
pub fn is_virtualenv_dir(dir: &Path) -> bool {
    dir.join("pyvenv.cfg").is_file()
}

/// One `.gitignore`, compiled by [`GitignoreBuilder`] with its own directory as the matching root
/// so its patterns are scoped to that subtree (gitignore semantics: a non-anchored pattern like
/// `skip.rs` matches only at or below the file's directory, not the whole repo). `rel_dir` is that
/// directory **relative to the matcher base** (the worktree root, or `config.root` outside git);
/// matching strips it off the candidate's base-relative path before applying the compiled matcher.
#[derive(Debug)]
struct ScopedGitignore {
    /// The `.gitignore`'s directory relative to the matcher base (empty for the base
    /// `.gitignore`); the matcher applies only to base-relative paths under it.
    rel_dir: PathBuf,
    gitignore: Gitignore,
}

/// Compiled ignore rules for one indexed root: the floor names plus an ancestor-anchored stack of
/// per-directory [`Gitignore`]s. Built once per walk/watch and shared so the walker and watcher
/// classify paths identically (issue #62).
///
/// **Two anchors.** `base` is the matcher's gitignore frame — the Git worktree root when
/// `config.root` is inside one (so ancestor `.gitignore` rules apply to a subdirectory root,
/// finding 3), else `config.root`. `root` is `config.root` itself; the floor check and the
/// "governed at all?" guard run relative to `root`, so a `config.root` living under a floor-named
/// worktree path (`/tmp/build/repo`) still indexes its own files (finding 1).
///
/// **Why a per-directory stack and not one flat builder:** `GitignoreBuilder::add` flattens every
/// file's globs into a single matcher rooted at the *builder* root and `**/`-prefixes non-anchored
/// patterns — so a nested `.gitignore` rule `skip.rs` would wrongly match `skip.rs` anywhere.
/// Compiling each `.gitignore` against *its own* directory and only applying it to descendants is
/// what makes nesting correct.
#[derive(Debug)]
pub struct IgnoreMatcher {
    /// `config.root` — the indexed root. Floor checks and the governed-path guard are relative to
    /// this (finding 1), independently of the wider `base`.
    root: PathBuf,
    /// The gitignore frame: the Git worktree root if `root` is inside one, else `root`. Every
    /// `ScopedGitignore.rel_dir` and all gitignore matching is relative to `base` (finding 3).
    base: PathBuf,
    /// Stack of compiled gitignores, **outermost first** (worktree-root/ancestor before nested).
    /// Matching applies the ones whose `rel_dir` is an ancestor of the candidate,
    /// outermost→innermost, deepest decision winning, so a nested whitelist can override an outer
    /// ignore — standard git precedence.
    stack: Vec<ScopedGitignore>,
}

impl IgnoreMatcher {
    /// Compile the matcher for `root`, scoping nested-`.gitignore` discovery to `target_dirs`
    /// (relative to `root`). Resolves the enclosing Git worktree root and seeds the stack with the
    /// ancestor `.gitignore` chain from there down to `root` (finding 3), then discovers nested
    /// gitignores only along the target trees (round-3 scoping fix) — never recursing the whole
    /// `root` into unindexed siblings. Never fails — a malformed gitignore is dropped and matching
    /// proceeds with what compiled.
    pub fn compile(root: &Path, target_dirs: &[PathBuf]) -> Self {
        // The gitignore frame: the worktree root if `root` is inside a Git worktree, else `root`.
        // Only accept it when `root` is actually a descendant (or equal) — a `--show-toplevel`
        // result we can't relate to `root` is unusable as a prefix-stripping base.
        //
        // `gix`'s `workdir()` is not canonicalized, so on Windows it can carry a different prefix
        // representation than `root` (e.g. plain `C:\…` vs a verbatim `\\?\C:\…` from a
        // canonicalized `config.root`) — a raw `root.starts_with(wt)` then wrongly fails and the
        // ancestor `.gitignore` chain is dropped. Decide the ancestor relationship on canonicalized
        // forms, but derive `base` by trimming components off `root` itself so it stays a textual
        // prefix of `root` (and of every caller path `is_ignored` strips against it).
        let base = git_history::worktree_root(root)
            .and_then(|wt| base_under_worktree(root, &wt))
            .unwrap_or_else(|| root.to_path_buf());

        let mut matcher = Self { root: root.to_path_buf(), base, stack: Vec::new() };

        // (1) Ancestor chain: every `.gitignore` from the worktree base down to (and including)
        // `root`, so worktree-root rules govern a subdirectory `config.root` (finding 3).
        matcher.collect_ancestor_gitignores();

        // (2) Nested gitignores, but ONLY along the configured target directories (round-3 scoping
        // fix) — not a recursive sweep of the whole `root`. Each target dir is walked top-down with
        // parent-exclusion pruning (finding 2). Dedup so a `.gitignore` under overlapping target
        // dirs is compiled once.
        if target_dirs.is_empty() {
            matcher.collect_nested_gitignores(root);
        } else {
            for dir in target_ancestor_dirs(root, target_dirs) {
                matcher.push_gitignore_in(&dir);
            }
            for target_dir in target_dirs {
                matcher.collect_nested_gitignores(&root.join(target_dir));
            }
        }

        // Outermost first: shortest rel_dir sorts before its descendants, making the precedence
        // walk in `decision_for` order-independent.
        matcher.stack.sort_by_key(|scoped| scoped.rel_dir.as_os_str().len());
        matcher
    }

    /// Whether `path` is ignored. `is_dir` must say whether the path is a directory — gitignore
    /// distinguishes `foo/` (dir-only) from `foo`.
    ///
    /// A path outside `config.root` is not governed (returns `false`). A floor-dir name among the
    /// components relative to `config.root` ignores it unconditionally (the floor can't be
    /// whitelisted away — finding 1). Otherwise each ancestor directory, walked **relative to the
    /// matcher base** (which may sit above `config.root`), is evaluated root→leaf against the
    /// scoped `.gitignore` stack: the first ancestor that resolves to *ignored* makes the whole
    /// path ignored (git parent exclusion — a deeper `!negation` cannot resurrect a file under
    /// an excluded directory). A whitelisted ancestor clears the ignored state for that level.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        // Governed only inside config.root; the floor is checked on the config.root-relative path
        // so a root under a floor-named ancestor still indexes (finding 1).
        let Ok(rel_to_root) = path.strip_prefix(&self.root) else {
            return false; // outside the indexed root — not governed by our rules.
        };
        if rel_contains_floor_dir(rel_to_root) {
            return true;
        }
        // Gitignore matching is base-relative so ancestor `.gitignore`s above config.root apply
        // (finding 3). `root` is always under `base`, so this strip succeeds.
        let Ok(rel) = path.strip_prefix(&self.base) else {
            return false;
        };
        // Walk ancestor prefixes root→leaf. `ignored` carries the inherited decision from shallower
        // levels; once a directory level lands on `Ignore`, every deeper level inherits it unless a
        // level is explicitly whitelisted (which clears it for that and deeper levels).
        let mut ignored = false;
        let mut prefix = PathBuf::new();
        let mut components = rel.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                continue; // skip `.`/`..`/prefix/root — base-relative paths shouldn't have them.
            };
            prefix.push(name);
            // The leaf uses the caller's `is_dir`; every intermediate prefix is a directory.
            let is_last = components.peek().is_none();
            let level_is_dir = if is_last { is_dir } else { true };
            match self.decision_for(&prefix, level_is_dir) {
                Match::Ignore(_) => ignored = true,
                Match::Whitelist(_) => ignored = false,
                Match::None => {},
            }
            // Parent exclusion: an ignored *directory* prunes its whole subtree — stop descending.
            // (If this was the leaf, the loop ends anyway.) A deeper negation can't reach inside.
            if ignored && level_is_dir && !is_last {
                return true;
            }
        }
        ignored
    }

    /// The combined gitignore decision for one base-relative path at one level, applying every
    /// scoped gitignore whose `rel_dir` is an ancestor, outermost→innermost (deepest wins). Uses
    /// `Gitignore::matched` (leaf-only, not `matched_path_or_any_parents`): the caller
    /// ([`is_ignored`]) already walks ancestors top-down, so per-level leaf matching is correct and
    /// avoids double parent-walking.
    fn decision_for(&self, rel: &Path, is_dir: bool) -> Match<()> {
        let mut decision = Match::None;
        for scoped in &self.stack {
            let Ok(scoped_rel) = rel.strip_prefix(&scoped.rel_dir) else {
                continue; // gitignore in a sibling/unrelated subtree — doesn't govern this path.
            };
            if scoped_rel.as_os_str().is_empty() {
                continue; // the gitignore's own directory.
            }
            match scoped.gitignore.matched(scoped_rel, is_dir) {
                Match::Ignore(_) => decision = Match::Ignore(()),
                Match::Whitelist(_) => decision = Match::Whitelist(()),
                Match::None => {},
            }
        }
        decision
    }

    /// Seed the stack with the ancestor `.gitignore` chain from the matcher `base` (worktree root)
    /// down to and including `root`. These are the rules Git applies to paths under a subdirectory
    /// `config.root` (finding 3). No-op when `base == root` (the chain is just `root` itself, which
    /// the nested scan also covers — dedup in [`push_gitignore_in`] keeps it single).
    fn collect_ancestor_gitignores(&mut self) {
        // Build the list of directories from base down to root: base, base/a, base/a/b, …, root.
        let Ok(rel) = self.root.strip_prefix(&self.base) else {
            return;
        };
        // Collect the component names first so `self` isn't borrowed across the mutating pushes.
        let names: Vec<PathBuf> = rel
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(PathBuf::from(name)),
                _ => None,
            })
            .collect();
        let mut dir = self.base.clone();
        self.push_gitignore_in(&dir);
        for name in names {
            dir.push(name);
            self.push_gitignore_in(&dir);
        }
    }

    /// Recursively collect + compile nested `.gitignore` files at or below `dir`, pruning floor
    /// dirs and — crucially — any directory an already-collected outer rule ignores (finding 2:
    /// don't descend into an excluded directory to read a nested gitignore that could wrongly
    /// un-ignore its contents). `dir` is absolute; the stack is grown in place so outer rules
    /// govern the descent decision for their children.
    fn collect_nested_gitignores(&mut self, dir: &Path) {
        self.push_gitignore_in(dir);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            if name.to_str().is_some_and(is_floor_dir) {
                continue;
            }
            let child = entry.path();
            // Don't descend into a directory an outer rule already ignores — a nested gitignore
            // there must not re-include files under an excluded parent (finding 2). We classify the
            // child as a directory against the rules collected so far.
            if self.is_ignored(&child, true) {
                continue;
            }
            self.collect_nested_gitignores(&child);
        }
    }

    /// Compile the `.gitignore` directly inside `dir` (if any) anchored at `dir`, and push it to
    /// the stack with `rel_dir` relative to `base`. Idempotent: a `.gitignore` already in the
    /// stack (the ancestor chain and the nested scan can overlap at `root`) is not added twice.
    fn push_gitignore_in(&mut self, dir: &Path) {
        let Ok(rel_dir) = dir.strip_prefix(&self.base) else {
            return;
        };
        let rel_dir = rel_dir.to_path_buf();
        if self.stack.iter().any(|scoped| scoped.rel_dir == rel_dir) {
            return; // already compiled (ancestor chain ↔ nested scan overlap at `root`).
        }
        if !dir.join(".gitignore").is_file() {
            return;
        }
        let mut builder = GitignoreBuilder::new(dir);
        // `add` returns an Option<Error> for partial-parse problems; ignore it (best-effort,
        // matching ripgrep's own tolerance) rather than failing the whole walk.
        let _ = builder.add(dir.join(".gitignore"));
        if let Ok(gitignore) = builder.build() {
            self.stack.push(ScopedGitignore { rel_dir, gitignore });
        }
    }
}

/// Directories between `root` and each configured target root, excluding `root` itself.
///
/// These are not scan roots: they can carry `.gitignore` files that govern nested targets such as
/// `src/generated`, but recursively walking them would scan unindexed siblings under `src/`. Keep
/// this path-prefix expansion shared so watcher subscriptions and ignore compilation agree on the
/// same ancestor surface.
pub(crate) fn target_ancestor_dirs(root: &Path, target_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut dirs = Vec::new();
    for target_dir in target_dirs {
        let mut dir = root.to_path_buf();
        for component in target_dir.components() {
            match component {
                Component::Normal(name) => {
                    dir.push(name);
                    if seen.insert(dir.clone()) {
                        dirs.push(dir.clone());
                    }
                },
                Component::CurDir => {},
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => break,
            }
        }
    }
    dirs
}

/// Whether the (`config.root`-relative) `rel` path is floored: any component is a floor directory
/// name, or any run of consecutive components matches a [`FLOOR_PATHS`] entry.
///
/// One allocation-free, short-circuiting pass over the components: this is the per-path gate of the
/// indexing walk ([`IgnoreMatcher::is_ignored`]), so it runs once for every path discovered and
/// must not allocate per call. `matched_len[i]` is how many leading components of `FLOOR_PATHS[i]`
/// the run ending at the component just read has matched.
fn rel_contains_floor_dir(rel: &Path) -> bool {
    // `floor[*matched]` below indexes with a value that is only ever 0 or 1, or one past a match
    // that did not reach `floor.len()` — so it is in bounds for every non-empty floor path. Pin
    // non-emptiness at compile time rather than leaving a panic reachable from the walk.
    const _: () = {
        let mut i = 0;
        while i < FLOOR_PATHS.len() {
            assert!(!FLOOR_PATHS[i].is_empty(), "a floor path needs at least one component");
            i += 1;
        }
    };

    let mut matched_len = [0usize; FLOOR_PATHS.len()];
    for component in rel.components() {
        // A component that is not valid UTF-8 can match no floor entry, and as `None` it also fails
        // both arms below — so it BREAKS a floor path's run of consecutive components. Dropping it
        // instead would splice its neighbours together and read `.cache/<non-utf8>/clangd` as
        // `.cache/clangd`, unconditionally excluding a tracked tree that merely happens to sit
        // between them.
        let name = component.as_os_str().to_str();
        if name.is_some_and(is_floor_dir) {
            return true;
        }
        for (floor, matched) in FLOOR_PATHS.iter().zip(matched_len.iter_mut()) {
            *matched = if name == Some(floor[*matched]) {
                *matched + 1
            } else if name == Some(floor[0]) {
                // A component that fails to extend the run but equals the floor's FIRST element
                // starts a fresh run, so `.cache/.cache/clangd` is floored. (One counter remembers
                // one candidate run, which is exact only while no floor path's prefix repeats
                // inside itself — none does.)
                1
            } else {
                0
            };
            if *matched == floor.len() {
                return true;
            }
        }
    }
    false
}

/// The gitignore base for `root` given an enclosing worktree root `wt`, or `None` when `wt` is not
/// an ancestor of (or equal to) `root`. Returns the matching ancestor in `root`'s OWN
/// representation so it stays a textual prefix of `root` (which `is_ignored`'s
/// `strip_prefix(&self.base)` — and watch.rs's strip — require for caller paths).
///
/// We walk `root`'s own ancestors and pick the one whose CANONICAL form equals the canonical `wt`.
/// Comparing canonicalized forms tolerates both a prefix-representation mismatch (Windows verbatim
/// `\\?\C:\…` vs `gix`'s plain `C:\…` workdir) and a symlinked path segment, while keeping the
/// returned base in `root`'s representation. (A depth-count derived from the canonical paths would
/// misindex `root.ancestors()` when a symlink makes `root`'s own component count differ.)
///
/// Shared with `watch.rs` (gitignore watch-dir + subdir-prefix derivation hit the same mismatch).
pub(crate) fn base_under_worktree(root: &Path, wt: &Path) -> Option<PathBuf> {
    let canon = |p: &Path| rag_rat_base::paths::canonicalize_or_simplified(p);
    let canon_wt = canon(wt);
    root.ancestors().find(|ancestor| canon(ancestor) == canon_wt).map(Path::to_path_buf)
}

#[cfg(test)]
#[path = "ignore_rules_tests.rs"]
mod tests;
