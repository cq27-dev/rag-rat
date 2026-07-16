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
