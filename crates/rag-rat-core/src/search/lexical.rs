use std::collections::BTreeMap;

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::index::ai;
use crate::query::graph_meta::GraphEvidence;

const BM25_WEIGHT: f64 = 0.45;
const VECTOR_WEIGHT: f64 = 0.35;
const SYMBOL_WEIGHT: f64 = 0.10;
const GRAPH_WEIGHT: f64 = 0.05;
const GIT_WEIGHT: f64 = 0.03;
const GITHUB_WEIGHT: f64 = 0.02;

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub chunk_id: i64,
    pub path: String,
    pub language: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub symbol_path: Option<String>,
    pub score: f64,
    /// Which retrieval modes found this hit: "lexical" (BM25 only), "vector" (embedding cosine
    /// only), or "hybrid" (both). Always present, so an agent knows whether embeddings
    /// contributed without passing explain=true (#41). "lexical" whenever no embedding model is
    /// active.
    pub retrieval_mode: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<GraphEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_components: Option<ScoreComponents>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ScoreComponents {
    pub bm25: f64,
    pub vector: f64,
    pub symbol: f64,
    pub graph: f64,
    pub git: f64,
    pub github: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_note: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub include_git: bool,
    pub include_papertrail: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self { include_git: true, include_papertrail: true }
    }
}

pub fn search(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(
        conn,
        query,
        limit,
        include_generated,
        ai::embed_query(conn, query)?,
        false,
        SearchOptions::default(),
    )
}

pub fn search_hash_baseline(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(
        conn,
        query,
        limit,
        include_generated,
        Some(ai::hash_query_embedding(query)?),
        false,
        SearchOptions::default(),
    )
}

pub fn search_explain(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(
        conn,
        query,
        limit,
        include_generated,
        ai::embed_query(conn, query)?,
        true,
        SearchOptions::default(),
    )
}

/// BM25/FTS-only search for latency-critical callers (the grep-augment hook): bypasses
/// `ai::embed_query`, so it can never trigger an embedding-model load. Also skips git and
/// papertrail boosts — pure lexical + structural rank.
pub fn search_lexical_only(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(conn, query, limit, include_generated, None, false, SearchOptions {
        include_git: false,
        include_papertrail: false,
    })
}

pub fn search_with_options(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
    explain: bool,
    options: SearchOptions,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(
        conn,
        query,
        limit,
        include_generated,
        ai::embed_query(conn, query)?,
        explain,
        options,
    )
}

