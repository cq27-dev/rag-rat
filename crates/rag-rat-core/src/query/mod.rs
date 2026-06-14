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
    pub language: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub symbol_path: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<graph_meta::GraphEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<memory::RepoMemory>,
}

pub fn read_chunk(conn: &Connection, chunk_id: i64) -> anyhow::Result<Option<ReadChunk>> {
    Ok(conn
        .query_row(
            "
            SELECT chunks.id, files.path, files.language, files.kind,
                   chunks.start_line, chunks.end_line, chunks.symbol_path, chunks.text
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            WHERE chunks.id = ?1
            ",
            [chunk_id],
            |row| {
                Ok(ReadChunk {
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
                })
            },
        )
        .optional()?)
}
