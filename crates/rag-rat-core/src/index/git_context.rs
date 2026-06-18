//! Git context for the active checkout: changed paths, worktree contexts, pathspec/path mapping,
//! and raw git invocation.

use super::*;

#[derive(Debug, Default)]
pub(crate) struct GitChangedPaths {
    pub(crate) changed: BTreeSet<PathBuf>,
    pub(crate) deleted: BTreeSet<PathBuf>,
}

pub(crate) fn git_changed_paths(root: &Path) -> anyhow::Result<GitChangedPaths> {
    let repo = gix::discover(root)?;
    let worktree_root = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("git repository has no worktree"))?
        .to_path_buf();
    let pathspec = config_root_pathspec(&worktree_root, root);
    let mut paths = GitChangedPaths::default();

    for item in repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::Files)
        .tree_index_track_renames(tree_index::TrackRenames::Disabled)
        .into_iter([pathspec])?
    {
        let item = item?;
        let Some(path) = repo_relative_path_to_config_path(&worktree_root, root, item.location())
        else {
            continue;
        };
        if root.join(&path).exists() {
            if !paths.deleted.contains(&path) {
                paths.changed.insert(path);
            }
        } else {
            paths.changed.remove(&path);
            paths.deleted.insert(path);
        }
    }

    Ok(paths)
}

pub(crate) fn repo_relative_path_to_config_path(
    worktree_root: &Path,
    config_root: &Path,
    repo_relative_path: &gix::bstr::BStr,
) -> Option<PathBuf> {
    let path = PathBuf::from(repo_relative_path.to_str_lossy().as_ref());
    worktree_root.join(path).strip_prefix(config_root).ok().map(Path::to_path_buf)
}

pub(crate) fn config_root_pathspec(worktree_root: &Path, config_root: &Path) -> BString {
    let relative = config_root.strip_prefix(worktree_root).unwrap_or_else(|_| Path::new(""));
    let relative = path_string(relative);
    if relative.is_empty() || relative == "." {
        BString::from("*")
    } else {
        BString::from(format!("{relative}/**"))
    }
}

pub(crate) fn matches_simple_pattern(path: &str, pattern: &str) -> bool {
    if let Some(extension) = pattern.strip_prefix("**/*.") {
        return path.ends_with(&format!(".{extension}"));
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path.starts_with(prefix);
    }
    path == pattern || path.contains(pattern.trim_matches('*'))
}

/// HEAD commit sha for `root` via gix, or empty if unborn / not a repo (matching the old
/// `rev-parse HEAD` failure behavior).
pub(crate) fn head_sha(root: &Path) -> String {
    gix::discover(root)
        .ok()
        .and_then(|repo| repo.head_id().ok().map(|id| id.to_hex().to_string()))
        .unwrap_or_default()
}

/// Whether the worktree has any uncommitted change (tracked modifications + untracked files), the
/// gix equivalent of a non-empty `git status --porcelain`. Lazy: stops at the first change.
pub(crate) fn is_worktree_dirty(root: &Path) -> bool {
    let Ok(repo) = gix::discover(root) else {
        return false;
    };
    let Ok(platform) = repo.status(gix::progress::Discard) else {
        return false;
    };
    // No pathspec → the whole worktree; lazy, so `.next()` stops at the first change.
    let Ok(mut changes) =
        platform.untracked_files(UntrackedFiles::Files).into_iter(None::<gix::bstr::BString>)
    else {
        return false;
    };
    changes.next().is_some()
}

/// The active-checkout `(commit_sha, worktree_id)` keys for `root`, as `open_config` derives them.
/// `pub` so out-of-crate callers that open an index by path (benches mirroring the production
/// `open_config` path, integration tests) can install the same active-checkout scope `search` uses.
pub fn resolve_git_context(root: &Path) -> (String, String) {
    let commit_sha = head_sha(root);
    let worktree_id = root.to_string_lossy().trim_end_matches('/').to_string();
    (commit_sha, worktree_id)
}

/// The live (commit_sha, worktree_id) keys across every worktree that shares this repo, from
/// `git worktree list --porcelain`. Each worktree contributes its HEAD commit (for clean rows)
/// and its path (for dirty/overlay rows). Returns empty vecs outside a git worktree.
pub(crate) fn live_worktree_contexts(root: &Path) -> (Vec<String>, Vec<String>) {
    let mut commits = Vec::new();
    let mut worktrees = Vec::new();
    let Ok(repo) = gix::discover(root) else {
        return (commits, worktrees);
    };
    let push_path = |worktrees: &mut Vec<String>, path: &Path| {
        worktrees.push(path.to_string_lossy().trim_end_matches('/').to_string());
    };
    // The main worktree (gix `worktrees()` lists only the LINKED ones).
    if let Some(workdir) = repo.workdir() {
        push_path(&mut worktrees, workdir);
    }
    if let Ok(id) = repo.head_id() {
        commits.push(id.to_hex().to_string());
    }
    // Linked worktrees: path from the proxy, HEAD from opening each one.
    if let Ok(proxies) = repo.worktrees() {
        for proxy in proxies {
            if let Ok(base) = proxy.base() {
                push_path(&mut worktrees, &base);
            }
            if let Ok(linked) = proxy.into_repo()
                && let Ok(id) = linked.head_id()
            {
                commits.push(id.to_hex().to_string());
            }
        }
    }
    (commits, worktrees)
}
