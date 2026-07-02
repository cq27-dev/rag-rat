//! Machine-stable repo identity: a content-derived `repo_id` (the repo's root-commit hash) plus a
//! cosmetic display name. This id is what the consolidated global database scopes every repo's
//! rows and memories by (memory-sync phase A). It is deliberately derived from the commit graph,
//! not the on-disk path, so worktrees, clones, and re-clones of the same repo share one identity.

use std::path::Path;

use anyhow::Context;

/// A repo's identity for the registry: the scoping key + a human label.
pub struct RepoIdentity {
    /// Machine-stable, content-derived (root-commit hash) unless pinned via `rag-rat.toml`. Never
    /// derived from the on-disk path.
    pub repo_id: String,
    /// The working-tree directory name — cosmetic only, never an identity input.
    pub display_name: String,
}

/// Resolve a repo's identity from its working tree at `root`.
///
/// The default `repo_id` is the **lexicographically smallest root-commit hash** reachable from
/// HEAD. A linear history has exactly one root (parentless) commit; a history stitched from
/// several orphan branches has several, and choosing the smallest by byte order makes the id
/// deterministic across machines regardless of which root the walk reaches first.
///
/// `override_id` (from `rag-rat.toml`'s `[index] repo_id`) wins outright when set and non-empty —
/// the escape hatch for forks that must NOT share identity with their upstream, and for pinning an
/// id on a repo that has no commits yet.
///
/// Errors when the directory is not a git repository, has no commits (unborn HEAD), or has no
/// reachable root commit — and no override was supplied.
pub fn resolve_repo_identity(
    root: &Path,
    override_id: Option<&str>,
) -> anyhow::Result<RepoIdentity> {
    let display_name =
        root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();

    if let Some(id) = override_id.map(str::trim).filter(|id| !id.is_empty()) {
        return Ok(RepoIdentity { repo_id: id.to_string(), display_name });
    }

    let repo_id = smallest_root_commit(root)?;
    Ok(RepoIdentity { repo_id, display_name })
}

/// The lexicographically smallest hash among the parentless commits reachable from HEAD. Mirrors
/// the git-history walk (`index::git_history`): discover the repo, resolve HEAD, `rev_walk`, and
/// keep the commits whose first parent is absent.
fn smallest_root_commit(root: &Path) -> anyhow::Result<String> {
    let repo = crate::index::discover_repo(root)?;
    // Fails on a non-git dir (already handled above) and on an unborn HEAD (empty repo).
    let head_id = repo.head_id().context("repository has no commits (unborn HEAD)")?.detach();

    let mut roots: Vec<String> = Vec::new();
    for info in repo.rev_walk([head_id]).all()? {
        let info = info?;
        let commit = repo.find_commit(info.id)?;
        // A root commit has no parents; `parent_ids().next().is_none()` is the same predicate the
        // history walk uses to detect roots / shallow boundaries.
        if commit.parent_ids().next().is_none() {
            roots.push(info.id.to_hex().to_string());
        }
    }
    roots.sort();
    roots.into_iter().next().context("repository history has no root commit")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let mut root = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        root.push(format!("rag-rat-repo-identity-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn git_out(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn init_repo(root: &Path) {
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.email", "t@e"]);
        git(root, &["config", "user.name", "t"]);
    }

    /// All root (parentless) commit hashes reachable from HEAD, sorted — the ground truth the
    /// resolver's "smallest root commit" must agree with, computed independently via git.
    fn root_hashes(root: &Path) -> Vec<String> {
        let mut hashes: Vec<String> = git_out(root, &["rev-list", "--max-parents=0", "HEAD"])
            .lines()
            .map(str::to_string)
            .collect();
        hashes.sort();
        hashes
    }

    #[test]
    fn single_root_repo_resolves_to_its_root_commit() {
        let root = temp_root();
        init_repo(&root);
        git(&root, &["commit", "--allow-empty", "-q", "-m", "genesis"]);

        let expected = root_hashes(&root);
        assert_eq!(expected.len(), 1, "a linear history has exactly one root");

        let identity = resolve_repo_identity(&root, None).unwrap();
        assert_eq!(identity.repo_id, expected[0]);
        assert_eq!(identity.display_name, root.file_name().unwrap().to_string_lossy());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn multi_root_history_resolves_to_lexicographically_smallest() {
        let root = temp_root();
        init_repo(&root);
        // Root A on main, then an unrelated orphan root B, then merge so HEAD reaches BOTH.
        git(&root, &["commit", "--allow-empty", "-q", "-m", "root-a"]);
        git(&root, &["checkout", "-q", "--orphan", "orphan-b"]);
        git(&root, &["commit", "--allow-empty", "-q", "-m", "root-b"]);
        git(&root, &["checkout", "-q", "main"]);
        git(&root, &["merge", "-q", "--allow-unrelated-histories", "--no-edit", "orphan-b"]);

        let roots = root_hashes(&root);
        assert_eq!(roots.len(), 2, "merged orphan histories give two roots");

        let identity = resolve_repo_identity(&root, None).unwrap();
        assert_eq!(identity.repo_id, roots[0], "smallest root hash by byte order wins");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn override_wins_over_derived_id() {
        let root = temp_root();
        init_repo(&root);
        git(&root, &["commit", "--allow-empty", "-q", "-m", "genesis"]);
        let derived = root_hashes(&root).remove(0);

        let identity = resolve_repo_identity(&root, Some("  pinned-id  ")).unwrap();
        assert_eq!(identity.repo_id, "pinned-id", "override is trimmed and wins");
        assert_ne!(identity.repo_id, derived);

        // An empty/whitespace override falls through to the derived id.
        let blank = resolve_repo_identity(&root, Some("   ")).unwrap();
        assert_eq!(blank.repo_id, derived);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn non_git_dir_without_override_is_an_error() {
        let root = temp_root(); // created, but never `git init`ed
        assert!(resolve_repo_identity(&root, None).is_err());
        // ...but an override lets a non-git dir resolve (a repo with no commits can still be
        // pinned).
        let identity = resolve_repo_identity(&root, Some("pinned")).unwrap();
        assert_eq!(identity.repo_id, "pinned");
        std::fs::remove_dir_all(&root).ok();
    }
}
