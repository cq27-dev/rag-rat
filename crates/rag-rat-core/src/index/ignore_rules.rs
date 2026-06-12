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

use std::path::{Path, PathBuf};

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
/// at or below the file's directory, not the whole repo).
#[derive(Debug)]
struct ScopedGitignore {
    /// Absolute directory the `.gitignore` lives in; the matcher applies only to paths under it.
    dir: PathBuf,
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
#[derive(Debug)]
pub struct IgnoreMatcher {
    /// Stack of compiled gitignores, **outermost first** (root before nested). Matching applies
    /// them in order and lets the deepest matching one win (last write), so a nested whitelist can
    /// override an outer ignore — standard git precedence.
    stack: Vec<ScopedGitignore>,
}

impl IgnoreMatcher {
    /// Compile the matcher for `root`. Collects every `.gitignore` at or below `root` (skipping the
    /// floor dirs themselves, so we never descend into `target/` to read its vendored gitignores)
    /// and compiles each against its own directory. Never fails — a malformed gitignore is dropped
    /// and matching proceeds with what compiled.
    pub fn compile(root: &Path) -> Self {
        let mut stack = Vec::new();
        for dir in collect_gitignore_dirs(root) {
            let mut builder = GitignoreBuilder::new(&dir);
            // `add` returns an Option<Error> for partial-parse problems; ignore it (best-effort,
            // matching ripgrep's own tolerance) rather than failing the whole walk.
            let _ = builder.add(dir.join(".gitignore"));
            if let Ok(gitignore) = builder.build() {
                stack.push(ScopedGitignore { dir, gitignore });
            }
        }
        // Outermost first: shortest dir path sorts before its descendants.
        stack.sort_by_key(|scoped| scoped.dir.as_os_str().len());
        Self { stack }
    }

    /// Whether `path` is ignored. `is_dir` must say whether the path is a directory — gitignore
    /// distinguishes `foo/` (dir-only) from `foo`. A floor-dir name anywhere in the path ignores it
    /// unconditionally (the floor can't be whitelisted away); otherwise each `.gitignore` whose
    /// directory is an ancestor is applied outermost→innermost and the last decision wins, so a
    /// nested `!pattern` whitelist overrides an outer ignore (standard git precedence).
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        if path_contains_floor_dir(path) {
            return true;
        }
        let mut ignored = false;
        for scoped in &self.stack {
            let Ok(rel) = path.strip_prefix(&scoped.dir) else {
                continue; // gitignore in a sibling/unrelated subtree — doesn't govern this path.
            };
            if rel.as_os_str().is_empty() {
                continue; // the gitignore's own directory.
            }
            // `matched_path_or_any_parents` so a file under an ignored *directory* (`generated/`
            // ignores `generated/out.rs`) is caught — plain `matched` only tests the leaf. It walks
            // the path's parents within this gitignore's scope and returns the closest decision.
            match scoped.gitignore.matched_path_or_any_parents(rel, is_dir) {
                Match::Ignore(_) => ignored = true,
                Match::Whitelist(_) => ignored = false,
                Match::None => {},
            }
        }
        ignored
    }
}

/// Whether any component of `path` is a floor directory name.
fn path_contains_floor_dir(path: &Path) -> bool {
    path.components().any(|component| component.as_os_str().to_str().is_some_and(is_floor_dir))
}

/// Collect directories at or below `root` that contain a `.gitignore`, **without** descending into
/// floor dirs (so we don't walk `target/`/`node_modules/` just to read gitignores the floor already
/// excludes). Returns directories (not the `.gitignore` paths) so each can be compiled against its
/// own root for correct nested scoping.
fn collect_gitignore_dirs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.join(".gitignore").is_file() {
            found.push(dir.clone());
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
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
            stack.push(entry.path());
        }
    }
    found
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
    fn negation_unignores_non_floor_dir() {
        let tmp = tempdir();
        write(&tmp.join(".gitignore"), "gen/\n!gen/keep.rs\n");
        write(&tmp.join("gen/keep.rs"), "x\n");
        write(&tmp.join("gen/drop.rs"), "x\n");
        let m = IgnoreMatcher::compile(&tmp);
        assert!(!m.is_ignored(&tmp.join("gen/keep.rs"), false), "whitelisted file un-ignored");
        assert!(m.is_ignored(&tmp.join("gen/drop.rs"), false), "sibling still ignored");
        fs::remove_dir_all(&tmp).ok();
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
