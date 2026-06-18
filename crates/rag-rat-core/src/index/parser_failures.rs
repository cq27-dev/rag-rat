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
        self.storage.connection().execute(
            "INSERT INTO parser_failures(path, language, message) VALUES (?1, ?2, ?3)",
            params![path_string(path), language.as_str(), message],
        )?;
        Ok(())
    }

    pub(super) fn parser_failure_count(&self) -> anyhow::Result<u64> {
        let count = self.storage.connection().query_row(
            "SELECT COUNT(*) FROM parser_failures",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub(super) fn parser_failure_paths(&self) -> anyhow::Result<Vec<ParserFailure>> {
        let mut stmt = self.storage.connection().prepare(
            "SELECT path, language, message FROM parser_failures ORDER BY path, language, message",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ParserFailure { path: row.get(0)?, language: row.get(1)?, message: row.get(2)? })
        })?;
        let mut failures = Vec::new();
        for row in rows {
            failures.push(row?);
        }
        Ok(failures)
    }
}