fn search_with_query_embedding(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
    query_embedding: Option<ai::QueryEmbedding>,
    explain: bool,
    options: SearchOptions,
) -> anyhow::Result<Vec<SearchHit>> {
    let terms = query_terms(query);
    let candidate_limit = i64::from(limit.max(10)).saturating_mul(8);
    let vector_available = query_embedding.is_some();
    let mut ranked = BTreeMap::<i64, RankedHit>::new();

    for (rank, hit) in
        bm25_candidates(conn, query, candidate_limit, include_generated)?.into_iter().enumerate()
    {
        let entry = ranked.entry(hit.chunk_id).or_insert_with(|| RankedHit::new(hit));
        entry.components.bm25 = BM25_WEIGHT * lexical_rank_score(rank);
    }

    for (hit, similarity) in
        vector_candidates(conn, query, candidate_limit, include_generated, query_embedding)?
    {
        let entry = ranked.entry(hit.chunk_id).or_insert_with(|| RankedHit::new(hit));
        entry.components.vector = VECTOR_WEIGHT * f64::from(similarity).clamp(0.0, 1.0);
    }

    let mut hits = ranked
        .into_values()
        .map(|mut hit| {
            let boosts = boosts(conn, &hit.hit, &terms, options)?;
            hit.components.symbol = SYMBOL_WEIGHT * boosts.symbol;
            hit.components.graph = GRAPH_WEIGHT * boosts.graph;
            hit.components.git = GIT_WEIGHT * boosts.git;
            hit.components.github = GITHUB_WEIGHT * boosts.github;
            Ok(hit.finish(explain, vector_available))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(hits)
}

struct RankedHit {
    hit: SearchHit,
    components: ScoreComponents,
}

impl RankedHit {
    fn new(hit: SearchHit) -> Self {
        Self { hit, components: ScoreComponents::default() }
    }

    fn finish(mut self, explain: bool, vector_available: bool) -> SearchHit {
        self.hit.score = crate::query::round_score(
            self.components.bm25
                + self.components.vector
                + self.components.symbol
                + self.components.graph
                + self.components.git
                + self.components.github,
        );
        // Always state how this hit was retrieved (#41): a hit enters `ranked` via the BM25 and/or
        // the vector candidate pass, so its mode follows which of those components scored.
        self.hit.retrieval_mode =
            match (self.components.bm25 > 0.0, self.components.vector > 0.0) {
                (true, true) => "hybrid",
                (false, true) => "vector",
                _ => "lexical",
            }
            .to_string();
        if explain {
            if !vector_available {
                self.components.vector_note =
                    Some("vector search unavailable: no current embedding model".to_string());
            } else if self.components.vector == 0.0 {
                self.components.vector_note =
                    Some("no positive current vector match for this chunk".to_string());
            }
            self.hit.score_components = Some(self.components);
        }
        self.hit
    }
}

fn lexical_rank_score(rank: usize) -> f64 {
    1.0 / ((rank + 1) as f64).sqrt()
}

fn bm25_candidates(
    conn: &Connection,
    query: &str,
    limit: i64,
    include_generated: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    let fts_query = fts_query(query);
    if fts_query == "\"\"" {
        return Ok(Vec::new());
    }
    let generated_filter = if include_generated { "1 = 1" } else { "files.generated = 0" };
    let sql = format!(
        "
        SELECT chunks.id, files.path, files.language, files.kind,
               chunks.start_line, chunks.end_line, chunks.symbol_path,
               bm25(chunk_fts) AS score,
               chunks.text
        FROM chunk_fts
        JOIN chunks ON chunks.id = chunk_fts.rowid
        JOIN files ON files.id = chunks.file_id
        WHERE chunk_fts MATCH ?1
          AND {generated_filter}
        ORDER BY score
        LIMIT ?2
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![fts_query, limit], |row| {
        let text: String = row.get(8)?;
        Ok(SearchHit {
            chunk_id: row.get(0)?,
            path: row.get(1)?,
            language: row.get(2)?,
            kind: row.get(3)?,
            start_line: row.get(4)?,
            end_line: row.get(5)?,
            symbol_path: row.get(6)?,
            score: row.get(7)?,
            // Placeholder — RankedHit::finish sets the real mode from the scored components.
            retrieval_mode: String::new(),
            summary: snippet(&text, query),
            graph: None,
            score_components: None,
        })
    })?;

    collect_rows(rows)
}

fn vector_candidates(
    conn: &Connection,
    query: &str,
    limit: i64,
    include_generated: bool,
    query_embedding: Option<ai::QueryEmbedding>,
) -> anyhow::Result<Vec<(SearchHit, f32)>> {
    let Some(query_embedding) = query_embedding else {
        return Ok(Vec::new());
    };
    let model_version = ai::active_embedding_model_version(conn, &query_embedding.model_id)?;
    let generated_filter = if include_generated { "1 = 1" } else { "files.generated = 0" };
    let sql = format!(
        "
        SELECT chunks.id, files.path, files.language, files.kind,
               chunks.start_line, chunks.end_line, chunks.symbol_path,
               chunks.text, chunk_embeddings.vector_blob
        FROM chunk_embeddings
        JOIN ai_models ON ai_models.model_id = chunk_embeddings.model_id
        JOIN chunks ON chunks.id = chunk_embeddings.chunk_id
        JOIN files ON files.id = chunks.file_id
        WHERE chunk_embeddings.model_id = ?1
          AND ai_models.installed = 1
          AND ai_models.disabled = 0
          AND ai_models.status = 'Ready'
          AND ai_models.embedding_dim = ?2
          AND chunk_embeddings.embedding_dim = ai_models.embedding_dim
          AND chunk_embeddings.status = 'Current'
          AND chunk_embeddings.source_text_hash = chunks.text_hash
          AND chunk_embeddings.model_version = ?3
          AND chunk_embeddings.embedding_text_version = ?4
          AND chunk_embeddings.input_hash != ''
          AND {generated_filter}
        ",
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            query_embedding.model_id,
            i64::try_from(query_embedding.dim).unwrap_or(i64::MAX),
            model_version,
            ai::EMBEDDING_TEXT_VERSION
        ],
        |row| {
            let text: String = row.get(7)?;
            let blob: Vec<u8> = row.get(8)?;
            Ok((
                SearchHit {
                    chunk_id: row.get(0)?,
                    path: row.get(1)?,
                    language: row.get(2)?,
                    kind: row.get(3)?,
                    start_line: row.get(4)?,
                    end_line: row.get(5)?,
                    symbol_path: row.get(6)?,
                    score: 0.0,
                    // Placeholder — RankedHit::finish sets the real mode from the scored
                    // components.
                    retrieval_mode: String::new(),
                    summary: snippet(&text, query),
                    graph: None,
                    score_components: None,
                },
                blob,
            ))
        },
    )?;
    let mut hits = Vec::new();
    for row in rows {
        let (hit, blob) = row?;
        let Some(vector) = ai::decode_vector(&blob, query_embedding.dim) else {
            continue;
        };
        let similarity = dot(&query_embedding.vector, &vector);
        if similarity > 0.0 {
            hits.push((hit, similarity));
        }
    }
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(hits)
}

#[derive(Debug, Clone, Default)]
struct BoostComponents {
    symbol: f64,
    graph: f64,
    git: f64,
    github: f64,
}

fn boosts(
    conn: &Connection,
    hit: &SearchHit,
    terms: &[String],
    options: SearchOptions,
) -> anyhow::Result<BoostComponents> {
    let historical = historical_boost(conn, &hit.path, options)?;
    Ok(BoostComponents {
        symbol: symbol_path_boost(hit, terms),
        graph: graph_boost(conn, hit, terms)?,
        git: historical.git,
        github: historical.github,
    })
}

fn symbol_path_boost(hit: &SearchHit, terms: &[String]) -> f64 {
    let path = hit.path.to_ascii_lowercase();
    let symbol = hit.symbol_path.as_deref().unwrap_or_default().to_ascii_lowercase();
    let mut boost: f64 = 0.0;
    for term in terms {
        if !term.is_empty() && symbol.contains(term) {
            boost += 0.50;
        }
        if !term.is_empty() && path.contains(term) {
            boost += 0.20;
        }
    }
    boost.min(1.0)
}

fn graph_boost(conn: &Connection, hit: &SearchHit, terms: &[String]) -> anyhow::Result<f64> {
    let Some(symbol) = hit.symbol_path.as_deref() else {
        return Ok(0.0);
    };
    let qualified = qualified_symbol_name(symbol);
    // Runs once PER CANDIDATE during ranking (~limit*8 times), so it is the hottest edge query in
    // the search path. After interning (#79) it MUST filter on `from_name_id` / `to_name_id` (the
    // INTEGER columns carrying `idx_edges_from_name` / `idx_edges_to_name`), not on the `edges`
    // view's computed `from_name` / `to_name` values — a value predicate through the view cannot
    // use those indexes, so it degrades to a full `edges_data` scan that joins the dictionary on
    // every row (the query_warm instruction + random-I/O blow-up). The value→id subqueries are
    // constant (≤2 ids each), keeping the index seek; the dictionary joins then reconstruct the
    // display strings only for the index-matched rows.
    let mut stmt = conn.prepare(
        "
        SELECT ek.value, conf.value, fn.value, tn.value
        FROM edges_data d
        JOIN edge_strings ek ON ek.id = d.edge_kind_id
        JOIN edge_strings conf ON conf.id = d.confidence_id
        LEFT JOIN edge_strings fn ON fn.id = d.from_name_id
        JOIN edge_strings tn ON tn.id = d.to_name_id
        WHERE d.from_name_id IN (SELECT id FROM edge_strings WHERE value IN (?1, ?2))
           OR d.to_name_id IN (SELECT id FROM edge_strings WHERE value IN (?1, ?2))
        ORDER BY
            CASE conf.value
                WHEN 'Exact' THEN 0
                WHEN 'Syntactic' THEN 1
                WHEN 'NameOnly' THEN 2
                ELSE 3
            END,
            ek.value
        LIMIT 64
        ",
    )?;
    let rows = stmt.query_map(params![symbol, qualified], |row| {
        Ok(GraphEdgeEvidence {
            edge_kind: row.get(0)?,
            confidence: row.get(1)?,
            from_name: row.get(2)?,
            to_name: row.get(3)?,
        })
    })?;
    let mut strongest: f64 = 0.0;
    let mut secondary: f64 = 0.0;
    for row in rows {
        let edge = row?;
        let Some(other) = edge.other_endpoint(symbol, qualified) else {
            continue;
        };
        let term_weight = if terms.iter().any(|term| !term.is_empty() && other.contains(term)) {
            1.0
        } else {
            0.35
        };
        let evidence =
            confidence_weight(&edge.confidence) * relation_weight(&edge.edge_kind) * term_weight;
        if evidence > strongest {
            secondary += strongest * 0.15;
            strongest = evidence;
        } else {
            secondary += evidence * 0.15;
        }
    }
    Ok((strongest + secondary).min(1.0))
}

#[derive(Debug)]
struct GraphEdgeEvidence {
    edge_kind: String,
    confidence: String,
    from_name: Option<String>,
    to_name: String,
}

impl GraphEdgeEvidence {
    fn other_endpoint(&self, symbol: &str, qualified: &str) -> Option<String> {
        let from_name = self.from_name.as_deref().unwrap_or_default();
        if from_name == symbol || from_name == qualified {
            return Some(self.to_name.to_ascii_lowercase());
        }
        if self.to_name == symbol || self.to_name == qualified {
            return Some(from_name.to_ascii_lowercase());
        }
        None
    }
}

fn qualified_symbol_name(symbol_path: &str) -> &str {
    for marker in [".rs::", ".ts::", ".tsx::", ".kt::", ".kts::"] {
        if let Some(index) = symbol_path.find(marker) {
            return &symbol_path[(index + marker.len())..];
        }
    }
    symbol_path
}

fn confidence_weight(confidence: &str) -> f64 {
    match confidence {
        "Exact" => 1.0,
        "Syntactic" => 0.70,
        "NameOnly" => 0.15,
        "Ambiguous" => 0.0,
        _ => 0.0,
    }
}

fn relation_weight(edge_kind: &str) -> f64 {
    match edge_kind {
        "calls_name" | "constructs" | "uses_macro" => 1.0,
        "imports" | "exports" => 0.60,
        "references_type" | "implements" | "extends" => 0.40,
        "contains" => 0.20,
        _ => 0.0,
    }
}

#[derive(Debug, Clone, Default)]
struct HistoricalBoost {
    git: f64,
    github: f64,
}

fn historical_boost(
    conn: &Connection,
    path: &str,
    options: SearchOptions,
) -> anyhow::Result<HistoricalBoost> {
    let git = if options.include_git {
        conn.query_row(
            "SELECT COUNT(*) FROM git_file_changes WHERE path = ?1 LIMIT 1",
            [path],
            |row| row.get::<_, i64>(0),
        )?
    } else {
        0
    };
    let github = if options.include_papertrail {
        conn.query_row(
            "SELECT COUNT(*) FROM github_refs WHERE source_path = ?1 LIMIT 1",
            [path],
            |row| row.get::<_, i64>(0),
        )?
    } else {
        0
    };
    Ok(HistoricalBoost {
        git: if git > 0 { 1.0 } else { 0.0 },
        github: if github > 0 { 1.0 } else { 0.0 },
    })
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(left, right)| left * right).sum()
}

