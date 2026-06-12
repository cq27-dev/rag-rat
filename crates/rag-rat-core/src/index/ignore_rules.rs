//! Shared ignore matcher for the walker (index discovery) and the watcher (event classification).
//!
//! Issue #62: discovery and the watcher both used to skip directories by a *hardcoded* name list
//! (`.git`, `.rag-rat`, `target`, `node_modules`, `dist`, `build`, `coverage`). That missed
//! repo-specific `.gitignore` entries — generated dirs, build outputs, vendored code in
//! non-standard locations — so they could get indexed and (recursively) watched.
//!
//! [`IgnoreMatcher`] compiles the repo's real `.gitignore` rules (root **and** nested gitignores,
//! via ripgrep's [`ignore`] crate) once, and both the walker and the watcher consult it so a path
//! one ignores the other also ignores — no drift. The hardcoded names are kept as a **floor**
//! ([`FLOOR_DIRS`]): they apply even in a non-git tree with no `.gitignore`, and they cover
//! rag-rat's own index dir (`.rag-rat/`), which must never be indexed regardless of gitignore
//! contents.
//!
//! **Two git semantics this matcher must get right** (both were P2 review findings — see issue #62
//! / PR #66):
//!
//! 1. **All checks run on the path *relative to the repo root*, never on its absolute ancestors.**
//!    The walker and the watcher feed absolute paths. A repo can live *under* a directory named
//!    like a floor entry (`/tmp/build/repo`, a checkout below `…/node_modules/…`), so testing floor
//!    components on the absolute path would mark the whole repo ignored → empty discovery. We strip
//!    the root first and test only the in-repo remainder.
//! 2. **Parent exclusion is honored: a file under an ignored directory is *not* re-included** by a
//!    deeper negation unless the *parent directory itself* is re-included. Git evaluates ancestors
//!    top-down and stops descending once a directory is excluded; a nested `gen/.gitignore`
//!    `!keep.rs` under a root-ignored `gen/` does **not** resurrect `gen/keep.rs`. [`is_ignored`]
//!    walks the relative path root→leaf and short-circuits at the first ignored ancestor, exactly
//!    as `ignore`'s own `WalkBuilder` does.

