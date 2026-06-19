//! Build the compressed `chunk_text` store (#77 Phase 2): train the shared dictionary on a sample
//! of chunk text and compress every chunk into `chunk_text`. During the transition the source of
//! truth is still `chunks.text`; this derives the compressed store from it. Idempotent.

use rusqlite::Connection;

use super::*;

/// Target chunk-text samples for dictionary training. zstd wants samples >> dict size; a few
/// thousand covers the corpus and bounds the train cost.
const DICT_SAMPLE_TARGET: i64 = 4096;

/// The latest (highest-version) dict, or `None` when no dict exists yet. The dict is an immutable
/// decode key, so writes compress against the CURRENT version and a future retrain adds a new one
/// (see the `chunk_text_dict` schema comment); "latest" is what new blobs reference.
pub(crate) fn latest_dict(conn: &Connection) -> anyhow::Result<Option<(i64, Vec<u8>)>> {
    Ok(conn
        .query_row(
            "SELECT version, dict FROM chunk_text_dict ORDER BY version DESC LIMIT 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?)
}

/// Build the compressed `chunk_text` store from a `(chunk_id, text)` `source` —
/// `temp.rebuild_chunk_text` during a full rebuild, or `(SELECT id AS chunk_id, text FROM chunks)`
/// in the V027 migration that retires the `chunks.text` column. `source` is an internal constant,
/// never user input. If no dict exists yet, trains version 1 from a sample of the source; otherwise
/// compresses against the existing latest version (the dict is immutable — never retrained in
/// place, so other contexts' blobs stay decodable). Does NOT clear `chunk_text`/`chunk_text_dict`:
/// the staged context's old rows were already cascade-deleted with its chunks; this inserts the
/// staged chunks' blobs only. Runs INSIDE the caller's transaction so a freshly-trained dict and
/// its blobs commit atomically.
pub(crate) fn build_store(conn: &Connection, source: &str) -> anyhow::Result<()> {
    let total: i64 =
        conn.query_row(&format!("SELECT COUNT(*) FROM {source}"), [], |row| row.get(0))?;
    if total == 0 {
        return Ok(());
    }
    let (version, dict) = match latest_dict(conn)? {
        Some(existing) => existing,
        None => {
            // First index: train version 1. Sample evenly across the id space (first-N would bias
            // to one corner of the repo). Fall back to an EMPTY dict (no-dict plain zstd) when the
            // corpus is too small to train on (`from_samples` hard-errors under ~7 samples).
            let stride = (total / DICT_SAMPLE_TARGET).max(1);
            let samples: Vec<Vec<u8>> = {
                let mut stmt = conn.prepare(&format!(
                    "SELECT text FROM {source} WHERE chunk_id % ?1 = 0 LIMIT ?2"
                ))?;
                let rows = stmt.query_map(params![stride, DICT_SAMPLE_TARGET], |row| {
                    Ok(row.get::<_, String>(0)?.into_bytes())
                })?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let dict = text_compression::train_dict(&samples, text_compression::DEFAULT_DICT_SIZE)
                .unwrap_or_default();
            conn.execute("INSERT INTO chunk_text_dict(version, dict) VALUES (1, ?1)", params![
                dict
            ])?;
            (1, dict)
        },
    };
    // Compress every staged chunk against the chosen dict version, reusing one compressor.
    let mut compressor = text_compression::ChunkCompressor::new(&dict)?;
    let mut read = conn.prepare(&format!("SELECT chunk_id, text FROM {source}"))?;
    let mut insert = conn.prepare(
        "INSERT INTO chunk_text(chunk_id, blob, raw_len, dict_version) VALUES (?1, ?2, ?3, ?4)",
    )?;
    let mut rows = read.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let text: String = row.get(1)?;
        let blob = compressor.compress(text.as_bytes())?;
        insert.execute(params![
            id,
            blob,
            i64::try_from(text.len()).unwrap_or(i64::MAX),
            version
        ])?;
    }
    Ok(())
}

impl IndexDatabase {
    /// Full-rebuild entry: build the store from the rebuild's `temp.rebuild_chunk_text` staging
    /// table (insert_chunks wrote the in-memory text there for the first index, when no dict
    /// exists), then clear the staging. A no-op when a dict already exists (insert_chunks
    /// compressed inline). See [`build_store`].
    pub(crate) fn build_chunk_text_store(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        build_store(conn, "temp.rebuild_chunk_text")?;
        conn.execute("DELETE FROM temp.rebuild_chunk_text", [])?;
        Ok(())
    }

    /// The latest dict version + its bytes, or `None` when no dict exists yet (a fresh DB mid first
    /// rebuild before the dict is trained). The incremental/heal write path compresses new chunks
    /// inline against this version and records it on the `chunk_text` row; absent → the path stages
    /// the text instead (the first full rebuild's bulk pass trains the dict).
    pub(crate) fn latest_chunk_text_dict(&self) -> anyhow::Result<Option<(i64, Vec<u8>)>> {
        latest_dict(self.storage.connection())
    }
}

/// Test seeder: chunks now have no `text` column, so tests that insert a chunk row directly must
/// also seed its compressed `chunk_text` blob (readers INNER JOIN `chunk_text`). Compresses against
/// the EXISTING latest dict version if one exists (e.g. a prior rebuild trained version 1) so the
/// blob is decodable; otherwise creates version 1 with the empty-dict (no-dict, plain zstd)
/// sentinel. Tagging an empty-dict blob with a trained version would make it undecodable.
#[cfg(test)]
pub(crate) fn seed_chunk_text(
    conn: &Connection,
    chunk_id: i64,
    text: &str,
) -> rusqlite::Result<()> {
    let (version, dict) = match latest_dict(conn).expect("read latest dict") {
        Some(existing) => existing,
        None => {
            conn.execute("INSERT INTO chunk_text_dict(version, dict) VALUES (1, x'')", [])?;
            (1, Vec::new())
        },
    };
    let blob = text_compression::ChunkCompressor::new(&dict)
        .and_then(|mut c| c.compress(text.as_bytes()))
        .expect("compress is infallible for a valid dict");
    conn.execute(
        "INSERT INTO chunk_text(chunk_id, blob, raw_len, dict_version) VALUES (?1, ?2, ?3, ?4)",
        params![chunk_id, blob, i64::try_from(text.len()).unwrap_or(i64::MAX), version],
    )
    .map(|_| ())
}
