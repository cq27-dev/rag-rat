//! Parser-failure bookkeeping: record a failed parse and report failure counts/paths.

use rag_rat_base::paths::path_string;

use super::*;

#[derive(Debug, Serialize)]
pub struct ParserFailure {
    pub path: String,
    pub language: String,
    pub message: String,
}

impl IndexDatabase {
    pub(super) fn insert_parser_failure(
        &self,
        path: &Path,
        language: Language,
        message: &str,
    ) -> anyhow::Result<()> {
        // `parser_failures` is keyed by `path` ONLY (no scope) and every reader counts it globally
        // (coverage, repo_brief, the orientation digest). A LINKED-WORKTREE OVERLAY pass routes its
        // files through this same write path, so a branch-only syntax error would be reported in
        // the BASE and sibling scopes — and `remove_file_in_scope`'s bare-path delete would
        // clear a real failure from another scope. The overlay is not the table's owner;
        // skip the write under an overlay scope and leave the global table to the base pass
        // (accepted recall: a branch-only parse failure is not surfaced in that branch's
        // coverage) (#219 review).
        if self.active_scope_is_linked_overlay() {
            return Ok(());
        }
        // `INSERT OR REPLACE`: the PK is now `(repo_id, path)` (V040), so a re-record of the same
        // path (e.g. without an intervening `remove_file_in_scope`) upserts rather than tripping
        // the PK. `repo_id` scopes the row so a sibling repo's failure at the same path is
        // a distinct row.
        self.storage.connection().execute(
            "INSERT OR REPLACE INTO parser_failures(repo_id, path, language, message)
             VALUES (?1, ?2, ?3, ?4)",
            params![self.active_repo_id, path_string(path), language.as_str(), message],
        )?;
        Ok(())
    }

    /// Clear a path's failure record on a CLEAN (re)parse — the write-time half of the table's
    /// maintenance contract (A6): `parser_failures` is path-keyed, generation-less indexer state
    /// OWNED at (re)parse time, never by the generation-dead gc sweep (which would delete a LIVE
    /// path's record while sweeping a superseded generation's row at the same path). The full
    /// rebuild reaches this per visited file; incremental passes additionally clear via
    /// `remove_file_in_scope`. Overlay-gated like [`Self::insert_parser_failure`] — an overlay pass
    /// must not clear the base scope's real failure.
    pub(super) fn clear_parser_failure(&self, path: &Path) -> anyhow::Result<()> {
        if self.active_scope_is_linked_overlay() {
            return Ok(());
        }
        self.storage.connection().execute(
            "DELETE FROM parser_failures WHERE repo_id = ?1 AND path = ?2",
            params![self.active_repo_id, path_string(path)],
        )?;
        Ok(())
    }

