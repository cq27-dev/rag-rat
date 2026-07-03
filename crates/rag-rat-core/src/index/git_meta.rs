//! Git history/GitHub status persistence and freshness for the active checkout.

use super::*;

impl IndexDatabase {
    /// Refresh `git_commit` / `git_dirty` meta, writing only the keys that actually changed.
    /// Returns whether either was written (so the caller can tell a no-op pass from a real one).
    pub(super) fn write_git_meta(&self, root: &Path) -> anyhow::Result<bool> {
        let commit = head_sha(root);
        let dirty = is_worktree_dirty(root);
        let commit_changed = self.set_repo_meta_if_changed("git_commit", &commit)?;
        let dirty_changed =
            self.set_repo_meta_if_changed("git_dirty", if dirty { "true" } else { "false" })?;
        Ok(commit_changed || dirty_changed)
    }

    /// O(1) check of whether the indexed git history is still current for `root`'s HEAD, so the
    /// incremental path can skip the full `git log` re-read + table wipe. See
    /// [`git_history::is_history_current`].
    pub(super) fn git_history_is_current(&self, root: &Path) -> bool {
        git_history::is_history_current(self.storage.connection(), root)
    }

    pub(super) fn apply_prepared_git_history(
        &self,
        root: &Path,
        handle: JoinHandle<anyhow::Result<git_history::PreparedGitHistory>>,
    ) -> anyhow::Result<GitHistoryIndexStatus> {
        let prepared = join_git_history_prepare(handle)?;
        git_history::apply_prepared(self.storage.connection(), root, prepared)
    }

    /// [`Self::apply_prepared_git_history`] minus the reload-gate cursor write — the
    /// generation-staged rebuild's rows-inert/cursors-last seam (A6, batch-4 P2): the bulky
    /// `git_commits`/`git_file_changes` rows land in Phase 2 (keyed, inert data — no reader
    /// treats row presence alone as authority), while the returned cursors are recorded inside
    /// the terminal flip transaction via [`Self::record_git_history_cursors`], so "what commit
    /// is this index at" flips atomically with the file generation.
    pub(super) fn apply_prepared_git_history_deferring_cursors(
        &self,
        root: &Path,
        handle: JoinHandle<anyhow::Result<git_history::PreparedGitHistory>>,
    ) -> anyhow::Result<Option<git_history::HistoryCursors>> {
        let prepared = join_git_history_prepare(handle)?;
        let (_status, cursors) = git_history::apply_prepared_deferring_cursors(
            self.storage.connection(),
            root,
            prepared,
        )?;
        Ok(cursors)
    }

    /// Record deferred history reload-gate cursors (see
    /// [`Self::apply_prepared_git_history_deferring_cursors`]).
    pub(super) fn record_git_history_cursors(
        &self,
        cursors: &git_history::HistoryCursors,
    ) -> anyhow::Result<()> {
        git_history::record_history_cursors(self.storage.connection(), cursors)
    }

    pub(super) fn git_history_status(&self) -> anyhow::Result<GitHistoryIndexStatus> {
        let Some(root) = self.storage.source_root() else {
            return git_history::status(self.storage.connection(), Path::new("."));
        };
        git_history::status(self.storage.connection(), root)
    }

    pub(super) fn github_status(&self) -> anyhow::Result<GitHubStatus> {
        github::status(self.storage.connection(), &self.github)
    }
}
