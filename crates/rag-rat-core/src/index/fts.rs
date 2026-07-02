//! Full-text search (FTS5) index lifecycle: rebuild/sync the FTS mirror and track its freshness.

use super::*;

impl IndexDatabase {
    /// Recovery / freshness rebuild: repopulate the contentless `chunk_fts` from the durable text
    /// store and rebuild the external-content `commit_fts`, then record freshness. The hot
    /// full-rebuild path uses [`finalize_full_rebuild_fts`] instead (chunk_fts is written inline).
    pub fn rebuild_fts(&self) -> anyhow::Result<()> {
        self.rebuild_chunk_fts()?;
        schema::rebuild_commit_fts(self.storage.connection())?;
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_repo_meta("fts_dirty", "false")?;
        Ok(())
    }

    /// Full-rebuild FTS finalize: `chunk_fts` was written inline during chunk
    /// insert, so only the external-content `commit_fts` needs the bulk 'rebuild' here. Records
    /// freshness like [`rebuild_fts`], without re-tokenizing every chunk.
    pub(super) fn finalize_full_rebuild_fts(&self) -> anyhow::Result<()> {
        schema::rebuild_commit_fts(self.storage.connection())?;
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_repo_meta("fts_dirty", "false")?;
        Ok(())
    }

    /// Repopulate the contentless `chunk_fts` from scratch (#77 Phase 2): clear it, then
    /// re-tokenize every chunk's text. The text comes from the compressed `chunk_text` store
    /// (decompressed with one reused decompressor); the `chunks.text` column is gone, so this INNER
    /// JOINs `chunk_text`. This is the recovery path only — the full-rebuild and incremental paths
    /// write `chunk_fts` inline from the in-memory chunk text, so this never runs there.
    ///
    /// COST: unlike the old external-content `'rebuild'` (which re-tokenized in-engine from the
    /// content column), this decompresses the WHOLE store in Rust. It is gated by
    /// `ensure_fts_fresh` (only fires when the FTS is genuinely dirty/stale — the same trigger
    /// as before), so a steady-state query never pays it; but a stale-read heal is now a
    /// heavier one-off.
    fn rebuild_chunk_fts(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        // 'delete-all' is the FTS5 command to clear a contentless index (a bare DELETE is
        // unsupported; on an external-content table it also corrupts a desynced index — #51).
        conn.execute("INSERT INTO chunk_fts(chunk_fts) VALUES('delete-all')", [])?;
        let dicts = crate::query::chunk_text_dicts(conn)?;
        let mut decoder = crate::index::text_compression::ChunkTextDecoder::new(&dicts);
        // Collect (rowid, ChunkTextRow) first: decompress's anyhow::Result can't cross the rusqlite
        // closure, and the SELECT statement can't stay open while we INSERT into chunk_fts.
        let rows: Vec<(i64, crate::index::text_compression::ChunkTextRow)> = {
            let mut stmt = conn.prepare(
                "SELECT chunks.id, chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version
                 FROM chunks JOIN chunk_text ON chunk_text.chunk_id = chunks.id",
            )?;
            let mapped = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, crate::index::text_compression::ChunkTextRow {
                    blob: row.get(1)?,
                    raw_len: row.get(2)?,
                    dict_version: row.get(3)?,
                }))
            })?;
            let mut out = Vec::new();
            for row in mapped {
                out.push(row?);
            }
            out
        };
        let mut insert = conn.prepare("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)")?;
        for (chunk_id, text_row) in rows {
            let text = text_row.resolve(&mut decoder)?;
            insert.execute(rusqlite::params![chunk_id, text])?;
        }
        Ok(())
    }

    pub fn sync_fts(&self) -> anyhow::Result<()> {
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_repo_meta("fts_dirty", "false")?;
        Ok(())
    }

    fn record_fts_current(&self) -> anyhow::Result<()> {
        self.set_repo_meta("fts_synced_at_ms", &now_ms().to_string())?;
        let revision = self.content_revision()?;
        self.set_repo_meta("fts_source_revision", &revision)?;
        Ok(())
    }

    pub(super) fn mark_fts_dirty(&self) -> anyhow::Result<()> {
        self.set_repo_meta("fts_dirty", "true")
    }

    pub(super) fn ensure_fts_fresh(&self) -> anyhow::Result<()> {
        let content_revision = self.content_revision()?;
        let fts_source_revision = self.repo_meta("fts_source_revision")?;
        if !self.fts_dirty()? && fts_source_revision.as_deref() == Some(content_revision.as_str()) {
            return Ok(());
        }
        self.rebuild_fts()?;
        let refreshed_revision = self.repo_meta("fts_source_revision")?;
        if refreshed_revision.as_deref() != Some(content_revision.as_str()) {
            anyhow::bail!(
                "FTS freshness invariant failed: content_revision={content_revision}, \
                 fts_source_revision={}",
                refreshed_revision.unwrap_or_else(|| "<missing>".to_string())
            );
        }
        Ok(())
    }

    pub(super) fn fts_dirty(&self) -> anyhow::Result<bool> {
        Ok(self.repo_meta("fts_dirty")?.as_deref() == Some("true"))
    }
}
