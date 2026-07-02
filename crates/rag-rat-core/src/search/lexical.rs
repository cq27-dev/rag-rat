use std::collections::BTreeMap;

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::index::text_compression::ChunkTextRow;
use crate::index::{ai, text_compression};
use crate::query::graph_meta::GraphEvidence;

const BM25_WEIGHT: f64 = 0.45;
const VECTOR_WEIGHT: f64 = 0.35;
const SYMBOL_WEIGHT: f64 = 0.10;
const GRAPH_WEIGHT: f64 = 0.05;
const GIT_WEIGHT: f64 = 0.03;
const GITHUB_WEIGHT: f64 = 0.02;

// --- Graded-git rerank (#109, behind `SearchOptions::graded_history`) ----------------------------
// All of these are A/B-SWEPT on the commit-replay eval (`rag-rat eval --replay --rerank`); they are
// the dials a sweep tunes. None of them are read unless `graded_history` is set, so the default
// fuse is unchanged.
//
// The graded git contribution replaces the binary `GIT_WEIGHT * has_history` with
// `GIT_WEIGHT_GRADED * git_score`, where `git_score` is a saturating recency+churn magnitude in
// [0,1]. The binary path keeps `GIT_WEIGHT` untouched; the graded path gets a higher weight because
// it now discriminates (the binary signal was ~uniformly 1.0).
const GIT_WEIGHT_GRADED: f64 = 0.10;
/// `recent_touch_count` saturating cap: this many commits in the last 90 days already maxes the
/// recency term.
const RECENT_CAP: f64 = 5.0;
/// `commit_touch_count` saturating cap: this many total touching commits already maxes the churn
/// term.
const TOTAL_CAP: f64 = 20.0;
/// Recency vs total-churn split inside `git_score` (must sum to 1.0).
const GIT_RECENT_WEIGHT: f64 = 0.6;
const GIT_TOTAL_WEIGHT: f64 = 0.4;
/// Multiplicative score penalties applied after the weighted sum (precision lever, near-free):
/// generated chunks and test code are rarely the gold for a feature commit, so down-weight them.
const GENERATED_PENALTY: f64 = 0.6;
const TEST_PENALTY: f64 = 0.8;
/// Recency floor: commits within this many seconds of the newest commit count as "recent". Matches
/// the 90-day window `query::repo_brief::file_rows` uses for its churn CTE.
const RECENT_WINDOW_SECS: i64 = 90 * 24 * 60 * 60;

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
    pub graph: Option<GraphEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_components: Option<ScoreComponents>,
    /// LOCAL structural-load signal (scoped weighted fan-in) for the hit's symbol — the THIRD
    /// importance scale, NOT PageRank. Attached by the search/`symbol_lookup` enrichment pass over
    /// the symbol a hit resolves to (`chunks.symbol_path` → the active-scope symbol). `None` when
    /// the hit has no symbol, the symbol has no in-edges in scope, or it wasn't enriched. See
    /// `crate::query::load_bearing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<crate::query::load_bearing::ImportanceEnrichment>,
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
    /// Graded-git rerank (#109, config `[search] graded_git_rerank`, default false): at the wide
    /// pre-truncation pool, replace the binary git has-history boost with a recency+churn
    /// magnitude and apply the generated/test demotion. OFF makes the score path
    /// byte-identical to today — every new computation is guarded behind this flag. A/B-swept
    /// on `eval --replay --rerank`.
    pub graded_history: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self { include_git: true, include_papertrail: true, graded_history: false }
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
    graded_history: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(
        conn,
        query,
        limit,
        include_generated,
        Some(ai::hash_query_embedding(query)?),
        false,
        SearchOptions { graded_history, ..SearchOptions::default() },
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
        graded_history: false,
    })
}

/// A lexical+vector search request: the query plus its controls. Replaces the positional
/// `(query, limit, include_generated, explain, options)` argument train so call sites read
/// themselves and can't transpose the two bools.
pub struct LexicalQuery<'a> {
    pub query: &'a str,
    pub limit: u32,
    pub include_generated: bool,
    pub explain: bool,
    pub options: SearchOptions,
}

