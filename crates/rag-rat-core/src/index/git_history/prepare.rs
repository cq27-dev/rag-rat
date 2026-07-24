use super::apply::history_cursors;
use super::read::{read_history, read_history_excluding};
use super::*;

pub(crate) fn prepare(root: &Path) -> anyhow::Result<PreparedGitHistory> {
    let Some(repo) = git_repo(root) else {
        return Ok(PreparedGitHistory {
            repo: None,
            commits: Vec::new(),
            changes: Vec::new(),
            complete: true,
            mode: PreparedGitHistoryMode::Full,
        });
    };
    // One streaming gix revwalk pinned to the captured HEAD produces both the commit records and
    // their file changes — no `git log` subprocess and no full-history stdout buffer, so memory
    // stays bounded on deep-history repos (#212). Pinning to the captured sha (not implicit HEAD)
    // keeps `prepare` atomic w.r.t. a concurrent commit, so the stored `git_history_indexed_head`
    // stays honest for the reload gate.
    let history = read_history(root, &repo.worktree_root, &repo.head);
    Ok(PreparedGitHistory {
        repo: Some(repo),
        commits: history.commits,
        changes: history.changes,
        complete: history.complete,
        mode: PreparedGitHistoryMode::Full,
    })
}

pub(crate) fn prepare_with_plan(
    root: &Path,
    plan: GitHistoryPreparePlan,
) -> anyhow::Result<PreparedGitHistory> {
    match plan {
        GitHistoryPreparePlan::Full => prepare(root),
        GitHistoryPreparePlan::Append { expected } => prepare_append(root, expected),
    }
}

fn prepare_append(root: &Path, expected: HistoryCursors) -> anyhow::Result<PreparedGitHistory> {
    let Some(repo) = git_repo(root) else {
        return Ok(PreparedGitHistory {
            repo: None,
            commits: Vec::new(),
            changes: Vec::new(),
            complete: true,
            mode: PreparedGitHistoryMode::Full,
        });
    };
    if expected.complete
        && !repo.shallow
        && root_key(root) == expected.root_key
        && is_fast_forward(root, &expected.head, &repo.head)
        && let Ok(history) =
            read_history_excluding(root, &repo.worktree_root, &repo.head, &expected.head)
        && history.complete
    {
        return Ok(PreparedGitHistory {
            repo: Some(repo),
            commits: history.commits,
            changes: history.changes,
            complete: true,
            mode: PreparedGitHistoryMode::Append { expected },
        });
    }
    prepare(root)
}

pub(crate) fn prepare_plan(conn: &Connection, root: &Path) -> GitHistoryPreparePlan {
    let Some(repo) = git_repo(root) else {
        return GitHistoryPreparePlan::Full;
    };
    if repo.shallow {
        return GitHistoryPreparePlan::Full;
    }
    let probe = || -> anyhow::Result<GitHistoryPreparePlan> {
        let repo_id = schema::active_repo_id(conn)?;
        let expected = history_cursors(conn, &repo_id)?
            .ok_or_else(|| anyhow::anyhow!("missing git history cursor"))?;
        let has_rows = scoped_table_row_count(conn, "git_commits", &repo_id)? > 0;
        if !has_rows
            || expected.head == repo.head
            || expected.root_key != root_key(root)
            || expected.shallow
            || !is_fast_forward(root, &expected.head, &repo.head)
        {
            return Ok(GitHistoryPreparePlan::Full);
        }
        Ok(GitHistoryPreparePlan::Append { expected })
    };
    probe().unwrap_or(GitHistoryPreparePlan::Full)
}
/// The enclosing Git worktree root for `root` (the directory `git rev-parse --show-toplevel`
/// reports), or `None` when `root` is not inside a Git worktree (or `git` is unavailable). This is
/// the single place the codebase shells `--show-toplevel`; reuse it rather than adding a parallel
/// git call (the ignore matcher anchors its `.gitignore` ancestor stack here — issue #62 finding 3:
/// a `config.root` that is a subdirectory of a larger worktree must honor the worktree-root rules).
pub(crate) fn worktree_root(root: &Path) -> Option<PathBuf> {
    rag_rat_base::repo_discover::discover_repo(root).ok()?.workdir().map(Path::to_path_buf)
}

pub(super) fn git_repo(root: &Path) -> Option<GitRepo> {
    let repo = rag_rat_base::repo_discover::discover_repo(root).ok()?;
    // `workdir()` is `None` for a bare repo — there is no worktree to index, so treat it as
    // "no git" (the previous `--show-toplevel` failed there too).
    let worktree_root = repo.workdir()?.to_path_buf();
    // `head_id()` fails on an unborn HEAD (empty repo) — same as the old `rev-parse HEAD`.
    let head = repo.head_id().ok()?.to_hex().to_string();
    let shallow = repo.is_shallow();
    Some(GitRepo { worktree_root, head, shallow })
}

pub(super) fn is_fast_forward(root: &Path, old_head: &str, new_head: &str) -> bool {
    let probe = || -> anyhow::Result<bool> {
        let mut repo = rag_rat_base::repo_discover::discover_repo(root)?;
        repo.object_cache_size_if_unset(16 * 1024 * 1024);
        let old_id = gix::ObjectId::from_hex(old_head.as_bytes())?;
        let new_id = gix::ObjectId::from_hex(new_head.as_bytes())?;
        let merge_base = repo.merge_base(old_id, new_id)?;
        Ok(merge_base.detach() == old_id)
    };
    probe().unwrap_or(false)
}

/// Canonical serialization of the indexed root for the reload gate. The git-history row set is
/// a function of (HEAD, root) because the `-- .` pathspec runs in `current_dir(root)`, so the
/// gate stores and compares the root alongside the head sha.
pub(super) fn root_key(root: &Path) -> String {
    root.display().to_string()
}

/// O(1) gate for the per-pass git-history reload (`apply_prepared` is a full `git log` re-read +
/// table wipe — O(total history); see its rewrite-safety note). Returns true only when the
/// indexed commit/file-change rows are still valid for the current repo state, so the caller may
/// skip the reload entirely. Conservative: any uncertainty returns false (reload).
///
/// HEAD sha is content-addressed over tree+parents, so any rewrite (squash/rebase/amend/
/// force-pull) moves it and forces a reload. The two cases where HEAD alone is *not* a complete
/// key — and are guarded here — are a shallow clone being deepened (history grows without moving
/// HEAD) and the DB being re-pointed at a different `root` subtree at the same HEAD.
pub(crate) fn is_history_current(conn: &Connection, root: &Path) -> bool {
    let Some(repo) = git_repo(root) else {
        // No git repo (or git failed): let apply_prepared run its clear() path.
        return false;
    };
    if repo.shallow {
        return false;
    }
    let probe = || -> anyhow::Result<bool> {
        let repo_id = schema::active_repo_id(conn)?;
        let Some(cursors) = history_cursors(conn, &repo_id)? else {
            return Ok(false);
        };
        // Guard against a torn/empty prior reload writing the meta without rows — counted per-repo
        // (V040), so a sibling repo's commits can't mask THIS repo's empty history.
        let has_rows = scoped_table_row_count(conn, "git_commits", &repo_id)? > 0;
        Ok(cursors.head == repo.head
            && cursors.root_key == root_key(root)
            && !cursors.shallow
            && has_rows)
    };
    probe().unwrap_or(false)
}
