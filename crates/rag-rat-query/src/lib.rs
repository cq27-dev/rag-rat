pub mod coupling;
pub mod graph;
pub mod graph_meta;
pub mod impact;
pub mod load_bearing;
pub mod memory;
pub mod pagerank;
pub mod repo_brief;
pub mod symbol;
pub mod text_compare;
pub mod tree;

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

/// Round a relevance/weight score to 4 decimal places for serialization — enough precision to
/// preserve ranking, without leaking float noise like `0.7071067811865475` into tool output.
pub fn round_score(value: f64) -> f64 {
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
    /// Distilled decision records (#705 drive-by) for the symbol this chunk defines — capped ≤2,
    /// labeled unreviewed. Empty for almost every chunk (facet-gated: a provider fix edge + a
    /// qualified symbol anchor). Populated by the core reader, never by the base `read_chunk`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub distilled_records: Vec<rag_rat_papertrail::DriveByRecord>,
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
pub fn read_chunk_with(
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
                        distilled_records: Vec::new(),
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
pub fn chunk_text_dicts(
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

/// Per-signal score breakdown for a hybrid search hit.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScoreComponents {
    pub bm25: f64,
    pub vector: f64,
    pub symbol: f64,
    pub graph: f64,
    pub git: f64,
    pub papertrail: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub chunk_id: i64,
    pub path: String,
    #[serde(rename = "lang")]
    pub language: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    #[serde(rename = "ref")]
    pub symbol_path: Option<String>,
    pub score: f64,
    /// Which retrieval modes found this hit: "lexical" (BM25 only), "vector" (embedding cosine
    /// only), or "hybrid" (both). Always present, so an agent knows whether embeddings
    /// contributed without passing explain=true (#41). "lexical" whenever no embedding model is
    /// active.
    pub retrieval_mode: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<graph_meta::GraphEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_components: Option<ScoreComponents>,
    /// LOCAL structural-load signal (scoped weighted fan-in) for the hit's symbol — the THIRD
    /// importance scale, NOT PageRank. Attached by the search/`symbol_lookup` enrichment pass over
    /// the symbol a hit resolves to (`chunks.symbol_path` → the active-scope symbol). `None` when
    /// the hit has no symbol, the symbol has no in-edges in scope, or it wasn't enriched. See
    /// `crate::load_bearing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<crate::load_bearing::ImportanceEnrichment>,
    /// Distilled decision records (#705 drive-by) for the symbol this hit resolves to — capped ≤2,
    /// labeled unreviewed. Empty for almost every hit (facet-gated: a provider fix edge + a
    /// qualified symbol anchor). Attached by the search enrichment pass, never by the base search.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub distilled_records: Vec<rag_rat_papertrail::DriveByRecord>,
}
