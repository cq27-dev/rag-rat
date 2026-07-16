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
    // One-shot read: load the dict versions and build a decoder for this call. Batch readers that
    // loop over many chunk ids (`stale_hit_paths`, eval) should build ONE decoder and call
    // [`read_chunk_with`] instead — the per-call dict SELECT + dictionary prep is ~20x the
    // decompress itself, so reusing it across a batch is the win (#77 Phase 2 read-path perf).
    let dicts = chunk_text_dicts(conn)?;
    let mut decoder = rag_rat_db::text_compression::ChunkTextDecoder::new(&dicts);
    read_chunk_with(conn, chunk_id, &mut decoder)
}

/// Read one chunk, resolving its text through a caller-owned dict decoder (reused across a batch).
/// Text comes from the compressed `chunk_text` store (#77 Phase 2) — the `chunks.text` column is
/// gone, so this INNER JOINs `chunk_text` (every live chunk has exactly one blob), and each blob is
/// decoded against its own `dict_version`.
pub(crate) fn read_chunk_with(
    conn: &Connection,
    chunk_id: i64,
    decoder: &mut rag_rat_db::text_compression::ChunkTextDecoder,
) -> anyhow::Result<Option<ReadChunk>> {
    use rag_rat_db::text_compression::ChunkTextRow;
    let row = conn
        .query_row(
            "
            SELECT chunks.id, files.path, files.language, files.kind,
                   chunks.start_line, chunks.end_line, chunks.symbol_path,
                   chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            JOIN chunk_text ON chunk_text.chunk_id = chunks.id
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
                    ChunkTextRow {
                        blob: row.get(7)?,
                        raw_len: row.get(8)?,
                        dict_version: row.get(9)?,
                    },
                ))
            },
        )
        .optional()?;
    let Some((mut chunk, text_row)) = row else {
        return Ok(None);
    };
    chunk.text = text_row.resolve(decoder)?;
    Ok(Some(chunk))
}

/// All chunk-text dictionary versions (version → dict bytes), loaded once and reused across a batch
/// via [`rag_rat_db::text_compression::ChunkTextDecoder`]. Each `chunk_text` blob records the
/// version it was compressed against (#77 Phase 2); a dict is an immutable decode key and a retrain
/// adds a version rather than replacing one, so a read may span multiple resident versions. An
/// empty map (fresh DB, no dicts yet) decodes nothing — every chunk has a blob only once a dict
/// exists.
pub(crate) fn chunk_text_dicts(
    conn: &Connection,
) -> anyhow::Result<std::collections::HashMap<i64, Vec<u8>>> {
    let mut stmt = conn.prepare("SELECT version, dict FROM chunk_text_dict")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)))?;
    let mut dicts = std::collections::HashMap::new();
    for row in rows {
        let (version, dict) = row?;
        dicts.insert(version, dict);
    }
    Ok(dicts)
}
