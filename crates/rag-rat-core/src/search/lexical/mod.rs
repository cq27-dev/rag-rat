mod candidates;
mod history;
mod query;
mod scoring;

use std::collections::BTreeMap;

use rag_rat_db::text_compression;
use rusqlite::Connection;

use crate::index::ai;

const BM25_WEIGHT: f64 = 0.45;
const VECTOR_WEIGHT: f64 = 0.35;
const SYMBOL_WEIGHT: f64 = 0.10;
const GRAPH_WEIGHT: f64 = 0.05;
const GIT_WEIGHT: f64 = 0.03;
const PAPERTRAIL_WEIGHT: f64 = 0.02;

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

pub use rag_rat_query::{ScoreComponents, SearchHit};

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
    let terms = query::query_terms(query);
    // REPO SCOPING (A4): every candidate row flows through the `files` scope VIEW (both the bm25
    // and the vector pass JOIN `files`), which filters `repo_id` FIRST — so in a consolidated
    // DB a sibling repo's chunks are dropped by the INNER JOIN before ranking. `active_repo_id`
    // is resolved ONCE here and threaded into the git/papertrail boost queries (which read the
    // direct-scoped `git_file_changes` / `papertrail_refs` by path, bypassing the view).
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    // RECALL BOUND: `chunk_fts` / `chunk_embeddings` MATCH globally, then the repo (+ commit +
    // worktree) filter is applied by the scope-view JOIN. Because SQLite applies `LIMIT` AFTER the
    // join filter, the candidate window is a PER-REPO window — the active repo is never starved by
    // a sibling repo's matches. `candidate_limit` (8× the caller's limit, well past the plan's
    // 4× floor) caps how many post-filter candidates enter ranking; a query whose active-repo
    // matches exceed 8×limit keeps only the BM25/vector-top `candidate_limit` of them, the same
    // bound as a single-repo DB.
    let candidate_limit = i64::from(limit.max(10)).saturating_mul(8);
    let vector_available = query_embedding.is_some();
    let mut ranked = BTreeMap::<i64, scoring::RankedHit>::new();
    // One dict decoder shared by both candidate passes (#77 Phase 2): bm25 and vector each
    // decompress a batch of snippet text and run sequentially, so loading the dict versions once
    // here avoids the duplicate SELECT + dictionary prep the two passes used to do independently.
    let dicts = rag_rat_query::chunk_text_dicts(conn)?;
    let mut decoder = text_compression::ChunkTextDecoder::new(&dicts);

    for (rank, hit) in
        candidates::bm25_candidates(conn, query, candidate_limit, include_generated, &mut decoder)?
            .into_iter()
            .enumerate()
    {
        let entry = ranked.entry(hit.chunk_id).or_insert_with(|| scoring::RankedHit::new(hit));
        entry.components.bm25 = BM25_WEIGHT * scoring::lexical_rank_score(rank);
    }

    for (hit, similarity) in candidates::vector_candidates(
        conn,
        query,
        candidate_limit,
        include_generated,
        query_embedding,
        &mut decoder,
    )? {
        let entry = ranked.entry(hit.chunk_id).or_insert_with(|| scoring::RankedHit::new(hit));
        entry.components.vector = VECTOR_WEIGHT * f64::from(similarity).clamp(0.0, 1.0);
    }

    // Graded-git rerank (#109): ONE batched git-churn query + ONE batched demotion query over the
    // WHOLE candidate pool, computed only under the flag. With `graded_history` false both maps
    // stay empty and unused, so the boost/finish path below is byte-identical to today. The
    // wide pre-truncation pool is the only site with reorder headroom beyond the top-10.
    let (graded_git, demotions) = if options.graded_history {
        let paths = ranked.values().map(|hit| hit.hit.path.clone()).collect::<Vec<_>>();
        let chunk_ids = ranked.keys().copied().collect::<Vec<_>>();
        (
            history::graded_git_scores(conn, &paths, &repo_id)?,
            scoring::demotion_flags(conn, &chunk_ids)?,
        )
    } else {
        (std::collections::HashMap::new(), std::collections::HashMap::new())
    };

    let mut hits = ranked
        .into_values()
        .map(|mut hit| {
            let boosts = scoring::boosts(conn, &hit.hit, &terms, options, &repo_id)?;
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
            hit.components.papertrail = PAPERTRAIL_WEIGHT * boosts.papertrail;
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
                finished.score = rag_rat_query::round_score(finished.score * penalty);
            }
            Ok(finished)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(hits)
}

#[cfg(test)]
mod tests;
