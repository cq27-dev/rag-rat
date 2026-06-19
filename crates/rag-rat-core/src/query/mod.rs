pub mod clusters;
pub mod graph;
pub mod graph_meta;
pub mod grep_augment;
pub mod impact;
pub mod load_bearing;
pub mod memory;
pub mod orientation;
pub mod pagerank;
pub mod repo_brief;
pub mod symbol;
pub(crate) mod text_compare;
pub mod tree;

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

/// Round a relevance/weight score to 4 decimal places for serialization — enough precision to
/// preserve ranking, without leaking float noise like `0.7071067811865475` into tool output.
pub(crate) fn round_score(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[derive(Debug, Serialize)]
pub struct ReadChunk {
    pub chunk_id: i64,
    pub path: String,
    #[serde(rename = "lang")]
    pub language: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    #[serde(rename = "ref")]
    pub symbol_path: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<graph_meta::GraphEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<memory::RepoMemory>,
}

pub fn read_chunk(conn: &Connection, chunk_id: i64) -> anyhow::Result<Option<ReadChunk>> {
    // One-shot read: load the dict and build a decompressor for this call. Batch readers that loop
    // over many chunk ids (`stale_hit_paths`, eval) should build ONE decompressor and call
    // [`read_chunk_with`] instead — the per-call dict SELECT + dictionary prep is ~20x the
    // decompress itself, so reusing it across a batch is the win (#77 Phase 2 read-path perf).
    let dict = chunk_text_dict(conn)?;
    let mut decompressor = crate::index::text_compression::ChunkDecompressor::new(&dict)?;
    read_chunk_with(conn, chunk_id, &mut decompressor)
}

/// Read one chunk, resolving its text through a caller-owned dict-bound decompressor (reused across
/// a batch). Text comes from the compressed `chunk_text` store (#77 Phase 2); `chunks.text` is the
/// LEFT-JOIN fallback for a chunk with no blob yet (mid-migration / incremental before a dict).
pub(crate) fn read_chunk_with(
    conn: &Connection,
    chunk_id: i64,
    decompressor: &mut crate::index::text_compression::ChunkDecompressor,
) -> anyhow::Result<Option<ReadChunk>> {
    use crate::index::text_compression::ChunkTextRow;
    let row = conn
        .query_row(
            "
            SELECT chunks.id, files.path, files.language, files.kind,
                   chunks.start_line, chunks.end_line, chunks.symbol_path,
                   chunks.text, chunk_text.blob, chunk_text.raw_len
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            LEFT JOIN chunk_text ON chunk_text.chunk_id = chunks.id
            WHERE chunks.id = ?1
            ",
            [chunk_id],
            |row| {
                Ok((
                    ReadChunk {
                        chunk_id: row.get(0)?,
                        path: row.get(1)?,
                        language: row.get(2)?,
                        kind: row.get(3)?,
                        start_line: row.get(4)?,
                        end_line: row.get(5)?,
                        symbol_path: row.get(6)?,
                        text: String::new(),
                        graph: None,
                        memories: Vec::new(),
                    },
                    ChunkTextRow { fallback: row.get(7)?, blob: row.get(8)?, raw_len: row.get(9)? },
                ))
            },
        )
        .optional()?;
    let Some((mut chunk, text_row)) = row else {
        return Ok(None);
    };
    chunk.text = text_row.resolve(decompressor)?;
    Ok(Some(chunk))
}

/// The single shared chunk-text dictionary blob (empty = no-dict sentinel, or absent), read
/// alongside a `chunk_text` blob to decompress it (#77 Phase 2).
pub(crate) fn chunk_text_dict(conn: &Connection) -> anyhow::Result<Vec<u8>> {
    Ok(conn
        .query_row("SELECT dict FROM chunk_text_dict WHERE id = 1", [], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .optional()?
        .unwrap_or_default())
}
