//! Build the compressed `chunk_text` store (#77 Phase 2): train the shared dictionary on a sample
//! of chunk text and compress every chunk into `chunk_text`. During the transition the source of
//! truth is still `chunks.text`; this derives the compressed store from it. Idempotent.

use super::*;

/// Target chunk-text samples for dictionary training. zstd wants samples >> dict size; a few
/// thousand covers the corpus and bounds the train cost.
const DICT_SAMPLE_TARGET: i64 = 4096;

impl IndexDatabase {
    /// Train the shared dictionary on a sample of chunk text and compress every chunk into
    /// `chunk_text` (#77 Phase 2). Idempotent — clears + rebuilds the store + the single dict row.
    /// Runs INSIDE the caller's transaction (the full rebuild's BEGIN..COMMIT); it does not open
    /// its own, so the dict row and the blobs compressed against it commit atomically — never a
    /// mixed-dict state where a stale blob can't be decompressed.
    pub(crate) fn build_chunk_text_store(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        conn.execute("DELETE FROM chunk_text", [])?;
        conn.execute("DELETE FROM chunk_text_dict", [])?;
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
        if total == 0 {
            return Ok(());
        }
        // Sample evenly across the id space (first-N would bias to one corner of the repo).
        let stride = (total / DICT_SAMPLE_TARGET).max(1);
        let samples: Vec<Vec<u8>> = {
            let mut stmt = conn.prepare("SELECT text FROM chunks WHERE id % ?1 = 0 LIMIT ?2")?;
            let rows = stmt.query_map(params![stride, DICT_SAMPLE_TARGET], |row| {
                Ok(row.get::<_, String>(0)?.into_bytes())
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        // Train; fall back to an EMPTY dict (no-dict plain zstd) when the corpus is too small to
        // train on (`from_samples` hard-errors under ~7 samples) — keeps tiny-repo indexing
        // working.
        let dict = text_compression::train_dict(&samples, text_compression::DEFAULT_DICT_SIZE)
            .unwrap_or_default();
        conn.execute(
            "INSERT INTO chunk_text_dict(id, dict, dict_version) VALUES (1, ?1, 1)",
            params![dict],
        )?;
        // Compress every chunk, reusing one dictionary-bound compressor across all rows.
        let mut compressor = text_compression::ChunkCompressor::new(&dict)?;
        let mut read = conn.prepare("SELECT id, text FROM chunks")?;
        let mut insert =
            conn.prepare("INSERT INTO chunk_text(chunk_id, blob, raw_len) VALUES (?1, ?2, ?3)")?;
        let mut rows = read.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let text: String = row.get(1)?;
            let blob = compressor.compress(text.as_bytes())?;
            insert.execute(params![id, blob, i64::try_from(text.len()).unwrap_or(i64::MAX)])?;
        }
        Ok(())
    }
}
