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
    ".rag-rat",
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
/// for sources. What actually has to be excluded is the index the live clangd oracle makes the
/// checkout write to itself — `.cache/clangd/` — which no clangd flag or environment variable can
/// relocate. Unfloored, those writes raise watcher events, which schedule maintenance passes,
/// which run the live oracle, which keeps clangd indexing: a loop the repo feeds itself.
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
fn rel_contains_floor_dir(rel: &Path) -> bool {
    // A component that is not valid UTF-8 can match no floor entry, but it must still BREAK a
    // floor path's run of consecutive components — dropping it would splice its neighbours
    // together and read `.cache/<non-utf8>/clangd` as `.cache/clangd`, unconditionally excluding a
    // tracked tree that merely happens to sit between them.
    let components: Vec<Option<&str>> =
        rel.components().map(|component| component.as_os_str().to_str()).collect();
    if components.iter().flatten().any(|name| is_floor_dir(name)) {
        return true;
    }
    FLOOR_PATHS.iter().any(|floor| {
        components
            .windows(floor.len())
            .any(|window| window.iter().zip(*floor).all(|(got, want)| *got == Some(*want)))
    })
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
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let canon_wt = canon(wt);
    root.ancestors().find(|ancestor| canon(ancestor) == canon_wt).map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Compile scoping discovery to the whole root (the common case in these unit tests).
    fn compile(root: &Path) -> IgnoreMatcher {
        IgnoreMatcher::compile(root, &[])
    }

    /// Initialize a real (empty) Git repo at `dir` so worktree-root resolution returns `dir`. Used
    /// by the ancestor-chain test; the others don't need git (base falls back to `root`).
    fn git_init(dir: &Path) {
        rag_rat_base::test_git::run(dir, &["init", "-q"]);
    }

    #[test]
    fn floor_dirs_ignored_without_any_gitignore() {
        let (_scratch, tmp) = tempdir();
        let m = compile(&tmp);
        assert!(m.is_ignored(&tmp.join("target"), true));
        assert!(m.is_ignored(&tmp.join("target/debug/foo.rs"), false));
        assert!(m.is_ignored(&tmp.join(".rag-rat"), true));
        assert!(m.is_ignored(&tmp.join(".claude/settings.json"), false));
        assert!(m.is_ignored(&tmp.join(".codex/config.toml"), false));
        assert!(m.is_ignored(&tmp.join("node_modules/pkg/index.ts"), false));
        assert!(m.is_ignored(&tmp.join(".build/checkouts/Dep/Sources/Dep.swift"), false));
        assert!(!m.is_ignored(&tmp.join("src/lib.rs"), false));
    }

    #[test]
    fn the_clangd_index_floor_is_relative_to_each_checkouts_own_root() {
        // A linked worktree shares the database with the main checkout, and the live oracle runs
        // per checkout — each spawning its own clangd, each writing its own `.cache/clangd`. The
        // floor is applied to the path relative to THAT checkout's root, so a linked worktree's
        // index is floored on its own account and the main checkout's tree is untouched by it.
        let (_scratch, main) = tempdir();
        git_init(&main);
        write(&main.join(".gitignore"), "ignored-by-git/\n");
        write(&main.join("src/lib.c"), "int a(void){return 0;}\n");
        let linked = main.join("wt");
        write(&linked.join("src/lib.c"), "int b(void){return 0;}\n");

        // Compiled against the LINKED checkout's root, with the main checkout as the gitignore
        // base above it — the ancestor-chain shape a linked worktree actually has.
        let m = IgnoreMatcher::compile(&linked, &[PathBuf::from(".")]);
        assert!(m.is_ignored(&linked.join(".cache/clangd/index/a.idx"), false));
        assert!(!m.is_ignored(&linked.join("src/lib.c"), false), "its own sources still index");
        assert!(
            !m.is_ignored(&linked.join(".cache/generated/api.c"), false),
            "and the narrow floor still does not swallow the rest of a tracked .cache",
        );
        // The main checkout's matcher floors its OWN index, and neither disturbs the other's
        // sources — the sibling-preservation property this topology exists to check.
        let main_matcher = IgnoreMatcher::compile(&main, &[PathBuf::from(".")]);
        assert!(main_matcher.is_ignored(&main.join(".cache/clangd/index/a.idx"), false));
        assert!(!main_matcher.is_ignored(&main.join("src/lib.c"), false));
        assert!(
            main_matcher.is_ignored(&linked.join(".cache/clangd/index/a.idx"), false),
            "a nested checkout's clangd index is floored from either side",
        );
    }

    #[test]
    fn clangds_own_index_is_floored_without_swallowing_every_dot_cache() {
        // The live clangd oracle makes the checkout's OWN tooling write here: clangd persists its
        // background index to `.cache/clangd/index/` and no flag relocates it. Unfloored, those
        // writes raise watcher events that schedule passes that run clangd that writes again.
        let (_scratch, tmp) = tempdir();
        let m = compile(&tmp);
        assert!(m.is_ignored(&tmp.join(".cache/clangd"), true));
        assert!(m.is_ignored(&tmp.join(".cache/clangd/index/main.c.ABC123.idx"), false));
        // …but the floor is unconditional and cannot be whitelisted back, so it must NOT swallow a
        // `.cache/` a repo genuinely tracks, nor a nested one it happens to own.
        assert!(!m.is_ignored(&tmp.join(".cache"), true));
        assert!(!m.is_ignored(&tmp.join(".cache/generated/api.ts"), false));
        assert!(!m.is_ignored(&tmp.join("src/.cache/fixtures/sample.c"), false));
        // The floor is a consecutive-component match, so a same-named pair deeper in the tree is
        // floored too (a nested checkout's clangd index), while `clangd` alone never is.
        assert!(m.is_ignored(&tmp.join("vendor/dep/.cache/clangd/index/a.idx"), false));
        assert!(!m.is_ignored(&tmp.join("src/clangd/wrapper.c"), false));
    }

    #[test]
    fn python_venv_floor_dirs_ignored_but_generic_env_indexed() {
        let (_scratch, tmp) = tempdir();
        let m = compile(&tmp);
        // Dotted tooling trees can never be first-party import packages, so they're floored even
        // with no .gitignore (#181).
        assert!(m.is_ignored(&tmp.join(".tox/py311/lib/foo.py"), false));
        assert!(m.is_ignored(&tmp.join(".nox/session/foo.py"), false));
        // site-packages stays floored regardless of the enclosing venv dir's name.
        assert!(m.is_ignored(&tmp.join("env/lib/python3.11/site-packages/dep.py"), false));
        // …but bare names that CAN be a real import package are NOT floored globally: `virtualenv`
        // is itself a PyPI package (`src/virtualenv/…` is first-party, #181 review), and
        // `env`/`.env` are too generic. Their own non-site-packages files stay indexable.
        assert!(!m.is_ignored(&tmp.join("src/virtualenv/__init__.py"), false));
        assert!(!m.is_ignored(&tmp.join("env/settings.py"), false));
        assert!(!m.is_ignored(&tmp.join(".env/config.py"), false));
    }

    #[test]
    fn root_gitignore_is_honored() {
        let (_scratch, tmp) = tempdir();
        write(&tmp.join(".gitignore"), "generated/\n*.bak\n");
        write(&tmp.join("src/lib.rs"), "fn a() {}\n");
        write(&tmp.join("generated/out.rs"), "fn b() {}\n");
        write(&tmp.join("src/old.bak"), "x\n");
        let m = compile(&tmp);
        assert!(m.is_ignored(&tmp.join("generated"), true), "gitignored dir");
        assert!(m.is_ignored(&tmp.join("generated/out.rs"), false), "file under gitignored dir");
        assert!(m.is_ignored(&tmp.join("src/old.bak"), false), "gitignored glob");
        assert!(!m.is_ignored(&tmp.join("src/lib.rs"), false), "non-ignored source");
    }

    #[test]
    fn nested_gitignore_scopes_to_its_subtree() {
        let (_scratch, tmp) = tempdir();
        // A nested gitignore ignores `vendor.rs` only under `sub/`, not at the root.
        write(&tmp.join("sub/.gitignore"), "vendor.rs\n");
        write(&tmp.join("sub/vendor.rs"), "x\n");
        write(&tmp.join("vendor.rs"), "x\n");
        let m = compile(&tmp);
        assert!(m.is_ignored(&tmp.join("sub/vendor.rs"), false), "nested rule applies in subtree");
        assert!(
            !m.is_ignored(&tmp.join("vendor.rs"), false),
            "nested rule does NOT leak to the root",
        );
    }

    #[test]
    fn whitelist_negation_unignores() {
        let (_scratch, tmp) = tempdir();
        write(&tmp.join(".gitignore"), "build/\n!build/keep.rs\n");
        write(&tmp.join("build/keep.rs"), "x\n");
        write(&tmp.join("build/drop.rs"), "x\n");
        let m = compile(&tmp);
        // NOTE: `build` is also a FLOOR dir, so the floor wins regardless of negation — assert that
        // the floor is unconditional. (A non-floor whitelisted dir is covered by the next test.)
        assert!(
            m.is_ignored(&tmp.join("build/keep.rs"), false),
            "floor dir beats gitignore negation"
        );
    }

    #[test]
    fn negation_unignores_whitelisted_dir_subtree() {
        // git can re-include a file under a directory ONLY if the directory itself is whitelisted.
        // `gen/` ignored + `!gen/` re-included + `gen/skip/` re-ignored: `gen/a.rs` is back,
        // `gen/skip/b.rs` is out.
        let (_scratch, tmp) = tempdir();
        write(&tmp.join(".gitignore"), "gen/\n!gen/\ngen/skip/\n");
        write(&tmp.join("gen/a.rs"), "x\n");
        write(&tmp.join("gen/skip/b.rs"), "x\n");
        let m = compile(&tmp);
        assert!(!m.is_ignored(&tmp.join("gen/a.rs"), false), "re-included dir's file un-ignored");
        assert!(m.is_ignored(&tmp.join("gen/skip/b.rs"), false), "re-ignored subdir still out");
    }

    #[test]
    fn nested_negation_under_ignored_parent_stays_ignored() {
        // FINDING 2: a nested `.gitignore` inside a directory ignored by an OUTER rule must NOT
        // re-include files. Git stops descending at the excluded `gen/`, so `gen/.gitignore`'s
        // `!keep.rs` is never consulted — `gen/keep.rs` stays ignored.
        let (_scratch, tmp) = tempdir();
        write(&tmp.join(".gitignore"), "gen/\n");
        write(&tmp.join("gen/.gitignore"), "!keep.rs\n");
        write(&tmp.join("gen/keep.rs"), "x\n");
        write(&tmp.join("gen/drop.rs"), "x\n");
        let m = compile(&tmp);
        assert!(
            m.is_ignored(&tmp.join("gen/keep.rs"), false),
            "nested negation under an ignored parent must NOT re-include",
        );
        assert!(m.is_ignored(&tmp.join("gen/drop.rs"), false), "sibling under ignored parent out");
    }

    #[test]
    fn flat_negation_unignores_non_floor_file() {
        // Distinct from the nested case: a SINGLE gitignore with `gen/` + `!gen/keep.rs` does NOT
        // re-include either, because git can't reach inside an excluded directory even from the
        // same file. Both stay ignored. (This is the git-correct behavior.)
        let (_scratch, tmp) = tempdir();
        write(&tmp.join(".gitignore"), "gen/\n!gen/keep.rs\n");
        write(&tmp.join("gen/keep.rs"), "x\n");
        write(&tmp.join("gen/drop.rs"), "x\n");
        let m = compile(&tmp);
        assert!(m.is_ignored(&tmp.join("gen/keep.rs"), false), "no reinclude under excluded dir");
        assert!(m.is_ignored(&tmp.join("gen/drop.rs"), false), "sibling still ignored");
    }

    #[test]
    fn repo_root_under_floor_named_ancestor_still_indexes() {
        // FINDING 1: the repo lives under a directory named like a floor entry (`build`). The floor
        // check must run on the path RELATIVE to the root, never on the absolute ancestors — so the
        // repo's own files are still indexed.
        let (_scratch, outer_base) = tempdir();
        let outer = outer_base.join("build");
        let root = outer.join("my-repo");
        write(&root.join("src/lib.rs"), "fn a() {}\n");
        write(&root.join("target/debug/built.rs"), "fn b() {}\n");
        let m = compile(&root);
        assert!(
            !m.is_ignored(&root.join("src/lib.rs"), false),
            "repo under a floor-named ancestor still indexes its files",
        );
        // The repo's OWN `target/` (a relative floor component) is still skipped.
        assert!(m.is_ignored(&root.join("target/debug/built.rs"), false), "in-repo floor skipped");
        // A path outside the repo root is simply not governed (not ignored) by our rules.
        assert!(!m.is_ignored(&outer.join("sibling.rs"), false), "outside-root path not governed");
    }

    #[test]
    fn worktree_root_gitignore_governs_subdirectory_config_root() {
        // FINDING 3 (round 3): `config.root` is a subdirectory (`crates`) of a larger Git worktree.
        // The worktree-root `.gitignore` rules Git would apply to paths under that subdirectory
        // must be honored — files Git ignores must not be indexed.
        let (_scratch, wt) = tempdir();
        git_init(&wt);
        // Worktree-root .gitignore ignores every `*.gen.rs` and the `vendored/` dir, repo-wide.
        write(&wt.join(".gitignore"), "*.gen.rs\nvendored/\n");
        let sub = wt.join("crates");
        write(&sub.join("lib.rs"), "fn a() {}\n");
        write(&sub.join("schema.gen.rs"), "fn g() {}\n");
        write(&sub.join("vendored/dep.rs"), "fn v() {}\n");

        // Compile with `config.root = crates` — the ancestor chain must pull in the worktree-root
        // `.gitignore` even though it sits ABOVE config.root.
        let m = IgnoreMatcher::compile(&sub, &[PathBuf::from(".")]);
        assert!(!m.is_ignored(&sub.join("lib.rs"), false), "normal source under subdir indexes");
        assert!(
            m.is_ignored(&sub.join("schema.gen.rs"), false),
            "worktree-root glob applies under the subdir config.root (finding 3)",
        );
        assert!(
            m.is_ignored(&sub.join("vendored/dep.rs"), false),
            "worktree-root dir rule applies under the subdir config.root (finding 3)",
        );
    }

    #[test]
    fn discovery_does_not_walk_unignored_sibling_outside_targets() {
        // ROUND 3 (scoping): a large unignored sibling dir OUTSIDE the configured target trees must
        // not be scanned for nested `.gitignore`s. We assert by behavior: a nested `.gitignore` in
        // the sibling is never compiled, so it has no effect on classification — and, conversely, a
        // nested `.gitignore` INSIDE the target tree IS picked up.
        let (_scratch, root) = tempdir();
        // Target tree: `src`. Sibling tree: `huge` (not a target).
        write(&root.join("src/.gitignore"), "skip.rs\n");
        write(&root.join("src/skip.rs"), "x\n");
        // Sibling's nested gitignore would, if scanned, ignore `marker.rs`. Scoped discovery must
        // NOT read it, so `marker.rs` stays unignored.
        write(&root.join("huge/.gitignore"), "marker.rs\n");
        write(&root.join("huge/marker.rs"), "x\n");

        let m = IgnoreMatcher::compile(&root, &[PathBuf::from("src")]);
        // In-target nested gitignore is honored.
        assert!(
            m.is_ignored(&root.join("src/skip.rs"), false),
            "in-target nested gitignore honored"
        );
        // Out-of-target sibling's nested gitignore was never compiled → no effect.
        assert!(
            !m.is_ignored(&root.join("huge/marker.rs"), false),
            "sibling outside target trees is not scanned for nested gitignores (scoping)",
        );
    }

    #[test]
    fn target_ancestor_gitignore_governs_nested_target_without_scanning_siblings() {
        let (_scratch, root) = tempdir();
        write(&root.join("src/.gitignore"), "generated/\n");
        write(&root.join("src/generated/lib.rs"), "x\n");
        write(&root.join("src/sibling/.gitignore"), "marker.rs\n");
        write(&root.join("src/sibling/marker.rs"), "x\n");

        let m = IgnoreMatcher::compile(&root, &[PathBuf::from("src/generated")]);
        assert!(
            m.is_ignored(&root.join("src/generated/lib.rs"), false),
            "target ancestor .gitignore must govern nested target files",
        );
        assert!(
            !m.is_ignored(&root.join("src/sibling/marker.rs"), false),
            "target ancestor compilation must not recursively scan unindexed siblings",
        );
    }

    fn tempdir() -> (rag_rat_base::test_scratch::ScratchDir, std::path::PathBuf) {
        let guard = rag_rat_base::test_scratch::ScratchDir::new("ignore");
        // Canonicalize so the absolute paths we build match what worktree-root resolution returns
        // (macOS /tmp is a symlink to /private/tmp; git reports the canonical form). The guard
        // still removes the directory: the canonical form is the same inode.
        let root = guard.path().canonicalize().unwrap_or_else(|_| guard.path().to_path_buf());
        (guard, root)
    }

    // base_under_worktree must return the worktree root in `root`'s OWN representation even when a
    // symlinked segment makes `root`'s component count differ from its canonical form — a depth
    // count derived from the canonical paths would misindex `root.ancestors()` and return a wrong
    // base (the parent of the worktree root, here), breaking the strip_prefix contract.
    #[cfg(unix)]
    #[test]
    fn base_under_worktree_handles_symlinked_root_segment() {
        use std::os::unix::fs::symlink;
        let (_scratch, wt) = tempdir(); // canonical worktree root
        fs::create_dir_all(wt.join("a/b")).unwrap();
        // `<wt>/link` (1 component) resolves to `<wt>/a/b` (2 components) — counts differ.
        let link = wt.join("link");
        symlink(wt.join("a/b"), &link).unwrap();
        // `root` (= the symlink) is NOT canonicalized — the case the helper must tolerate; `wt` is
        // a genuine textual ancestor of it.
        let base = base_under_worktree(&link, &wt);
        assert_eq!(base.as_deref(), Some(wt.as_path()), "base must be wt in root's representation");
        assert!(
            link.strip_prefix(base.unwrap()).is_ok(),
            "base must stay a textual prefix of root"
        );
    }
}
