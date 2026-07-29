//! `/api/chunk/text` composition: current-source validation without healing writes.

use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::index::anchors::{self, AnchorStatus};
use crate::index::{IndexDatabase, IndexError};

#[derive(Debug, Serialize)]
pub struct LensChunkText {
    pub chunk_id: i64,
    pub text: String,
}

impl IndexDatabase {
    /// Validate stored chunk text against current source without mutating the index.
    pub fn lens_chunk_text(&self, chunk_id: i64) -> anyhow::Result<Option<LensChunkText>> {
        let Some(chunk) = rag_rat_query::read_chunk(self.storage.connection(), chunk_id)? else {
            return Ok(None);
        };
        let root = if self.active_scope_is_linked_overlay() {
            crate::index::linked_source_root(
                self.storage.source_root().ok_or_else(|| {
                    anyhow::anyhow!("current source root is unavailable for chunk {chunk_id}")
                })?,
                PathBuf::from(&self.active_worktree_id).as_path(),
            )?
        } else {
            self.storage
                .source_root()
                .ok_or_else(|| {
                    anyhow::anyhow!("current source root is unavailable for chunk {chunk_id}")
                })?
                .to_path_buf()
        };
        let current_text = fs::read_to_string(root.join(&chunk.path))
            .map_err(|_| IndexError::Gone { chunk_id })?;
        let anchor = self.chunk_anchor(chunk_id)?;
        let text = match anchors::validate(
            &chunk.text,
            usize::try_from(chunk.start_line).unwrap_or(1),
            usize::try_from(chunk.end_line).unwrap_or(1),
            &anchor,
            &current_text,
        ) {
            AnchorStatus::Exact => anchors::slice_lines(
                &current_text,
                usize::try_from(chunk.start_line).unwrap_or(1),
                usize::try_from(chunk.end_line).unwrap_or(1),
            )
            .ok_or_else(|| IndexError::StaleChunk { chunk_id, path: chunk.path.clone() })?,
            AnchorStatus::Relocated { text, .. } => text,
            AnchorStatus::Stale => {
                anyhow::bail!(IndexError::StaleChunk { chunk_id, path: chunk.path });
            },
        };
        Ok(Some(LensChunkText { chunk_id: chunk.chunk_id, text }))
    }
}
