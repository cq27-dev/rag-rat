use std::path::Path;

use rag_rat_base::language::Language;
use rusqlite::{Connection, params};

use super::{ScoreComponents, SearchHit, SearchOptions, history};

pub(super) struct RankedHit {
    pub(super) hit: SearchHit,
    pub(super) components: ScoreComponents,
}

impl RankedHit {
    pub(super) fn new(hit: SearchHit) -> Self {
        Self { hit, components: ScoreComponents::default() }
    }

    pub(super) fn finish(mut self, explain: bool, vector_available: bool) -> SearchHit {
        self.hit.score = rag_rat_query::round_score(
            self.components.bm25
                + self.components.vector
                + self.components.symbol
                + self.components.graph
                + self.components.git
                + self.components.papertrail,
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

pub(super) fn lexical_rank_score(rank: usize) -> f64 {
    1.0 / ((rank + 1) as f64).sqrt()
}

#[derive(Debug, Clone, Default)]
pub(super) struct BoostComponents {
    pub(super) symbol: f64,
    pub(super) graph: f64,
    pub(super) git: f64,
    pub(super) papertrail: f64,
}

pub(super) fn boosts(
    conn: &Connection,
    hit: &SearchHit,
    terms: &[String],
    options: SearchOptions,
    repo_id: &str,
) -> anyhow::Result<BoostComponents> {
    let historical = history::historical_boost(conn, &hit.path, options, repo_id)?;
    Ok(BoostComponents {
        symbol: symbol_path_boost(hit, terms),
        graph: graph_boost(conn, hit, terms, repo_id)?,
        git: historical.git,
        papertrail: historical.papertrail,
    })
}

pub(super) fn symbol_path_boost(hit: &SearchHit, terms: &[String]) -> f64 {
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

pub(super) fn graph_boost(
    conn: &Connection,
    hit: &SearchHit,
    terms: &[String],
    repo_id: &str,
) -> anyhow::Result<f64> {
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
    // prepare_cached: this runs once per search CANDIDATE (~limit*8); caching collapses ~80
    // cold statement compilations of this multi-join query to one per connection (#79 cold-query
    // prepare cost).
    // REPO SCOPING (A3): the name-id predicate matches by symbol NAME, which is not repo-unique — a
    // sibling repo's identically-named symbol would otherwise inject its edges into this hit's
    // graph score. Constrain each edge to the active repo through its `source_file_id →
    // files.repo_id` (the same repo predicate the candidate/git reads carry). `source_file_id`
    // is always set (its FK is `ON DELETE CASCADE`, and every edge is inserted with its file
    // id), so the `EXISTS` drops no live edge in a single-repo DB. It applies AFTER the
    // `from_name_id`/`to_name_id` index seek narrows to ≤64 candidate edges, so the hot-path
    // integer indexes still drive the scan.
    //
    // A6 SURVIVOR (deliberately NOT generation-scoped, re-justified in the P2 sweep): this is a
    // RANKING boost, not an exactness counter — a superseded generation's edges can inflate a
    // confidence pick until gc, shifting ordering marginally before self-correcting, while the
    // candidate set itself flows through the generation-scoped view. Re-evaluate only if this
    // ever feeds a count a caller treats as exact.
    let mut stmt = conn.prepare_cached(
        "
        SELECT ek.value, conf.value, fn.value, tn.value
        FROM edges_data d
        JOIN name_strings ek ON ek.id = d.edge_kind_id
        JOIN name_strings conf ON conf.id = d.confidence_id
        LEFT JOIN name_strings fn ON fn.id = d.from_name_id
        JOIN name_strings tn ON tn.id = d.to_name_id
        WHERE (d.from_name_id IN (SELECT id FROM name_strings WHERE value IN (?1, ?2))
            OR d.to_name_id IN (SELECT id FROM name_strings WHERE value IN (?1, ?2)))
          -- Materialized visibility (#734): excludes suppressed candidates and internal
          -- dispatch FACT rows in one integer compare (the predicate the edges view enforces).
          AND d.hidden = 0
          AND EXISTS (SELECT 1 FROM main.files f
                       WHERE f.id = d.source_file_id AND f.repo_id = ?3)
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
    let rows = stmt.query_map(params![symbol, qualified, repo_id], |row| {
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
pub(super) struct GraphEdgeEvidence {
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

/// The bare qualified name (`Type::method`) inside an indexed symbol path
/// (`src/thing.rs::Type::method`) — the form graph edges store as an endpoint name, so
/// `graph_boost` can match a search hit against its incoming/outgoing edges.
///
/// The file/name boundary is found by testing each `::` against the LANGUAGE REGISTRY rather than a
/// hardcoded extension list. A literal list silently skips whichever language nobody remembered to
/// add — it was missing `.swift`, `.py`, `.c`, and `.cpp`, so hits in those languages kept the
/// whole path as their name, matched no edge endpoint, and got zero graph boost. Deriving the
/// boundary from [`Language`] means a language registered later is covered without touching this
/// function.
pub(super) fn qualified_symbol_name(symbol_path: &str) -> &str {
    let mut cursor = 0;
    while let Some(offset) = symbol_path[cursor..].find("::") {
        let split = cursor + offset;
        if Language::from_path(Path::new(&symbol_path[..split])).is_some() {
            return &symbol_path[split + "::".len()..];
        }
        cursor = split + "::".len();
    }
    symbol_path
}

pub(super) fn confidence_weight(confidence: &str) -> f64 {
    match confidence {
        "Exact" => 1.0,
        "Syntactic" => 0.70,
        "NameOnly" => 0.15,
        "Ambiguous" => 0.0,
        _ => 0.0,
    }
}

pub(super) fn relation_weight(edge_kind: &str) -> f64 {
    match edge_kind {
        "calls_name" | "constructs" | "uses_operator" | "uses_precedence_group" | "uses_macro" =>
            1.0,
        "imports" | "exports" => 0.60,
        "references_type" | "implements" | "extends" => 0.40,
        "contains" => 0.20,
        _ => 0.0,
    }
}

/// The generated/test demotion flags for one candidate chunk (graded-git rerank, #109).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Demotion {
    pub(super) generated: bool,
    /// Effective test flag: `symbols.is_test` (V035) when the chunk resolves to a symbol, else
    /// `files.has_test_code` (V024). Both are precomputed columns.
    pub(super) is_test: bool,
}

/// Per-chunk generated/test demotion flags keyed by candidate chunk_id, computed in ONE batched
/// query over the whole candidate pool (same per-pool, not per-candidate, discipline as
/// [`graded_git_scores`]). `files.generated` / `files.has_test_code` are precomputed file columns;
/// `symbols.is_test` is resolved through the chunk's qualified `symbol_path` (matching
/// `qualified_name_id` in the same file) and takes precedence when present.
pub(super) fn demotion_flags(
    conn: &Connection,
    chunk_ids: &[i64],
) -> anyhow::Result<std::collections::HashMap<i64, Demotion>> {
    let mut flags = std::collections::HashMap::new();
    if chunk_ids.is_empty() {
        return Ok(flags);
    }
    let placeholders = std::iter::repeat_n("?", chunk_ids.len()).collect::<Vec<_>>().join(", ");
    // LEFT JOIN symbols on the chunk's qualified symbol_path within the same file (the interned
    // qualified_name reconstructed via name_strings, as in query_api/importance.rs). MAX(is_test)
    // over any matched symbol; COALESCE to files.has_test_code when no symbol resolves.
    let sql = format!(
        "
        SELECT chunks.id,
               files.generated,
               COALESCE(MAX(symbols.is_test), files.has_test_code) AS is_test
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        LEFT JOIN symbols
          ON symbols.file_id = chunks.file_id
         AND chunks.symbol_path IS NOT NULL
         AND symbols.qualified_name_id =
             (SELECT id FROM name_strings WHERE value = chunks.symbol_path)
        WHERE chunks.id IN ({placeholders})
        GROUP BY chunks.id
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let params = chunk_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect::<Vec<_>>();
    let rows = stmt.query_map(params.as_slice(), |row| {
        let chunk_id: i64 = row.get(0)?;
        let generated: i64 = row.get(1)?;
        let is_test: i64 = row.get(2)?;
        Ok((chunk_id, Demotion { generated: generated != 0, is_test: is_test != 0 }))
    })?;
    for row in rows {
        let (chunk_id, demotion) = row?;
        flags.insert(chunk_id, demotion);
    }
    Ok(flags)
}

pub(super) fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(left, right)| left * right).sum()
}
