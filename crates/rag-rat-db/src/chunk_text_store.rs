//! Build the compressed `chunk_text` store (#77 Phase 2): train the shared dictionary on a sample
//! of chunk text and compress every chunk into `chunk_text`. During the transition the source of
//! truth is still `chunks.text`; this derives the compressed store from it. Idempotent.

use rusqlite::{Connection, OptionalExtension, params};

use crate::text_compression;

/// Target chunk-text samples for dictionary training. zstd wants samples >> dict size; a few
/// thousand covers the corpus and bounds the train cost.
const DICT_SAMPLE_TARGET: i64 = 4096;

/// The latest (highest-version) dict, or `None` when no dict exists yet. The dict is an immutable
/// decode key, so writes compress against the CURRENT version and a future retrain adds a new one
/// (see the `chunk_text_dict` schema comment); "latest" is what new blobs reference.
pub fn latest_dict(conn: &Connection) -> anyhow::Result<Option<(i64, Vec<u8>)>> {
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
pub fn build_store(conn: &Connection, source: &str) -> anyhow::Result<()> {
    let total: i64 =
        conn.query_row(&format!("SELECT COUNT(*) FROM {source}"), [], |row| row.get(0))?;
    // NB: do NOT early-return on total==0. A full rebuild that indexes files but produces ZERO
    // chunks (e.g. a whitespace-only markdown file) must still establish version 1 — otherwise the
    // index is left dict-less with files present, and the next incremental/heal hits insert_chunks'
    // "no dict" branch, which stages into the rebuild-only `temp.rebuild_chunk_text` and either
    // orphans the chunk (same connection) or errors "no such table" (fresh connection). An empty
    // corpus trains an empty (no-dict) dict; later chunks compress against it inline.
    let (version, dict) = match latest_dict(conn)? {
        Some(existing) => existing,
        None => {
            // First index: train version 1. Sample evenly across the id space (first-N would bias
            // to one corner of the repo). Fall back to an EMPTY dict (no-dict plain zstd) when the
            // corpus is too small to train on (`from_samples` hard-errors under ~7 samples; an
            // empty source yields zero samples → empty dict, which is the no-dict sentinel).
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