use std::path::{Component, Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Directory names always skipped, gitignore or not — the floor under the compiled `.gitignore`
/// rules. `.rag-rat` is rag-rat's own index dir (indexing it would be a feedback loop); `.git` is
/// never source; the rest are conventional build/dependency/output dirs that a non-git tree (no
/// `.gitignore` to compile) still must not index. In a git tree these usually also appear in
/// `.gitignore`, but the floor guarantees them even when they don't.
const FLOOR_DIRS: &[&str] =
    &[".git", ".rag-rat", ".omx", ".omc", "node_modules", "target", "dist", "build", "coverage"];

/// Whether a single path component matches a floor directory name (see [`FLOOR_DIRS`]).
fn is_floor_dir(name: &str) -> bool {
    FLOOR_DIRS.contains(&name)
}

/// One `.gitignore`, compiled with its own directory as the matching root so its patterns are
/// scoped to that subtree (gitignore semantics: a non-anchored pattern like `skip.rs` matches only
/// at or below the file's directory, not the whole repo). `rel_dir` is that directory **relative to
/// the repo root** — matching strips it off the candidate's repo-relative path before applying the
/// compiled matcher, keeping every decision in repo-relative space (see [`IgnoreMatcher`] finding
/// 1).
#[derive(Debug)]
struct ScopedGitignore {
    /// The `.gitignore`'s directory relative to the repo root (empty for the root `.gitignore`);
    /// the matcher applies only to repo-relative paths under it.
    rel_dir: PathBuf,
    gitignore: Gitignore,
}

/// Compiled ignore rules for one repo root: the floor names plus a per-directory stack of compiled
/// `.gitignore` files (root and nested), each scoped to its own subtree. Built once per walk/watch
/// and shared so the walker and watcher classify paths identically (issue #62).
///
/// **Why a per-directory stack and not one flat `GitignoreBuilder`:** `GitignoreBuilder::add`
/// flattens every file's globs into a single matcher rooted at the *builder* root and gives each
/// non-anchored pattern a `**/` prefix — so a nested `.gitignore` rule `skip.rs` would wrongly
/// match `skip.rs` anywhere in the repo. Compiling each `.gitignore` against *its own* directory
/// and only applying it to descendants of that directory is what makes nesting correct.
///
/// **All matching is repo-relative** (`root` is stripped first), and ancestor directories are
/// evaluated top-down with parent-exclusion short-circuit — see the module docs and [`is_ignored`].
#[derive(Debug)]
pub struct IgnoreMatcher {
    /// The repo root. Absolute candidate paths are stripped to this before any floor / gitignore
    /// check so we never test ancestors *above* the repo (finding 1).
    root: PathBuf,
    /// Stack of compiled gitignores, **outermost first** (root before nested). Matching applies
    /// the ones whose `rel_dir` is an ancestor of the candidate, outermost→innermost, deepest
    /// decision winning, so a nested whitelist can override an outer ignore — standard git
    /// precedence.
    stack: Vec<ScopedGitignore>,
}

impl IgnoreMatcher {
    /// Compile the matcher for `root`. Collects every `.gitignore` at or below `root` (skipping the
    /// floor dirs themselves, and **not descending into a directory an outer rule already
    /// ignores**, so a nested gitignore can't un-ignore files under an excluded parent —
    /// finding 2) and compiles each against its own directory. Never fails — a malformed
    /// gitignore is dropped and matching proceeds with what compiled.
    pub fn compile(root: &Path) -> Self {
        let mut matcher = Self { root: root.to_path_buf(), stack: Vec::new() };
        // Discover and compile gitignores top-down so an outer rule is in `stack` before we decide
        // whether to descend into a child directory (parent-exclusion pruning, finding 2).
        matcher.collect_gitignores(root);
        // Outermost first: shortest rel_dir sorts before its descendants. (collect_gitignores
        // already pushes top-down, but make the invariant explicit and order-independent.)
        matcher.stack.sort_by_key(|scoped| scoped.rel_dir.as_os_str().len());
        matcher
    }

    /// Whether `path` is ignored. `is_dir` must say whether the path is a directory — gitignore
    /// distinguishes `foo/` (dir-only) from `foo`.
    ///
    /// The candidate is first made **repo-relative** by stripping `root`; a path outside the repo
    /// is not governed by these rules (returns `false`). A floor-dir name among the *relative*
    /// components ignores it unconditionally (the floor can't be whitelisted away). Otherwise each
    /// ancestor directory is evaluated root→leaf against the scoped `.gitignore` stack: the first
    /// ancestor that resolves to *ignored* makes the whole path ignored (git parent exclusion — a
    /// deeper `!negation` cannot resurrect a file under an excluded directory). A whitelisted
    /// ancestor clears the ignored state for that level so its subtree is re-evaluated.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false; // outside the repo root — not governed by our rules.
        };
        if rel_contains_floor_dir(rel) {
            return true;
        }
        // Walk ancestor prefixes root→leaf. `ignored` carries the inherited decision from shallower
        // levels; once a directory level lands on `Ignore`, every deeper level inherits it unless a
        // level is explicitly whitelisted (which clears it for that and deeper levels).
        let mut ignored = false;
        let mut prefix = PathBuf::new();
        let mut components = rel.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                continue; // skip `.`/`..`/prefix/root — repo-relative paths shouldn't have them.
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

    /// The combined gitignore decision for one repo-relative path at one level, applying every
    /// scoped gitignore whose `rel_dir` is an ancestor, outermost→innermost (deepest wins). Uses
    /// `matched` (not `matched_path_or_any_parents`): the caller ([`is_ignored`]) already walks
    /// ancestors top-down, so per-level leaf matching is correct and avoids double parent-walking.
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

    /// Recursively collect + compile `.gitignore` files at or below `dir`, pruning floor dirs and —
    /// crucially — any directory an already-collected outer rule ignores (finding 2: don't descend
    /// into an excluded directory to read a nested gitignore that could wrongly un-ignore its
    /// contents). `dir` is absolute; `self.stack` is grown in place so outer rules govern the
    /// descent decision for their children.
    fn collect_gitignores(&mut self, dir: &Path) {
        if dir.join(".gitignore").is_file() {
            let rel_dir = dir.strip_prefix(&self.root).unwrap_or(Path::new("")).to_path_buf();
            let mut builder = GitignoreBuilder::new(dir);
            // `add` returns an Option<Error> for partial-parse problems; ignore it (best-effort,
            // matching ripgrep's own tolerance) rather than failing the whole walk.
            let _ = builder.add(dir.join(".gitignore"));
            if let Ok(gitignore) = builder.build() {
                self.stack.push(ScopedGitignore { rel_dir, gitignore });
            }
        }
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
            self.collect_gitignores(&child);
        }
    }
}

