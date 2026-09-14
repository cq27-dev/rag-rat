//! Chunk reads with live anchor validation and graph and memory context.

use super::*;

impl IndexDatabase {
    pub fn read_chunk(&self, chunk_id: i64) -> anyhow::Result<Option<rag_rat_query::ReadChunk>> {
        // Internal/CLI/test entry — always the FULL memory bodies; the surface-aware path is the
        // MCP `read_chunk` tool, which calls `read_chunk_with_graph_and_memories` with the
        // config surface.
        self.read_chunk_with_graph_and_memories(
            chunk_id,
            GraphMetaMode::Full,
            20,
            true,
            rag_rat_base::config::MemorySurface::Full,
        )
    }

    pub fn read_chunk_with_graph(
        &self,
        chunk_id: i64,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
    ) -> anyhow::Result<Option<rag_rat_query::ReadChunk>> {
        // `include_memories = false`, so the surface never applies — pass `Full`.
        self.read_chunk_with_graph_and_memories(
            chunk_id,
            graph_mode,
            graph_limit,
            false,
            rag_rat_base::config::MemorySurface::Full,
        )
    }

    pub fn read_chunk_with_graph_and_memories(
        &self,
        chunk_id: i64,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
        include_memories: bool,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<Option<rag_rat_query::ReadChunk>> {
        let Some(mut chunk) = self.read_chunk_current(chunk_id)? else {
            return Ok(None);
        };
        graph_meta::attach_to_read_chunk(
            self.storage.connection(),
            &mut chunk,
            graph_mode,
            graph_limit,
        )?;
        if include_memories {
            let conn = self.storage.connection();
            // Drive-by chunk attachments honor `[memory] surface`: under `Summary` each memory's
            // body is deferred to `memory show`, leaving the summary + verdict marker
            // (title-only fallback). #582: the Summary hydration runs a RANKED chunk_fts query —
            // heal-and-retry.
            chunk.memories = crate::index::retry_once_on_fts_corruption(
                || {
                    let mut memories = rag_rat_query::memory::memories_for_chunk(
                        conn,
                        chunk_id,
                        DRIVE_BY_CHUNK_MEMORY_LIMIT,
                    )?;
                    rag_rat_query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                    Ok(memories)
                },
                || self.heal_corrupt_fts(),
            )?;
            // Distilled decision records (#705 drive-by) for the symbol this chunk defines, capped
            // ≤2 and labeled unreviewed. Rides the memories flag; facet-gated, so empty for almost
            // every chunk — matches the symbol_lookup convention (a lightweight per-item surface).
            chunk.distilled_records = self.records_for_chunk_symbol(chunk_id, 2)?;
        }
        Ok(Some(chunk))
    }

    pub(crate) fn read_chunk_current(
        &self,
        chunk_id: i64,
    ) -> anyhow::Result<Option<rag_rat_query::ReadChunk>> {
        let dicts = rag_rat_query::chunk_text_dicts(self.storage.connection())?;
        let mut decoder = rag_rat_db::text_compression::ChunkTextDecoder::new(&dicts);
        self.read_chunk_current_with(chunk_id, &mut decoder)
    }

    /// Live-revalidating chunk read that resolves text through a caller-owned dict decoder (reused
    /// across a batch) rather than reloading the dict versions per call.
    pub(crate) fn read_chunk_current_with(
        &self,
        chunk_id: i64,
        decoder: &mut rag_rat_db::text_compression::ChunkTextDecoder,
    ) -> anyhow::Result<Option<rag_rat_query::ReadChunk>> {
        let Some(mut chunk) =
            rag_rat_query::read_chunk_with(self.storage.connection(), chunk_id, decoder)?
        else {
            return Ok(None);
        };
        // Under a LINKED-WORKTREE OVERLAY scope, `source_root` is the MAIN checkout — NOT the
        // branch the chunk came from. Live-revalidating against main would slice the chunk
        // text out of main's copy of the file (returning BASE text for a branch chunk
        // whenever the anchor still matches), or call the overlay-guarded `heal_file`
        // no-op. The overlay rows are maintained by `index_worktree_overlay` (read from the
        // linked checkout), so the STORED text is already the branch's — return it as-is
        // and skip live revalidation (#219 review). The base/main scope keeps full live
        // revalidation below.
        if self.active_scope_is_linked_overlay() {
            return Ok(Some(chunk));
        }
        let Some(root) = self.storage.source_root() else {
            return Ok(Some(chunk));
        };
        let source_path = root.join(&chunk.path);
        let current_text = match fs::read_to_string(&source_path) {
            Ok(text) => text,
            Err(_) => {
                let path = chunk.path.clone();
                // #767 review: the gated variant — a stale-scope read path must not stamp a
                // `kind='deleted'` row for a repo `rag-rat rm` already purged.
                self.mark_file_deleted_if_not_removed(Path::new(&path))?;
                self.sync_fts()?;
                anyhow::bail!(IndexError::Gone { chunk_id });
            },
        };
        let anchor = self.chunk_anchor(chunk_id)?;
        let status = anchors::validate(
            &chunk.text,
            usize::try_from(chunk.start_line).unwrap_or(1),
            usize::try_from(chunk.end_line).unwrap_or(1),
            &anchor,
            &current_text,
        );
        match status {
            AnchorStatus::Exact => {
                if let Some(text) = anchors::slice_lines(
                    &current_text,
                    usize::try_from(chunk.start_line).unwrap_or(1),
                    usize::try_from(chunk.end_line).unwrap_or(1),
                ) {
                    chunk.text = text;
                }
                Ok(Some(chunk))
            },
            AnchorStatus::Relocated { start_line, end_line, text } => {
                chunk.start_line = i64::try_from(start_line)?;
                chunk.end_line = i64::try_from(end_line)?;
                chunk.text = text;
                Ok(Some(chunk))
            },
            AnchorStatus::Stale => {
                self.heal_file(Path::new(&chunk.path))?;
                self.sync_fts()?;
                let healed = rag_rat_query::read_chunk(self.storage.connection(), chunk_id)?;
                match healed {
                    Some(chunk) => Ok(Some(chunk)),
                    None => anyhow::bail!(IndexError::StaleChunk { chunk_id, path: chunk.path }),
                }
            },
        }
    }
}