fn fts_query(query: &str) -> String {
    let terms = query_terms(query)
        .into_iter()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if terms.is_empty() { "\"\"".to_string() } else { terms.join(" OR ") }
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn snippet(text: &str, query: &str) -> String {
    let terms = query_terms(query);
    let lines = text.lines().collect::<Vec<_>>();
    let hit = lines.iter().position(|line| {
        let lower = line.to_ascii_lowercase();
        terms.iter().any(|term| lower.contains(term))
    });
    let start = hit.unwrap_or(0).saturating_sub(1);
    let end = (start + 3).min(lines.len());
    lines[start..end].join("\n")
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::index::schema;

    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('src/watch.rs', 'rust', 'source', 'abc', 0, 0)",
            [],
        )
        .unwrap();
        let chunk_id: i64 = conn
            .query_row(
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                    start_line, end_line, text, text_hash)
                 VALUES (1, 'symbol', 'watcher_main', 0, 10, 1, 20,
                         'fn watcher_main() { /* election retry loop */ }', 'h1')
                 RETURNING id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // Populate the FTS index directly — external-content FTS5 tables need an explicit
        // INSERT on the FTS virtual table to build shadow-table entries (or a 'rebuild', as
        // schema::rebuild_fts now does). A direct INSERT keeps this seed self-contained.
        conn.execute(
            "INSERT INTO chunk_fts(rowid, text)
             VALUES (?1, 'fn watcher_main() { /* election retry loop */ }')",
            [chunk_id],
        )
        .unwrap();
        conn
    }

    /// Regression guard (#79 query_warm): `graph_boost` runs once per candidate (~limit*8), so its
    /// `from_name`/`to_name` filter MUST stay on the `from_name_id`/`to_name_id` INTEGER indexes.
    /// Through the `edges` view a value predicate degrades to a full edges_data scan that joins the
    /// dictionary per row (the 5x blow-up). Pin the plan: the candidate filter uses the int index.
    #[test]
    fn graph_boost_uses_the_name_id_indexes() {
        let conn = seeded_conn();
        let plan = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT ek.value FROM edges_data d
                 JOIN edge_strings ek ON ek.id = d.edge_kind_id
                 WHERE d.from_name_id IN (SELECT id FROM edge_strings WHERE value IN ('a', 'b'))
                    OR d.to_name_id IN (SELECT id FROM edge_strings WHERE value IN ('a', 'b'))",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("idx_edges_from_name") && plan.contains("idx_edges_to_name"),
            "graph_boost candidate filter must use the name_id indexes, got plan:\n{plan}"
        );
        assert!(
            !plan.contains("SCAN d "),
            "graph_boost must not full-scan edges_data, got plan:\n{plan}"
        );
    }

    #[test]
    fn search_lexical_only_returns_bm25_hits_without_embeddings() {
        let conn = seeded_conn();
        let hits = search_lexical_only(&conn, "election retry", 5, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/watch.rs");
        // No model is configured in this DB; reaching here without error proves no embed path ran.
        // retrieval_mode is always present and states the mode without needing explain (#41).
        assert_eq!(hits[0].retrieval_mode, "lexical");
    }

    #[test]
    fn retrieval_mode_is_lexical_when_no_embedding_model() {
        let conn = seeded_conn();
        // The default search path embeds the query, but with no model it falls back to BM25 —
        // every hit must be labeled "lexical", never an empty string or an overclaimed mode.
        let hits = search(&conn, "election retry", 5, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].retrieval_mode, "lexical");
    }
}