/// Whether any component of the (repo-relative) `rel` path is a floor directory name.
fn rel_contains_floor_dir(rel: &Path) -> bool {
    rel.components().any(|component| component.as_os_str().to_str().is_some_and(is_floor_dir))
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

    #[test]
    fn floor_dirs_ignored_without_any_gitignore() {
        let tmp = tempdir();
        let m = IgnoreMatcher::compile(&tmp);
        assert!(m.is_ignored(&tmp.join("target"), true));
        assert!(m.is_ignored(&tmp.join("target/debug/foo.rs"), false));
        assert!(m.is_ignored(&tmp.join(".rag-rat"), true));
        assert!(m.is_ignored(&tmp.join("node_modules/pkg/index.ts"), false));
        assert!(!m.is_ignored(&tmp.join("src/lib.rs"), false));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn root_gitignore_is_honored() {
        let tmp = tempdir();
        write(&tmp.join(".gitignore"), "generated/\n*.bak\n");
        write(&tmp.join("src/lib.rs"), "fn a() {}\n");
        write(&tmp.join("generated/out.rs"), "fn b() {}\n");
        write(&tmp.join("src/old.bak"), "x\n");
        let m = IgnoreMatcher::compile(&tmp);
        assert!(m.is_ignored(&tmp.join("generated"), true), "gitignored dir");
        assert!(m.is_ignored(&tmp.join("generated/out.rs"), false), "file under gitignored dir");
        assert!(m.is_ignored(&tmp.join("src/old.bak"), false), "gitignored glob");
        assert!(!m.is_ignored(&tmp.join("src/lib.rs"), false), "non-ignored source");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn nested_gitignore_scopes_to_its_subtree() {
        let tmp = tempdir();
        // A nested gitignore ignores `vendor.rs` only under `sub/`, not at the root.
        write(&tmp.join("sub/.gitignore"), "vendor.rs\n");
        write(&tmp.join("sub/vendor.rs"), "x\n");
        write(&tmp.join("vendor.rs"), "x\n");
        let m = IgnoreMatcher::compile(&tmp);
        assert!(m.is_ignored(&tmp.join("sub/vendor.rs"), false), "nested rule applies in subtree");
        assert!(
            !m.is_ignored(&tmp.join("vendor.rs"), false),
            "nested rule does NOT leak to the root",
        );
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn whitelist_negation_unignores() {
        let tmp = tempdir();
        write(&tmp.join(".gitignore"), "build/\n!build/keep.rs\n");
        write(&tmp.join("build/keep.rs"), "x\n");
        write(&tmp.join("build/drop.rs"), "x\n");
        let m = IgnoreMatcher::compile(&tmp);
        // NOTE: `build` is also a FLOOR dir, so the floor wins regardless of negation — assert that
        // the floor is unconditional. (A non-floor whitelisted dir is covered by the next test.)
        assert!(
            m.is_ignored(&tmp.join("build/keep.rs"), false),
            "floor dir beats gitignore negation"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn negation_unignores_whitelisted_dir_subtree() {
        // git can re-include a file under a directory ONLY if the directory itself is whitelisted.
        // `gen/` ignored + `!gen/` re-included + `gen/skip/` re-ignored: `gen/a.rs` is back,
        // `gen/skip/b.rs` is out.
        let tmp = tempdir();
        write(&tmp.join(".gitignore"), "gen/\n!gen/\ngen/skip/\n");
        write(&tmp.join("gen/a.rs"), "x\n");
        write(&tmp.join("gen/skip/b.rs"), "x\n");
        let m = IgnoreMatcher::compile(&tmp);
        assert!(!m.is_ignored(&tmp.join("gen/a.rs"), false), "re-included dir's file un-ignored");
        assert!(m.is_ignored(&tmp.join("gen/skip/b.rs"), false), "re-ignored subdir still out");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn nested_negation_under_ignored_parent_stays_ignored() {
        // FINDING 2: a nested `.gitignore` inside a directory ignored by an OUTER rule must NOT
        // re-include files. Git stops descending at the excluded `gen/`, so `gen/.gitignore`'s
        // `!keep.rs` is never consulted — `gen/keep.rs` stays ignored.
        let tmp = tempdir();
        write(&tmp.join(".gitignore"), "gen/\n");
        write(&tmp.join("gen/.gitignore"), "!keep.rs\n");
        write(&tmp.join("gen/keep.rs"), "x\n");
        write(&tmp.join("gen/drop.rs"), "x\n");
        let m = IgnoreMatcher::compile(&tmp);
        assert!(
            m.is_ignored(&tmp.join("gen/keep.rs"), false),
            "nested negation under an ignored parent must NOT re-include",
        );
        assert!(m.is_ignored(&tmp.join("gen/drop.rs"), false), "sibling under ignored parent out");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn flat_negation_unignores_non_floor_file() {
        // Distinct from the nested case: a SINGLE gitignore with `gen/` + `!gen/keep.rs` does NOT
        // re-include either, because git can't reach inside an excluded directory even from the
        // same file. Both stay ignored. (This is the git-correct behavior; the pre-fix stack
        // wrongly un-ignored here.)
        let tmp = tempdir();
        write(&tmp.join(".gitignore"), "gen/\n!gen/keep.rs\n");
        write(&tmp.join("gen/keep.rs"), "x\n");
        write(&tmp.join("gen/drop.rs"), "x\n");
        let m = IgnoreMatcher::compile(&tmp);
        assert!(m.is_ignored(&tmp.join("gen/keep.rs"), false), "no reinclude under excluded dir");
        assert!(m.is_ignored(&tmp.join("gen/drop.rs"), false), "sibling still ignored");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn repo_root_under_floor_named_ancestor_still_indexes() {
        // FINDING 1: the repo lives under a directory named like a floor entry (`build`). The floor
        // check must run on the path RELATIVE to the root, never on the absolute ancestors — so the
        // repo's own files are still indexed.
        let outer = tempdir().join("build");
        let root = outer.join("my-repo");
        write(&root.join("src/lib.rs"), "fn a() {}\n");
        write(&root.join("target/debug/built.rs"), "fn b() {}\n");
        let m = IgnoreMatcher::compile(&root);
        assert!(
            !m.is_ignored(&root.join("src/lib.rs"), false),
            "repo under a floor-named ancestor still indexes its files",
        );
        // The repo's OWN `target/` (a relative floor component) is still skipped.
        assert!(m.is_ignored(&root.join("target/debug/built.rs"), false), "in-repo floor skipped");
        // A path outside the repo root is simply not governed (not ignored) by our rules.
        assert!(!m.is_ignored(&outer.join("sibling.rs"), false), "outside-root path not governed");
        fs::remove_dir_all(outer.parent().unwrap()).ok();
    }

    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ragrat-ignore-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
