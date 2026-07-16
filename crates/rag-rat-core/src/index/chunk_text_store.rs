//! `IndexDatabase` surface over the db layer's chunk-text store.

use rag_rat_db::chunk_text_store::*;

use super::*;

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
    conn: &rusqlite::Connection,
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
    let blob = rag_rat_db::text_compression::ChunkCompressor::new(&dict)
        .and_then(|mut c| c.compress(text.as_bytes()))
        .expect("compress is infallible for a valid dict");
    conn.execute(
        "INSERT INTO chunk_text(chunk_id, blob, raw_len, dict_version) VALUES (?1, ?2, ?3, ?4)",
        params![chunk_id, blob, i64::try_from(text.len()).unwrap_or(i64::MAX), version],
    )
    .map(|_| ())
}
