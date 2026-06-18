//! File-row reads and scope mutations: fetch the file row, mark/remove files in the active scope,
//! and count indexed files.

use super::*;

impl IndexDatabase {
    pub(super) fn mark_file_deleted(&self, path: &Path) -> anyhow::Result<()> {
        self.write_tombstone_in_scope(path, &self.active_worktree_id)
    }

    /// Write a `kind='deleted'` overlay tombstone for `path` in an EXPLICIT `worktree_id` scope
    /// (not necessarily the active one). The scope view excludes such a row from the overlay
    /// branch AND (because the committed branch's `path NOT IN (overlay paths)` subquery still
    /// counts it) suppresses the base committed row — so the path is HIDDEN rather than falling
    /// through to the base. That is exactly what a linked worktree's branch-deleted file needs
    /// (#219); `mark_file_deleted` is the active-scope special case.
    pub(super) fn write_tombstone_in_scope(
        &self,
        path: &Path,
        worktree_id: &str,
    ) -> anyhow::Result<()> {
        let path = path_string(path);
        self.remove_file_in_scope(Path::new(&path), "", worktree_id)?;
        self.storage.connection().execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, \
             indexed_at_ms, indexed_revision, commit_sha, worktree_id)
             VALUES (?1, 'unknown', 'deleted', '', 0, 0, ?2, '', '', ?3)
             ON CONFLICT(path, commit_sha, worktree_id) DO UPDATE SET
                kind = 'deleted',
                sha256 = '',
                modified_at_ms = 0,
                indexed_at_ms = excluded.indexed_at_ms",
            params![path, now_ms(), worktree_id],
        )?;
        self.mark_fts_dirty()?;
        Ok(())
    }

    pub(super) fn remove_file_in_scope(
        &self,
        path: &Path,
        commit_sha: &str,
        worktree_id: &str,
    ) -> anyhow::Result<()> {
        let path = path_string(path);
        // Direct edges_data writes (#79): these statements touch up to every in-edge of a file's
        // symbols, so they must not pay the view triggers' per-row dictionary probes.
        // 'NameOnly' is the EdgeConfidence demotion the resolver applies to a target-less edge.
        let name_only_id = edges::intern_edge_string(self.storage.connection(), "NameOnly")?;
        self.storage.connection().execute(
            "UPDATE edges_data
             SET to_symbol_id = NULL,
                 confidence_id = ?4
             WHERE to_symbol_id IN (
                 SELECT symbols.id FROM symbols
                 JOIN main.files ON main.files.id = symbols.file_id
                 WHERE main.files.path = ?1
                   AND main.files.commit_sha = ?2
                   AND main.files.worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id, name_only_id],
        )?;
        self.storage.connection().execute(
            "DELETE FROM edges_data
             WHERE source_file_id IN (
                    SELECT id FROM main.files
                    WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3
                )
                OR from_symbol_id IN (
                    SELECT symbols.id FROM symbols
                    JOIN main.files ON main.files.id = symbols.file_id
                    WHERE main.files.path = ?1
                      AND main.files.commit_sha = ?2
                      AND main.files.worktree_id = ?3
                )",
            params![path, commit_sha, worktree_id],
        )?;
        // `parser_failures` is keyed by `path` only (no scope). A LINKED-WORKTREE OVERLAY pass must
        // NOT delete by bare path: that would clear a REAL parse failure recorded for the same path
        // by the base (or a sibling) scope. The overlay never WRITES this table either (see
        // `insert_parser_failure`), so it has nothing of its own to remove (#219 review).
        if !self.active_scope_is_linked_overlay() {
            self.storage
                .connection()
                .execute("DELETE FROM parser_failures WHERE path = ?1", [&path])?;
        }
        self.storage.connection().execute(
            "DELETE FROM chunk_fts
             WHERE rowid IN (
                 SELECT chunks.id FROM chunks
                 JOIN main.files ON main.files.id = chunks.file_id
                 WHERE main.files.path = ?1
                   AND main.files.commit_sha = ?2
                   AND main.files.worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id],
        )?;
        // Deleting the chunks cascades (ON DELETE CASCADE, foreign_keys=ON) to git_chunk_blame,
        // chunk_embeddings, and chunk_summaries — so the gate skipping the full git-history wipe
        // does NOT leak blame. (`docs` has no FK and is not cleaned here — a pre-existing gap,
        // tracked separately.)
        self.storage.connection().execute(
            "DELETE FROM chunks
             WHERE file_id IN (
                SELECT id FROM main.files
                WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id],
        )?;
        self.storage.connection().execute(
            "DELETE FROM symbols
             WHERE file_id IN (
                SELECT id FROM main.files
                WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id],
        )?;
        self.storage.connection().execute(
            "DELETE FROM main.files WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3",
            params![path, commit_sha, worktree_id],
        )?;
        self.mark_fts_dirty()?;
        Ok(())
    }

    pub(super) fn file_row(&self, path: &Path) -> anyhow::Result<FileRow> {
        self.storage
            .connection()
            .query_row(
                "SELECT language, kind FROM files WHERE path = ?1",
                [path_string(path)],
                |row| {
                    let language: String = row.get(0)?;
                    let kind: String = row.get(1)?;
                    Ok((language, kind))
                },
            )
            .map_err(Into::into)
            .and_then(|(language, kind)| {
                Ok(FileRow { language: language.parse()?, kind: kind.parse()? })
            })
    }

    pub(super) fn indexed_files(&self) -> anyhow::Result<Vec<IndexedFile>> {
        let mut stmt =
            self.storage.connection().prepare("SELECT path, sha256 FROM files ORDER BY path")?;
        let rows =
            stmt.query_map([], |row| Ok(IndexedFile { path: row.get(0)?, sha256: row.get(1)? }))?;
        let mut files = Vec::new();
        for row in rows {
            files.push(row?);
        }
        Ok(files)
    }

    /// Re-derive `files.generated` from the current [`is_generated_path`] heuristic (the single
    /// source of truth) for every file whose stored flag disagrees, gated on
    /// [`GENERATED_FLAGS_VERSION`] so it runs once per definition change. Needed because
    /// incremental discovery rewrites a file row only on sha/language/kind change — when the
    /// *meaning* of the flag changes (#202) the inputs are identical, so nothing would refresh
    /// it. Idempotent. Runs only on a write-bearing open (read-only opens see the stale version
    /// and fall back).
    pub(super) fn ensure_generated_flags_current(&self) -> anyhow::Result<()> {
        if self.meta(GENERATED_FLAGS_VERSION_KEY)?.as_deref() == Some(GENERATED_FLAGS_VERSION) {
            return Ok(());
        }
        self.storage.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.rederive_generated_flags();
        if result.is_err() {
            let _ = self.storage.execute_batch("ROLLBACK");
            result?;
        }
        self.set_meta(GENERATED_FLAGS_VERSION_KEY, GENERATED_FLAGS_VERSION)?;
        self.storage.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Stamp the generated-flags version current. Called after a full rebuild / incremental pass,
    /// which already write correct flags via `file_is_generated`, so the next open skips the
    /// re-derive.
    pub(super) fn mark_generated_flags_current(&self) -> anyhow::Result<()> {
        self.set_meta(GENERATED_FLAGS_VERSION_KEY, GENERATED_FLAGS_VERSION)
    }

    fn rederive_generated_flags(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        // The generated flag is a property of the file PATH (+ target kind), not the active scope,
        // so re-derive over the base `main.files` for every row — NOT the per-connection `files`
        // scope view (a non-updatable UNION; #89).
        let rows: Vec<(i64, String, String)> = {
            let mut stmt = conn.prepare("SELECT id, path, kind FROM main.files")?;
            let mapped = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?;
            mapped.collect::<rusqlite::Result<_>>()?
        };
        // Mirror `file_is_generated` without parsing `kind` (skips the `deleted`/`unknown` markers
        // cleanly): explicit generated target OR the path heuristic.
        let mut update = conn.prepare_cached(
            "UPDATE main.files SET generated = ?2 WHERE id = ?1 AND generated != ?2",
        )?;
        for (id, path, kind) in rows {
            let generated = kind == TargetKind::Generated.as_str() || is_generated_path(&path);
            update.execute(params![id, generated])?;
        }
        Ok(())
    }

    pub(super) fn indexed_file_count(&self) -> anyhow::Result<usize> {
        let count =
            self.storage
                .connection()
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }
}
