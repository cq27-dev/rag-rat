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
    // Text comes from the compressed store (#77 Phase 2); chunks.text is the LEFT-JOIN fallback for
    // a chunk with no chunk_text row yet (mid-migration / incremental before a dict existed).
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
                        text: row.get(7)?,
                        graph: None,
                        memories: Vec::new(),
                    },
                    row.get::<_, Option<Vec<u8>>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                ))
            },
        )
        .optional()?;
    let Some((mut chunk, blob, raw_len)) = row else {
        return Ok(None);
    };
    if let (Some(blob), Some(raw_len)) = (blob, raw_len) {
        let dict = chunk_text_dict(conn)?;
        let bytes =
            crate::index::text_compression::decompress(&blob, &dict, raw_len.max(0) as usize)?;
        chunk.text = String::from_utf8(bytes)?;
    }
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