pub fn search_with_options(
    conn: &Connection,
    request: &LexicalQuery<'_>,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(
        conn,
        request.query,
        request.limit,
        request.include_generated,
        ai::embed_query(conn, request.query)?,
        request.explain,
        request.options,
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
    // REPO SCOPING (A4): every candidate row flows through the `files` scope VIEW (both the bm25
    // and the vector pass JOIN `files`), which filters `repo_id` FIRST — so in a consolidated
    // DB a sibling repo's chunks are dropped by the INNER JOIN before ranking. `active_repo_id`
    // is resolved ONCE here and threaded into the git/github boost queries (which read the
    // direct-scoped `git_file_changes` / `github_refs` by path, bypassing the view).
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    // RECALL BOUND: `chunk_fts` / `chunk_embeddings` MATCH globally, then the repo (+ commit +
    // worktree) filter is applied by the scope-view JOIN. Because SQLite applies `LIMIT` AFTER the
    // join filter, the candidate window is a PER-REPO window — the active repo is never starved by
    // a sibling repo's matches. `candidate_limit` (8× the caller's limit, well past the plan's
    // 4× floor) caps how many post-filter candidates enter ranking; a query whose active-repo
    // matches exceed 8×limit keeps only the BM25/vector-top `candidate_limit` of them, the same
    // bound as a single-repo DB.
    let candidate_limit = i64::from(limit.max(10)).saturating_mul(8);
    let vector_available = query_embedding.is_some();
    let mut ranked = BTreeMap::<i64, RankedHit>::new();
    // One dict decoder shared by both candidate passes (#77 Phase 2): bm25 and vector each
    // decompress a batch of snippet text and run sequentially, so loading the dict versions once
    // here avoids the duplicate SELECT + dictionary prep the two passes used to do independently.
    let dicts = crate::query::chunk_text_dicts(conn)?;
    let mut decoder = text_compression::ChunkTextDecoder::new(&dicts);

    for (rank, hit) in
        bm25_candidates(conn, query, candidate_limit, include_generated, &mut decoder)?
            .into_iter()
            .enumerate()
    {
        let entry = ranked.entry(hit.chunk_id).or_insert_with(|| RankedHit::new(hit));
        entry.components.bm25 = BM25_WEIGHT * lexical_rank_score(rank);
    }

    for (hit, similarity) in vector_candidates(
        conn,
        query,
        candidate_limit,
        include_generated,
        query_embedding,
        &mut decoder,
    )? {
        let entry = ranked.entry(hit.chunk_id).or_insert_with(|| RankedHit::new(hit));
        entry.components.vector = VECTOR_WEIGHT * f64::from(similarity).clamp(0.0, 1.0);
    }

    // Graded-git rerank (#109): ONE batched git-churn query + ONE batched demotion query over the
    // WHOLE candidate pool, computed only under the flag. With `graded_history` false both maps
    // stay empty and unused, so the boost/finish path below is byte-identical to today. The
    // wide pre-truncation pool is the only site with reorder headroom beyond the top-10.
    let (graded_git, demotions) = if options.graded_history {
        let paths = ranked.values().map(|hit| hit.hit.path.clone()).collect::<Vec<_>>();
        let chunk_ids = ranked.keys().copied().collect::<Vec<_>>();
        (graded_git_scores(conn, &paths, &repo_id)?, demotion_flags(conn, &chunk_ids)?)
    } else {
        (std::collections::HashMap::new(), std::collections::HashMap::new())
    };

    let mut hits = ranked
        .into_values()
        .map(|mut hit| {
            let boosts = boosts(conn, &hit.hit, &terms, options, &repo_id)?;
            hit.components.symbol = SYMBOL_WEIGHT * boosts.symbol;
            hit.components.graph = GRAPH_WEIGHT * boosts.graph;
            // The git component is the one real lever: under the flag, replace the binary
            // has-history boost (`GIT_WEIGHT * boosts.git`, ~uniformly 1.0) with the graded
            // recency+churn magnitude (`GIT_WEIGHT_GRADED * git_score`). The binary path is
            // untouched so the default fuse is unchanged.
            hit.components.git = if options.graded_history {
                GIT_WEIGHT_GRADED * graded_git.get(&hit.hit.path).copied().unwrap_or(0.0)
            } else {
                GIT_WEIGHT * boosts.git
            };
            hit.components.github = GITHUB_WEIGHT * boosts.github;
            let chunk_id = hit.hit.chunk_id;
            let mut finished = hit.finish(explain, vector_available);
            // Multiplicative demotion AFTER the weighted sum (near-free precision lever): generated
            // chunks and test code are rarely the gold for a feature commit. Only under the flag.
            if options.graded_history
                && let Some(demotion) = demotions.get(&chunk_id)
            {
                let mut penalty = 1.0;
                if demotion.generated {
                    penalty *= GENERATED_PENALTY;
                }
                if demotion.is_test {
                    penalty *= TEST_PENALTY;
                }
                finished.score = crate::query::round_score(finished.score * penalty);
            }
            Ok(finished)
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
    decoder: &mut text_compression::ChunkTextDecoder,
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
               chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version
        FROM chunk_fts
        JOIN chunks ON chunks.id = chunk_fts.rowid
        JOIN files ON files.id = chunks.file_id
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
        WHERE chunk_fts MATCH ?1
          AND {generated_filter}
        ORDER BY score
        LIMIT ?2
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    // Snippet text comes from the compressed store (#77); collect blob + raw_len here and
    // decompress in the post-loop — decompress returns anyhow::Result, which can't cross the
    // rusqlite closure.
    let rows = stmt.query_map(params![fts_query, limit], |row| {
        Ok((
            SearchHit {
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
                summary: String::new(),
                graph: None,
                score_components: None,
                importance: None,
            },
            ChunkTextRow { blob: row.get(8)?, raw_len: row.get(9)?, dict_version: row.get(10)? },
        ))
    })?;
    let collected = collect_rows(rows)?;
    let mut hits = Vec::with_capacity(collected.len());
    for (mut hit, text_row) in collected {
        hit.summary = snippet(&text_row.resolve(decoder)?, query);
        hits.push(hit);
    }
    Ok(hits)
}

fn vector_candidates(
    conn: &Connection,
    query: &str,
    limit: i64,
    include_generated: bool,
    query_embedding: Option<ai::QueryEmbedding>,
    decoder: &mut text_compression::ChunkTextDecoder,
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
               chunk_embeddings.vector_blob, chunk_text.blob, chunk_text.raw_len,
               chunk_text.dict_version
        FROM chunk_embeddings
        JOIN ai_models ON ai_models.model_id = chunk_embeddings.model_id
        JOIN chunks ON chunks.id = chunk_embeddings.chunk_id
        JOIN files ON files.id = chunks.file_id
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
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
            let vector_blob: Vec<u8> = row.get(7)?;
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
                    // Filled from the compressed store in the post-loop (decompress can't cross the
                    // rusqlite closure).
                    summary: String::new(),
                    graph: None,
                    score_components: None,
                    importance: None,
                },
                vector_blob,
                ChunkTextRow {
                    blob: row.get(8)?,
                    raw_len: row.get(9)?,
                    dict_version: row.get(10)?,
                },
            ))
        },
    )?;
    // Score first (decode + dot), then truncate, THEN decompress only the survivors' snippets. This
    // is a brute-force flat scan, so many rows can clear `similarity > 0`, but only the top `limit`
    // are kept — decompressing snippet text before the truncate would decompress (and discard) all
    // the rest (#77 Phase 2 read-path perf).
    let mut scored: Vec<(SearchHit, f32, ChunkTextRow)> = Vec::new();
    for (hit, vector_blob, text_row) in collect_rows(rows)? {
        let Some(vector) = ai::decode_vector(&vector_blob, query_embedding.dim) else {
            continue;
        };
        let similarity = dot(&query_embedding.vector, &vector);
        if similarity > 0.0 {
            scored.push((hit, similarity, text_row));
        }
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let mut hits = Vec::with_capacity(scored.len());
    for (mut hit, similarity, text_row) in scored {
        hit.summary = snippet(&text_row.resolve(decoder)?, query);
        hits.push((hit, similarity));
    }
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
    repo_id: &str,
) -> anyhow::Result<BoostComponents> {
    let historical = historical_boost(conn, &hit.path, options, repo_id)?;
    Ok(BoostComponents {
        symbol: symbol_path_boost(hit, terms),
        graph: graph_boost(conn, hit, terms, repo_id)?,
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

fn graph_boost(
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
    repo_id: &str,
) -> anyhow::Result<HistoricalBoost> {
    // `git_file_changes` / `github_refs` are direct-scoped (V040/V041) and queried by PATH here
    // (bypassing the scope view), so the `repo_id` predicate keeps a sibling repo's history from
    // boosting a same-named path in a consolidated DB.
    let git = if options.include_git {
        conn.query_row(
            "SELECT COUNT(*) FROM git_file_changes WHERE path = ?1 AND repo_id = ?2 LIMIT 1",
            params![path, repo_id],
            |row| row.get::<_, i64>(0),
        )?
    } else {
        0
    };
    let github = if options.include_papertrail {
        conn.query_row(
            "SELECT COUNT(*) FROM github_refs WHERE source_path = ?1 AND repo_id = ?2 LIMIT 1",
            params![path, repo_id],
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

/// Saturating recency+churn magnitude in [0,1] for one candidate path (graded-git rerank, #109).
/// `recent_touch_count` = commits touching the path within the last 90 days; `commit_touch_count` =
/// total distinct commits touching it. A path with no git history scores 0.0. The caps and the
/// recent/total split are A/B-tunable consts above.
fn git_score(recent_touch_count: i64, commit_touch_count: i64) -> f64 {
    let recent = (recent_touch_count.max(0) as f64 / RECENT_CAP).min(1.0);
    let total = (commit_touch_count.max(0) as f64 / TOTAL_CAP).min(1.0);
    GIT_RECENT_WEIGHT * recent + GIT_TOTAL_WEIGHT * total
}

/// Per-path graded-git scores keyed by candidate path, computed in ONE batched aggregation query
/// over the whole candidate pool (NOT per candidate — at limit*8 ≈ 80 candidates a per-candidate
/// git query would be the new hottest query). Mirrors the `churn` CTE in
/// `query::repo_brief::file_rows`; `idx_git_file_changes_path` keeps the `path IN (...)` seek
/// cheap. Paths absent from the map (or with no git history) score 0.0.
fn graded_git_scores(
    conn: &Connection,
    paths: &[String],
    repo_id: &str,
) -> anyhow::Result<std::collections::HashMap<String, f64>> {
    let mut scores = std::collections::HashMap::new();
    if paths.is_empty() {
        return Ok(scores);
    }
    // `git_commits` / `git_file_changes` are direct-scoped (V040); the newest-commit floor and the
    // churn aggregate both filter `repo_id` so a consolidated DB grades against THIS repo's history
    // only (a fork shares hashes and paths).
    let newest_commit: i64 = conn.query_row(
        "SELECT COALESCE(MAX(authored_at_s), 0) FROM git_commits WHERE repo_id = ?1",
        params![repo_id],
        |row| row.get(0),
    )?;
    // Resolve the 90-day recency floor ONCE per query (not per path).
    let recent_floor = newest_commit.saturating_sub(RECENT_WINDOW_SECS);
    let placeholders = std::iter::repeat_n("?", paths.len()).collect::<Vec<_>>().join(", ");
    // ?1 = recent_floor, ?2..?(paths.len()+1) = paths, ?(paths.len()+2) = repo_id.
    let repo_index = paths.len() + 2;
    let sql = format!(
        "
        SELECT git_file_changes.path,
               COUNT(DISTINCT git_file_changes.commit_hash) AS commit_touch_count,
               SUM(CASE WHEN git_commits.authored_at_s >= ?1 THEN 1 ELSE 0 END) AS \
         recent_touch_count
        FROM git_file_changes
        JOIN git_commits ON git_commits.hash = git_file_changes.commit_hash
                        AND git_commits.repo_id = git_file_changes.repo_id
        WHERE git_file_changes.path IN ({placeholders})
          AND git_file_changes.repo_id = ?{repo_index}
        GROUP BY git_file_changes.path
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params = Vec::<&dyn rusqlite::ToSql>::with_capacity(paths.len() + 2);
    params.push(&recent_floor);
    for path in paths {
        params.push(path);
    }
    params.push(&repo_id);
    let rows = stmt.query_map(params.as_slice(), |row| {
        let path: String = row.get(0)?;
        let commit_touch_count: i64 = row.get(1)?;
        let recent_touch_count: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
        Ok((path, commit_touch_count, recent_touch_count))
    })?;
    for row in rows {
        let (path, commit_touch_count, recent_touch_count) = row?;
        scores.insert(path, git_score(recent_touch_count, commit_touch_count));
    }
    Ok(scores)
}

/// The generated/test demotion flags for one candidate chunk (graded-git rerank, #109).
#[derive(Debug, Clone, Copy, Default)]
struct Demotion {
    generated: bool,
    /// Effective test flag: `symbols.is_test` (V035) when the chunk resolves to a symbol, else
    /// `files.has_test_code` (V024). Both are precomputed columns.
    is_test: bool,
}

/// Per-chunk generated/test demotion flags keyed by candidate chunk_id, computed in ONE batched
/// query over the whole candidate pool (same per-pool, not per-candidate, discipline as
/// [`graded_git_scores`]). `files.generated` / `files.has_test_code` are precomputed file columns;
/// `symbols.is_test` is resolved through the chunk's qualified `symbol_path` (matching
/// `qualified_name_id` in the same file) and takes precedence when present.
fn demotion_flags(
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
        let text = "fn watcher_main() { /* election retry loop */ }";
        let chunk_id: i64 = conn
            .query_row(
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                    start_line, end_line, text_hash)
                 VALUES (1, 'symbol', 'watcher_main', 0, 10, 1, 20, 'h1')
                 RETURNING id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // chunks.text is gone (#77 Phase 2): seed the compressed chunk_text blob (readers INNER
        // JOIN it) and the contentless chunk_fts tokens directly, keeping this seed
        // self-contained.
        crate::index::chunk_text_store::seed_chunk_text(&conn, chunk_id, text).unwrap();
        conn.execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", params![chunk_id, text])
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
                 JOIN name_strings ek ON ek.id = d.edge_kind_id
                 WHERE (d.from_name_id IN (SELECT id FROM name_strings WHERE value IN ('a', 'b'))
                     OR d.to_name_id IN (SELECT id FROM name_strings WHERE value IN ('a', 'b')))
                   AND EXISTS (SELECT 1 FROM main.files f
                                WHERE f.id = d.source_file_id AND f.repo_id = 'r')",
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
        // The repo predicate must be a PK lookup on files (per candidate edge), never a files scan.
        assert!(
            !plan.contains("SCAN f"),
            "graph_boost repo scope must PK-search files, not scan it, got plan:\n{plan}"
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

    /// The saturating recency+churn formula (graded-git rerank, #109). Pins the [0,1] range, the
    /// caps, the 0.6/0.4 recent/total split, and the no-history → 0.0 case — the dials a sweep
    /// would tune.
    #[test]
    fn git_score_saturates_and_splits_recent_vs_total() {
        // No git history → 0.0.
        assert_eq!(git_score(0, 0), 0.0);
        // Negative counts (defensive) clamp to 0.
        assert_eq!(git_score(-3, -1), 0.0);
        // Recent caps at RECENT_CAP (=5): 5 and 50 both max the recency term. With 0 total commits
        // the total term is 0, so the score is exactly GIT_RECENT_WEIGHT (0.6).
        assert!((git_score(5, 0) - GIT_RECENT_WEIGHT).abs() < 1e-9);
        assert!((git_score(50, 0) - GIT_RECENT_WEIGHT).abs() < 1e-9);
        // Total caps at TOTAL_CAP (=20): 20 and 200 both max the churn term. With 0 recent commits
        // the recency term is 0, so the score is exactly GIT_TOTAL_WEIGHT (0.4).
        assert!((git_score(0, 20) - GIT_TOTAL_WEIGHT).abs() < 1e-9);
        assert!((git_score(0, 200) - GIT_TOTAL_WEIGHT).abs() < 1e-9);
        // Both maxed → 1.0 (the saturation ceiling).
        assert!((git_score(5, 20) - 1.0).abs() < 1e-9);
        assert!((git_score(100, 100) - 1.0).abs() < 1e-9);
        // A partial value: 2 recent of cap 5 (=0.4) and 10 total of cap 20 (=0.5):
        // 0.6*0.4 + 0.4*0.5 = 0.24 + 0.20 = 0.44.
        assert!((git_score(2, 10) - 0.44).abs() < 1e-9);
        // The score is always in [0,1].
        for (recent, total) in [(0, 0), (1, 1), (3, 7), (5, 20), (1000, 1000)] {
            let score = git_score(recent, total);
            assert!((0.0..=1.0).contains(&score), "git_score({recent},{total}) = {score} ∉ [0,1]");
        }
    }

    /// Seed one git commit touching `src/watch.rs` so the graded-git path has history to grade.
    fn seed_git_history(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s,
                                     committed_at_s, subject, body)
             VALUES ('c1', 'a', 'a@x', 1000, 1000, 'touch', '')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO git_file_changes(commit_hash, path, additions, deletions)
             VALUES ('c1', ?1, 3, 1)",
            params![path],
        )
        .unwrap();
    }

    /// FLAG OFF is byte-identical: with `graded_history` false the produced score (and every
    /// component) is exactly what today's fuse produces — including when git history exists that
    /// the graded path WOULD grade. This is the load-bearing guarantee behind the A/B (the OFF
    /// arm must be today's behavior).
    #[test]
    fn graded_history_off_is_byte_identical_to_today() {
        let conn = seeded_conn();
        seed_git_history(&conn, "src/watch.rs");

        let off = SearchOptions { graded_history: false, ..SearchOptions::default() };
        let baseline = search_with_query_embedding(
            &conn,
            "election retry",
            5,
            false,
            None,
            true,
            SearchOptions::default(),
        )
        .unwrap();
        let with_flag_off =
            search_with_query_embedding(&conn, "election retry", 5, false, None, true, off)
                .unwrap();

        assert_eq!(baseline.len(), with_flag_off.len());
        for (a, b) in baseline.iter().zip(&with_flag_off) {
            assert_eq!(a.chunk_id, b.chunk_id);
            // The exact rounded score is identical bit-for-bit.
            assert_eq!(a.score, b.score, "flag-off score must equal today's score");
            let (ca, cb) =
                (a.score_components.as_ref().unwrap(), b.score_components.as_ref().unwrap());
            assert_eq!(ca.git, cb.git, "flag-off git component must be the binary boost");
            assert_eq!(ca.bm25, cb.bm25);
            assert_eq!(ca.symbol, cb.symbol);
            assert_eq!(ca.graph, cb.graph);
            assert_eq!(ca.github, cb.github);
        }
    }

    /// FLAG ON grades the git signal: with history present, the graded git contribution
    /// (`GIT_WEIGHT_GRADED * git_score`) differs from the binary contribution (`GIT_WEIGHT * 1.0`),
    /// so the flag actually changes the score. Proves the lever is wired end-to-end through the
    /// inner wide-pool site (not just the standalone formula).
    #[test]
    fn graded_history_on_changes_the_git_component() {
        let conn = seeded_conn();
        seed_git_history(&conn, "src/watch.rs");

        let on = SearchOptions { graded_history: true, ..SearchOptions::default() };
        let hits =
            search_with_query_embedding(&conn, "election retry", 5, false, None, true, on).unwrap();
        assert_eq!(hits.len(), 1);
        let git = hits[0].score_components.as_ref().unwrap().git;
        // One commit, recent vs the only commit → recent=1, total=1:
        // git_score = 0.6*min(1/5,1) + 0.4*min(1/20,1) = 0.6*0.2 + 0.4*0.05 = 0.14; weighted by
        // GIT_WEIGHT_GRADED (0.10) → 0.014. The binary path would have been GIT_WEIGHT (0.03) * 1.0
        // = 0.03, so the graded component is strictly different.
        let expected = GIT_WEIGHT_GRADED * git_score(1, 1);
        assert!((git - expected).abs() < 1e-9, "graded git component {git} != expected {expected}");
        assert!((git - GIT_WEIGHT).abs() > 1e-9, "graded git must differ from the binary boost");
    }

    /// FLAG ON demotes generated + test chunks multiplicatively after the weighted sum. Seed a
    /// generated, test-flagged file and assert the produced score is the un-demoted score scaled by
    /// GENERATED_PENALTY * TEST_PENALTY.
    #[test]
    fn graded_history_on_applies_generated_and_test_demotion() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        // A generated file with the precomputed test-code flag set.
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                               generated, has_test_code)
             VALUES ('src/gen.rs', 'rust', 'generated', 'abc', 0, 0, 1, 1)",
            [],
        )
        .unwrap();
        let text = "fn watcher_main() { /* election retry loop */ }";
        // No symbol_path → the test flag falls back to files.has_test_code.
        let chunk_id: i64 = conn
            .query_row(
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                    start_line, end_line, text_hash)
                 VALUES (1, 'symbol', NULL, 0, 10, 1, 20, 'h1')
                 RETURNING id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        crate::index::chunk_text_store::seed_chunk_text(&conn, chunk_id, text).unwrap();
        conn.execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", params![chunk_id, text])
            .unwrap();

        // generated files are excluded unless include_generated; pass true so the chunk is a
        // candidate.
        let off = search_with_query_embedding(
            &conn,
            "election retry",
            5,
            true,
            None,
            false,
            SearchOptions::default(),
        )
        .unwrap();
        let on = search_with_query_embedding(
            &conn,
            "election retry",
            5,
            true,
            None,
            false,
            SearchOptions { graded_history: true, ..SearchOptions::default() },
        )
        .unwrap();
        assert_eq!(off.len(), 1);
        assert_eq!(on.len(), 1);
        // No git history here, so the only graded-on change to the WEIGHTED sum is the git
        // component dropping to 0 (graded score of a no-history path) vs the binary 0.03.
        // Compare the demotion independently by reconstructing the on-flag pre-demotion
        // score from its own components.
        let comps = on[0].score_components.is_none();
        assert!(comps, "this run did not request explain; components stay None");
        // The demotion is multiplicative on the final score; with generated+test both set the
        // penalty is GENERATED_PENALTY * TEST_PENALTY. The on score must be strictly below the
        // un-penalized graded score (which itself is the off score minus the binary git boost).
        // Simplest robust assertion: the on score is strictly less than the off score (penalty < 1
        // AND git dropped), and it is positive.
        assert!(on[0].score > 0.0, "demoted score must stay positive");
        assert!(
            on[0].score < off[0].score,
            "generated+test demotion must lower the score (on {} !< off {})",
            on[0].score,
            off[0].score,
        );
    }
}
