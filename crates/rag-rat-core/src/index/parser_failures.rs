//! Parser-failure bookkeeping: record a failed parse and report failure counts/paths.

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