    /// STAGE a full-rebuild pass's parser-failure mutation for `path` instead of writing the
    /// generation-less table mid-wave (A6, P2 review): the waves commit BEFORE the flip, so a
    /// direct upsert/clear would expose the UNPUBLISHED generation's failure state to readers
    /// still scoped to the old generation — and a tail failure would leave it torn off from the
    /// never-flipped pointer. `message = None` records a CLEAN parse (clear at publish); `Some`
    /// records the failure text. Staged rows live in the rebuild connection's
    /// `temp.rebuild_parser_failures` (created by `index_targets_with_progress`) and are applied
    /// atomically with the pointer by [`Self::apply_staged_parser_failures`]. Last write per path
    /// wins (path-PK upsert), matching the direct path's INSERT OR REPLACE.
    pub(super) fn stage_parser_failure(
        &self,
        path: &Path,
        language: Language,
        message: Option<&str>,
    ) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "INSERT OR REPLACE INTO temp.rebuild_parser_failures(path, language, message)
             VALUES (?1, ?2, ?3)",
            params![path_string(path), language.as_str(), message],
        )?;
        Ok(())
    }

    /// Publish the staged parser-failure state for `target` — the finalize half of the table's
    /// maintenance contract (A6). TWO callers (batch 7): the full rebuild runs it inside the
    /// terminal flip transaction (after the carry-forward), atomic with the pointer, so readers
    /// see the OLD failure state until the flip, then exactly the published generation's; the
    /// standalone `index_targets` runs it in its own finalize transaction at the connection's
    /// LIVE generation (no flip exists there — its failure state publishes atomically with its
    /// own edges instead). Three steps:
    ///
    /// 1. Apply staged UPSERTS — paths that failed this pass: parse errors (which also have a file
    ///    row) and PREPARE failures (unreadable / invalid UTF-8 — which never earn one).
    /// 2. Apply staged CLEARS — paths that parsed cleanly this pass.
    /// 3. Orphan sweep — drop records for paths the rebuilt BASE scope no longer contains: a path
    ///    REMOVED from the tree is never visited, so no staged row covers it and its stale record
    ///    would otherwise linger forever (the generation-dead gc sweep deliberately never touches
    ///    this table). Presence is judged against the BASE scope at `target` (`commit_sha` = the
    ///    rebuilt HEAD or `worktree_id` = the base checkout) — NOT any row at `target`: the
    ///    carry-forward drags other-commit leftovers and overlays onto `target`, and this table is
    ///    base-owned (`insert_parser_failure` never writes under an overlay). Staged-UPSERTED paths
    ///    are EXCLUDED (P2 review): a PREPARE-failed file has no file row in ANY generation, so a
    ///    file-row presence test alone would sweep the failure recorded moments earlier — the
    ///    staged set is the authoritative "visited and failed" list and shields exactly those.
    pub(super) fn apply_staged_parser_failures(&self, target: i64) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        conn.execute(
            "INSERT OR REPLACE INTO parser_failures(repo_id, path, language, message)
             SELECT ?1, path, language, message
             FROM temp.rebuild_parser_failures WHERE message IS NOT NULL",
            params![self.active_repo_id],
        )?;
        conn.execute(
            "DELETE FROM parser_failures
             WHERE repo_id = ?1
               AND path IN (
                   SELECT path FROM temp.rebuild_parser_failures WHERE message IS NULL
               )",
            params![self.active_repo_id],
        )?;
        conn.execute(
            "DELETE FROM parser_failures
             WHERE repo_id = ?1
               AND path NOT IN (
                   SELECT path FROM temp.rebuild_parser_failures WHERE message IS NOT NULL
               )
               AND path NOT IN (
                   SELECT path FROM main.files
                   WHERE repo_id = ?1 AND generation = ?2
                     AND (commit_sha = ?3 OR worktree_id = ?4)
               )",
            params![self.active_repo_id, target, self.active_commit_sha, self.active_worktree_id],
        )?;
        conn.execute_batch("DELETE FROM temp.rebuild_parser_failures;")?;
        Ok(())
    }

    pub(super) fn parser_failure_count(&self) -> anyhow::Result<u64> {
        // Scoped to `active_repo_id` (A3): the row is written under a `repo_id`, so an unscoped
        // count would report a sibling repo's parse failures as this repo's coverage in a
        // consolidated DB.
        let count = self.storage.connection().query_row(
            "SELECT COUNT(*) FROM parser_failures WHERE repo_id = ?1",
            [&self.active_repo_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub(super) fn parser_failure_paths(&self) -> anyhow::Result<Vec<ParserFailure>> {
        // Scoped to `active_repo_id` (A3): a same-path healthy file in this repo must not be
        // reported parser-failed on the strength of a sibling repo's failure at that path.
        let mut stmt = self.storage.connection().prepare(
            "SELECT path, language, message FROM parser_failures WHERE repo_id = ?1
             ORDER BY path, language, message",
        )?;
        let rows = stmt.query_map([&self.active_repo_id], |row| {
            Ok(ParserFailure { path: row.get(0)?, language: row.get(1)?, message: row.get(2)? })
        })?;
        let mut failures = Vec::new();
        for row in rows {
            failures.push(row?);
        }
        Ok(failures)
    }
}
