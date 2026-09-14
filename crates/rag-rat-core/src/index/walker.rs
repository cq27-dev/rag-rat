use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use rag_rat_base::config::ResolvedTarget;

use crate::index::ignore_rules::IgnoreMatcher;

/// Walk one target's directories, honoring the repo's compiled `.gitignore` rules (root + nested)
/// plus the hardcoded floor (see [`IgnoreMatcher`]). `ignore` is compiled once per index pass and
/// shared across targets so the walker and watcher classify paths identically (issue #62).
pub fn walk_target(
    root: &Path,
    target: &ResolvedTarget,
    ignore: &IgnoreMatcher,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    for directory in &target.directories {
        let dir = root.join(directory);
        // A configured target directory may be ABSENT under `root` — most importantly when the
        // config came from a linked worktree that added a BRANCH-ONLY target dir, but `Config.root`
        // is anchored to the MAIN checkout (so the shared index has one base commit) (#219 review).
        // Base discovery over main must SKIP the missing dir, not hard-error: there is nothing to
        // index there on main, and the linked worktree's overlay pass picks up the branch-only
        // files separately. Without this, a hook/maintenance launched from such a branch
        // aborted before `refresh_worktree_overlays` could run.
        if !dir.is_dir() {
            continue;
        }
        walk_dir(root, &dir, target, ignore, &mut files)?;
    }
    Ok(files.into_iter().collect())
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    target: &ResolvedTarget,
    ignore: &IgnoreMatcher,
    files: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        // Honor `.gitignore` (root + nested) and the hardcoded floor. We test the full path with
        // its dir-ness so nested-gitignore scoping and `foo/`-style dir-only rules resolve
        // correctly; the floor (`.git`, `.rag-rat`, `target`, …) short-circuits inside
        // `is_ignored`.
        if ignore.is_ignored(&path, file_type.is_dir()) {
            continue;
        }
        if file_type.is_dir() {
            walk_dir(root, &path, target, ignore, files)?;
        } else if file_type.is_file() && is_target_file(root, &path, target) {
            files.insert(path);
        }
    }
    Ok(())
}

fn is_target_file(root: &Path, path: &Path, target: &ResolvedTarget) -> bool {
    // The target's language must CLAIM this extension (not bare `from_path` detection): that's what
    // lets a `cpp` binding index its `.h` headers, which bare detection resolves to C. The file is
    // then parsed as the target's language (see `discovery::target_for_path`).
    if !target.language.claims_path(path) {
        return false;
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    // The include/exclude patterns are `/`-spelled, so match against the SAME rendering
    // `files.path` is stored with — never a blanket backslash rewrite, which would let a pattern
    // claim (or exclude) a Unix file whose NAME contains a backslash by pretending it is nested.
    let relative = rag_rat_base::paths::path_string(relative);
    target.globs_claim(&relative)
}

#[cfg(test)]
#[path = "walker_tests.rs"]
mod tests;
