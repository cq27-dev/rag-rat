//! Weighted PageRank over the symbol→symbol edge graph — "which symbols are load-bearing?" (#108).
//!
//! An edge `from_symbol → to_symbol` (a call, type reference, impl, …) flows importance to the
//! callee, so a symbol many things depend on scores high. The pure [`pagerank`] core takes a
//! weighted adjacency + an optional personalization vector (bias the random surfer toward the
//! agent's changed/query symbols — the Aider repo-map trick) and is unit-tested without a DB;
//! [`important_symbols`] builds the adjacency from the active checkout's resolved edges and returns
//! the top-ranked symbols with metadata.

use std::collections::HashMap;

use rusqlite::Connection;
use serde::Serialize;

/// Standard random-restart probability — the surfer follows an edge with probability `DAMPING`,
/// teleports with `1 - DAMPING`.
const DAMPING: f64 = 0.85;
/// Iteration cap; PageRank on these graphs converges well within this. Paired with `TOLERANCE`.
const MAX_ITERS: usize = 100;
/// L1-change convergence threshold: stop once the rank vector barely moves.
const TOLERANCE: f64 = 1e-6;
/// Hard cap on returned rows. Hydration is one point query per winner, so a hostile or careless
/// `limit` (the MCP/CLI surface takes a `u32`) can't turn this into tens of thousands of lookups.
const MAX_RESULTS: usize = 500;

/// Per-edge-kind weight: structural dependencies (calls, impls) carry full importance; weaker
/// associations (imports, containment) carry less. Unknown kinds default to `1.0`.
///
/// `pub(crate)` so the scoped-weighted-fan-in enrichment (`query::load_bearing`, the third
/// importance scale) reuses the SAME weight table — this is the single source of truth for
/// edge-kind weighting; do not duplicate it.
pub(crate) fn edge_weight(kind: &str) -> f64 {
    match kind {
        "calls_name" | "implements" => 1.0,
        "references_type" | "constructs" => 0.7,
        "uses_macro" => 0.5,
        "imports" | "exports" => 0.3,
        "contains" => 0.2,
        _ => 1.0,
    }
}

/// Multiplier on [`edge_weight`] by the heuristic resolver's confidence in the edge
/// (`EdgeConfidence` db strings). A name-only guess is a weak signal that a dependency exists, so
/// it should flow less of the source's rank to that callee than a structurally-resolved call.
/// Unknown values default to `1.0` (same defensive posture as [`edge_weight`]). A SCIP-verified
/// edge bypasses this entirely and uses [`COMPILER_FACTOR`].
///
/// `pub(crate)` so `query::load_bearing` reuses the SAME confidence table for scoped-weighted
/// fan-in — single source of truth; do not duplicate.
pub(crate) fn confidence_factor(confidence: &str) -> f64 {
    match confidence {
        "Exact" => 1.0,
        "Syntactic" => 0.85,
        "NameOnly" => 0.4,
        "Ambiguous" => 0.2,
        _ => 1.0,
    }
}

/// Confidence factor for an edge a SCIP oracle confirmed or resolved — above a heuristic `Exact`
/// (`1.0`), so a compiler-verified dependency outranks a merely well-guessed one. Because PageRank
/// normalizes each source node's out-weights, the bonus only changes the outcome at a *mixed*
/// source (some edges compiler-verified, some heuristic), tilting that node's rank toward the
/// verified callees; it is a no-op at a node whose edges are all the same tier.
///
/// `pub(crate)` so `query::load_bearing` weights an oracle-confirmed in-edge at the compiler tier
/// in scoped-weighted fan-in — single source of truth; do not duplicate.
pub(crate) const COMPILER_FACTOR: f64 = 1.2;

/// What a current, in-scope SCIP oracle verdict does to an edge during ranking, keyed by `edge_id`.
/// Built by the query layer from `OracleResolutionKind` so this module stays free of oracle types.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EdgeOracleEffect {
    /// The real callee is out of corpus — a `resolved-external`, or a `contradict` the compiler
    /// couldn't resolve to an in-corpus symbol. Drop the phantom in-repo edge from the graph.
    Drop,
    /// The compiler confirmed the heuristic target — keep it, but weight at [`COMPILER_FACTOR`].
    Confirm,
    /// The compiler resolved the edge to an in-corpus symbol id — retarget the edge there and
    /// weight at [`COMPILER_FACTOR`]. Covers both an `upgrade` (of an unconfirmed edge) and an
    /// in-corpus `contradict` (overriding a wrong heuristic target): in both cases the id is
    /// the compiler's answer.
    Retarget(i64),
}

/// One node's outgoing edges as `(target_index, weight)`. Index space is `0..n`.
pub type Adjacency = Vec<Vec<(usize, f64)>>;

/// Weighted PageRank. `out_edges[i]` is node `i`'s outgoing `(target, weight)` list. `personalize`,
/// when `Some`, is the teleport distribution (need not be normalized; non-finite/negative entries
/// are treated as 0, and an all-zero vector falls back to uniform); `None` = uniform teleport.
/// Returns a score per node summing to ~1. Dangling nodes (no out-edges) redistribute their mass
/// via the teleport vector each iteration, so rank is conserved.
pub fn pagerank(n: usize, out_edges: &Adjacency, personalize: Option<&[f64]>) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    // Teleport distribution: normalized personalization, or uniform.
    let teleport: Vec<f64> = match personalize {
        Some(p) if p.len() == n => {
            let total: f64 = p.iter().filter(|x| x.is_finite() && **x > 0.0).sum();
            if total > 0.0 {
                p.iter().map(|x| if x.is_finite() && *x > 0.0 { x / total } else { 0.0 }).collect()
            } else {
                vec![1.0 / n as f64; n]
            }
        },
        _ => vec![1.0 / n as f64; n],
    };
    // Pre-normalize each node's out-weights so they sum to 1 (a dangling node stays empty).
    let normalized: Adjacency = out_edges
        .iter()
        .map(|edges| {
            let total: f64 = edges.iter().map(|(_, w)| w.max(0.0)).sum();
            if total > 0.0 {
                edges.iter().map(|&(t, w)| (t, w.max(0.0) / total)).collect()
            } else {
                Vec::new()
            }
        })
        .collect();

    let mut rank = teleport.clone();
    for _ in 0..MAX_ITERS {
        // Mass stranded on dangling nodes this round, redistributed via teleport.
        let dangling: f64 = (0..n).filter(|&i| normalized[i].is_empty()).map(|i| rank[i]).sum();
        let mut next = vec![0.0_f64; n];
        for (i, edges) in normalized.iter().enumerate() {
            let share = DAMPING * rank[i];
            for &(target, weight) in edges {
                next[target] += share * weight;
            }
        }
        let base = (1.0 - DAMPING) + DAMPING * dangling;
        for i in 0..n {
            next[i] += base * teleport[i];
        }
        let delta: f64 = (0..n).map(|i| (next[i] - rank[i]).abs()).sum();
        rank = next;
        if delta < TOLERANCE {
            break;
        }
    }
    rank
}

/// Which importance scale a result was computed on. The label string is the contract the spec's
/// "three scales" table pins — agents read it to know whether they're looking at GLOBAL or
/// PERSONALIZED PageRank, so they must NEVER be presented as comparable. (The third scale, "local
/// structural load", is a different surface — `impact_surface`/search enrichment — not this tool.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportanceMode {
    /// No (effective) seeds — live whole-graph PageRank.
    Global,
    /// Seeded (by name/id or auto-diff) — PageRank biased toward the working set.
    PersonalizedToChanges,
}

impl ImportanceMode {
    /// The agent-facing label. Matches the spec's three-scale table verbatim.
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global PageRank importance",
            Self::PersonalizedToChanges => "importance relative to your current changes",
        }
    }
}

impl Serialize for ImportanceMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.label())
    }
}

/// How the personalization seed was produced. `git_diff` = auto-seeded from the working set (the
/// MCP default); `explicit` = the caller named symbols (ids/names/paths).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedKind {
    GitDiff,
    Explicit,
}

/// Seeds that were considered but did not contribute a graph node, bucketed by why. Present only on
/// an auto-diff seed (`git_diff`); an explicit seed reports its misses via `skipped.no_symbols` too
/// but has no deleted/generated notion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SkippedSeeds {
    /// Changed paths that no longer exist on disk (deleted/renamed-away) — git-diff only.
    pub deleted: u64,
    /// Changed paths classified as generated artifacts — git-diff only.
    pub generated: u64,
    /// Considered seeds (paths or names) that resolved to no symbol in the active scope.
    pub no_symbols: u64,
}

/// Provenance of the personalization seed, present only when the result is seeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeedSource {
    pub kind: SeedKind,
    /// Paths the seed considered (git-diff: changed paths; explicit: 0 — explicit seeds are names,
    /// not paths).
    pub changed_paths: u64,
    /// Of `changed_paths`, how many were indexed in the active scope (git-diff only).
    pub indexed_paths: u64,
    /// Symbol ids that became graph seeds.
    pub symbol_seed_count: u64,
    pub skipped: SkippedSeeds,
}

/// The labeled result of an importance query: the mode + seed provenance carry more for an agent
/// than the bare ranking does. This is the output contract for the CLI/MCP `important_symbols`
/// surface (a labeled object, NOT a bare array).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportantSymbolsResult {
    pub mode: ImportanceMode,
    /// Present only when the result is seeded (mode = personalized OR a fall-through that still
    /// computed a seed source). Absent for an un-seeded global query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_source: Option<SeedSource>,
    /// Present only on a fall-through to global from an intended seed (e.g. the diff had no
    /// indexed symbols) — a short human-readable reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Diff paths considered, surfaced on a fall-through so the caller sees WHY the seed was
    /// empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_paths_considered: Option<u64>,
    /// Of the considered diff paths, how many yielded any indexed symbol — `0` on the no-symbols
    /// fall-through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_paths_with_symbols: Option<u64>,
    pub symbols: Vec<SymbolImportance>,
}

/// A ranked load-bearing symbol.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SymbolImportance {
    pub symbol_id: i64,
    pub qualified_name: String,
    pub path: String,
    pub kind: String,
    /// PageRank score (higher = more depended-upon). Scores sum to ~1 across all graph nodes.
    pub score: f64,
}

/// Options for [`important_symbols`].
pub struct ImportanceOptions<'a> {
    /// Max symbols to return.
    pub limit: usize,
    /// Optional personalization seed — symbol ids to bias importance toward (the agent's changed /
    /// query symbols). Empty/none = global importance.
    pub personalize_to: &'a [i64],
    /// Optional SCIP-oracle effects keyed by `edge_id` (current + in-scope verdicts). `None` = no
    /// oracle run for this checkout → rank the heuristic graph with confidence weighting only. The
    /// caller builds this so absent oracle data costs zero (no scan); see the query-layer wrapper.
    pub oracle_effects: Option<&'a HashMap<i64, EdgeOracleEffect>>,
}

/// Compute weighted PageRank over the active checkout's resolved symbol→symbol edges and return the
/// top-`limit` load-bearing symbols. Reads `edges_data` joined to the per-connection `files` scope
/// view (active checkout only), using edges where both endpoints resolved to a symbol. Returns an
/// empty list when the graph has no resolved symbol edges.
pub fn important_symbols(
    conn: &Connection,
    options: ImportanceOptions<'_>,
) -> anyhow::Result<Vec<SymbolImportance>> {
    // Resolved symbol→symbol edges in the active checkout: both endpoints non-null, source file in
    // the scope view. `edge_strings` resolves the edge-kind and confidence ids to their names — the
    // kind sets the base weight, the confidence scales it (a name-only guess flows less rank than a
    // structurally-resolved call). `d.id` keys the optional SCIP-oracle effect lookup.
    let mut stmt = conn.prepare(
        "SELECT d.id, d.from_symbol_id, d.to_symbol_id, ek.value, cf.value
         FROM edges_data d
         JOIN files ON files.id = d.source_file_id
         JOIN edge_strings ek ON ek.id = d.edge_kind_id
         JOIN edge_strings cf ON cf.id = d.confidence_id
         WHERE d.from_symbol_id IS NOT NULL AND d.to_symbol_id IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // Map symbol ids to a dense 0..n index space.
    let mut index_of: HashMap<i64, usize> = HashMap::new();
    let mut symbol_ids: Vec<i64> = Vec::new();
    let intern = |id: i64, index_of: &mut HashMap<i64, usize>, ids: &mut Vec<i64>| -> usize {
        *index_of.entry(id).or_insert_with(|| {
            ids.push(id);
            ids.len() - 1
        })
    };
    let mut out_edges: Adjacency = Vec::new();
    for (edge_id, from, to, kind, confidence) in &rows {
        // Apply the SCIP verdict, if any: drop a contradicted/external edge entirely, retarget an
        // upgrade to the compiler's resolved symbol, and weight a confirmed/upgraded edge at the
        // compiler tier. Absent a verdict, fall back to heuristic confidence weighting.
        let (to_id, weight) = match options.oracle_effects.and_then(|m| m.get(edge_id)) {
            Some(EdgeOracleEffect::Drop) => continue,
            Some(EdgeOracleEffect::Retarget(resolved)) =>
                (*resolved, edge_weight(kind) * COMPILER_FACTOR),
            Some(EdgeOracleEffect::Confirm) => (*to, edge_weight(kind) * COMPILER_FACTOR),
            None => (*to, edge_weight(kind) * confidence_factor(confidence)),
        };
        let from_idx = intern(*from, &mut index_of, &mut symbol_ids);
        let to_idx = intern(to_id, &mut index_of, &mut symbol_ids);
        if out_edges.len() < symbol_ids.len() {
            out_edges.resize_with(symbol_ids.len(), Vec::new);
        }
        out_edges[from_idx].push((to_idx, weight));
    }
    out_edges.resize_with(symbol_ids.len(), Vec::new);

    // Personalization: 1.0 on each seed symbol that is present in the graph, else uniform.
    let personalize: Option<Vec<f64>> = if options.personalize_to.is_empty() {
        None
    } else {
        let mut vector = vec![0.0_f64; symbol_ids.len()];
        let mut any = false;
        for id in options.personalize_to {
            if let Some(&idx) = index_of.get(id) {
                vector[idx] = 1.0;
                any = true;
            }
        }
        any.then_some(vector)
    };

    let scores = pagerank(symbol_ids.len(), &out_edges, personalize.as_deref());

    // Top-`limit` indices by score. `sort_by` is stable, so equal scores keep insertion order →
    // deterministic output for a fixed index. Clamp to `MAX_RESULTS` so the hydration loop below is
    // bounded regardless of the caller's `limit`.
    let mut ranked: Vec<usize> = (0..symbol_ids.len()).collect();
    ranked.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(options.limit.min(MAX_RESULTS));

    // Hydrate the winners with symbol metadata. Joining `symbols` to the per-connection `files`
    // scope view keeps hydration scope-consistent with the edge query: a winner whose file isn't in
    // the active checkout drops out instead of emitting an empty path. Endpoints are active-scope
    // by the edge re-resolution invariant, so this rarely fires — when it does, the result is
    // shorter than `limit` rather than wrong.
    let mut out = Vec::with_capacity(ranked.len());
    for idx in ranked {
        let symbol_id = symbol_ids[idx];
        let row = conn
            .query_row(
                "SELECT s.qualified_name, s.kind, f.path
                 FROM symbols s
                 JOIN files f ON f.id = s.file_id
                 WHERE s.id = ?1",
                [symbol_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .ok();
        let Some((qualified_name, kind, path)) = row else { continue };
        out.push(SymbolImportance { symbol_id, qualified_name, path, kind, score: scores[idx] });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;
    use crate::index::schema;

    fn approx_desc(scores: &[f64]) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..scores.len()).collect();
        idx.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap());
        idx
    }

    /// `ImportanceOptions` with no SCIP effects — the heuristic + confidence ranking path.
    fn opts(limit: usize, personalize_to: &[i64]) -> ImportanceOptions<'_> {
        ImportanceOptions { limit, personalize_to, oracle_effects: None }
    }

    /// Score of the symbol with `qualified_name` in a result set, or `0.0` when absent (a dropped /
    /// never-interned node).
    fn score_of(out: &[SymbolImportance], qualified_name: &str) -> f64 {
        out.iter().find(|s| s.qualified_name == qualified_name).map_or(0.0, |s| s.score)
    }

    #[test]
    fn ranks_a_hub_highest() {
        // 0→2, 1→2, 3→2 : everyone depends on 2 → 2 is the most load-bearing.
        let adj: Adjacency = vec![vec![(2, 1.0)], vec![(2, 1.0)], vec![], vec![(2, 1.0)]];
        let scores = pagerank(4, &adj, None);
        assert_eq!(
            approx_desc(&scores)[0],
            2,
            "the hub everyone points at ranks first: {scores:?}"
        );
        let sum: f64 = scores.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "rank is conserved (sums to 1): {sum}");
    }

    #[test]
    fn weight_lifts_the_heavier_dependency() {
        // 0 depends on both 1 and 2, but on 2 much more heavily → 2 outranks 1.
        let adj: Adjacency = vec![vec![(1, 0.2), (2, 1.0)], vec![], vec![]];
        let scores = pagerank(3, &adj, None);
        assert!(scores[2] > scores[1], "heavier-weighted callee ranks higher: {scores:?}");
    }

    #[test]
    fn dangling_node_conserves_rank() {
        // 0→1, 1 dangling (no out-edges): mass must not leak — total stays ~1.
        let adj: Adjacency = vec![vec![(1, 1.0)], vec![]];
        let scores = pagerank(2, &adj, None);
        let sum: f64 = scores.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "dangling mass redistributed, not lost: {sum}");
    }

    #[test]
    fn personalization_biases_toward_the_seed() {
        // A symmetric two-node graph; personalizing toward node 0 must rank it above node 1.
        let adj: Adjacency = vec![vec![(1, 1.0)], vec![(0, 1.0)]];
        let global = pagerank(2, &adj, None);
        assert!((global[0] - global[1]).abs() < 1e-6, "symmetric graph is balanced: {global:?}");
        let biased = pagerank(2, &adj, Some(&[1.0, 0.0]));
        assert!(biased[0] > biased[1], "personalization lifts the seed node: {biased:?}");
    }

    #[test]
    fn empty_graph_is_empty() {
        let empty: Adjacency = Vec::new();
        assert!(pagerank(0, &empty, None).is_empty());
    }

    #[test]
    fn invalid_personalization_falls_back_to_uniform() {
        // Wrong length and all-zero personalization both degrade to uniform teleport (no panic).
        let adj: Adjacency = vec![vec![(1, 1.0)], vec![(0, 1.0)]];
        let wrong_len = pagerank(2, &adj, Some(&[1.0]));
        let all_zero = pagerank(2, &adj, Some(&[0.0, 0.0]));
        for scores in [wrong_len, all_zero] {
            assert!((scores[0] - scores[1]).abs() < 1e-6, "uniform fallback: {scores:?}");
        }
    }

    #[test]
    fn edge_weight_downweights_weak_kinds() {
        assert!(edge_weight("calls_name") > edge_weight("imports"));
        assert!(edge_weight("implements") > edge_weight("contains"));
        assert_eq!(edge_weight("something_new"), 1.0, "unknown kinds default to full weight");
    }

    #[test]
    fn important_symbols_ranks_the_most_depended_on_first() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('a.rs', 'rust', 'source', 'h', 0, 0)",
            [],
        )
        .unwrap();
        for (name, qname) in
            [("caller_a", "a::caller_a"), ("caller_b", "a::caller_b"), ("hub", "a::hub")]
        {
            conn.execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte,
                                     end_byte, signature, docs)
                 VALUES (1, 'rust', ?1, ?2, 'function', 0, 10, NULL, NULL)",
                params![name, qname],
            )
            .unwrap();
        }
        // symbol ids 1,2,3 in insertion order; both callers call the hub (id 3).
        for from in [1_i64, 2] {
            conn.execute(
                "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                                   target_qualified_name, edge_kind, confidence)
                 VALUES (1, ?1, 3, 'hub', 'a::hub', 'calls_name', 'exact')",
                params![from],
            )
            .unwrap();
        }

        let out = important_symbols(&conn, opts(10, &[])).unwrap();
        assert!(!out.is_empty(), "graph has edges → results");
        assert_eq!(out[0].qualified_name, "a::hub", "the called hub ranks first: {out:?}");
        assert_eq!(out[0].path, "a.rs");
        assert_eq!(out[0].kind, "function");

        // No resolved symbol→symbol edges → empty (not an error).
        let bare = Connection::open_in_memory().unwrap();
        schema::apply(&bare).unwrap();
        assert!(important_symbols(&bare, opts(10, &[])).unwrap().is_empty());
    }

    /// Insert `n` symbols (`s1..sn`, ids `1..=n`) plus `edges` as `(from_id, to_id)` calls, all in
    /// one file. Returns the connection ready for `important_symbols`.
    fn graph_conn(n: usize, edges: &[(i64, i64)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('a.rs', 'rust', 'source', 'h', 0, 0)",
            [],
        )
        .unwrap();
        for i in 1..=n {
            conn.execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte,
                                     end_byte, signature, docs)
                 VALUES (1, 'rust', ?1, ?2, 'function', 0, 10, NULL, NULL)",
                params![format!("s{i}"), format!("a::s{i}")],
            )
            .unwrap();
        }
        for &(from, to) in edges {
            conn.execute(
                "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                                   target_qualified_name, edge_kind, confidence)
                 VALUES (1, ?1, ?2, 'x', 'a::x', 'calls_name', 'exact')",
                params![from, to],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn limit_truncates_and_a_huge_limit_is_safe() {
        let conn = graph_conn(3, &[(1, 3), (2, 3)]);
        // `limit` truncates to fewer than the node count.
        let one = important_symbols(&conn, opts(1, &[])).unwrap();
        assert_eq!(one.len(), 1, "limit caps the result count");
        // An absurd limit clamps to the node count via MAX_RESULTS — no panic, no overflow, no
        // tens-of-thousands of point lookups.
        let huge = important_symbols(&conn, opts(u32::MAX as usize, &[])).unwrap();
        assert_eq!(huge.len(), 3, "bounded by the graph, never by the caller's limit");
    }

    #[test]
    fn personalization_biases_the_db_ranking() {
        // Two disjoint symmetric 2-cycles: {1↔2} and {3↔4}. Globally all four are balanced;
        // personalizing toward symbol 1 must lift its cluster (1,2) above the other (3,4).
        let conn = graph_conn(4, &[(1, 2), (2, 1), (3, 4), (4, 3)]);
        let out = important_symbols(&conn, opts(4, &[1])).unwrap();
        let top_two: Vec<&str> = out.iter().take(2).map(|s| s.qualified_name.as_str()).collect();
        assert!(
            top_two.iter().all(|q| *q == "a::s1" || *q == "a::s2"),
            "personalized cluster ranks first: {out:?}"
        );
    }

    /// Like [`graph_conn`] but each edge carries an explicit confidence (`EdgeConfidence::as_str`
    /// casing, e.g. `"Exact"` / `"NameOnly"`); kind is always `calls_name`. Edge ids are `1..` in
    /// insertion order, so a test can key an `oracle_effects` map by them.
    fn conf_conn(n: usize, edges: &[(i64, i64, &str)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('a.rs', 'rust', 'source', 'h', 0, 0)",
            [],
        )
        .unwrap();
        for i in 1..=n {
            conn.execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte,
                                     end_byte, signature, docs)
                 VALUES (1, 'rust', ?1, ?2, 'function', 0, 10, NULL, NULL)",
                params![format!("s{i}"), format!("a::s{i}")],
            )
            .unwrap();
        }
        for &(from, to, confidence) in edges {
            conn.execute(
                "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                                   target_qualified_name, edge_kind, confidence)
                 VALUES (1, ?1, ?2, 'x', 'a::x', 'calls_name', ?3)",
                params![from, to, confidence],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn confidence_factor_orders_by_certainty() {
        assert!(confidence_factor("Exact") > confidence_factor("Syntactic"));
        assert!(confidence_factor("Syntactic") > confidence_factor("NameOnly"));
        assert!(confidence_factor("NameOnly") > confidence_factor("Ambiguous"));
        assert_eq!(
            confidence_factor("whatever"),
            1.0,
            "unknown confidence defaults to full weight"
        );
    }

    #[test]
    fn confidence_lifts_the_more_certain_dependency() {
        // 1 calls 2 (Exact) and 3 (NameOnly), same kind → the exactly-resolved callee outranks the
        // name-only guess purely on confidence.
        let conn = conf_conn(3, &[(1, 2, "Exact"), (1, 3, "NameOnly")]);
        let out = important_symbols(&conn, opts(10, &[])).unwrap();
        assert!(
            score_of(&out, "a::s2") > score_of(&out, "a::s3"),
            "higher-confidence callee ranks higher: {out:?}"
        );
    }

    #[test]
    fn contradicted_edge_is_dropped_from_ranking() {
        // 1 calls 2 (edge id 1) and 3 (edge id 2); the oracle contradicts 1→2. The phantom target
        // gets no rank and falls out of the graph entirely; the real callee ranks first.
        let conn = conf_conn(3, &[(1, 2, "Exact"), (1, 3, "Exact")]);
        let effects = HashMap::from([(1_i64, EdgeOracleEffect::Drop)]);
        let out = important_symbols(&conn, ImportanceOptions {
            limit: 10,
            personalize_to: &[],
            oracle_effects: Some(&effects),
        })
        .unwrap();
        assert_eq!(out[0].qualified_name, "a::s3", "the surviving callee ranks first: {out:?}");
        assert!(
            !out.iter().any(|s| s.qualified_name == "a::s2"),
            "the contradicted-only target drops out of the graph: {out:?}"
        );
    }

    #[test]
    fn upgrade_retargets_rank_to_the_resolved_symbol() {
        // 1 calls 2 heuristically (edge id 1), but the oracle upgrades the edge to resolve at 3.
        // Rank flows to the compiler's target, not the heuristic guess.
        let conn = conf_conn(3, &[(1, 2, "NameOnly")]);
        let effects = HashMap::from([(1_i64, EdgeOracleEffect::Retarget(3))]);
        let out = important_symbols(&conn, ImportanceOptions {
            limit: 10,
            personalize_to: &[],
            oracle_effects: Some(&effects),
        })
        .unwrap();
        assert_eq!(out[0].qualified_name, "a::s3", "rank flows to the retargeted symbol: {out:?}");
        assert!(
            !out.iter().any(|s| s.qualified_name == "a::s2"),
            "the heuristic target gets no rank: {out:?}"
        );
    }

    #[test]
    fn confirm_overrides_low_heuristic_confidence() {
        // 1 calls 2 (name_only, CONFIRMED, edge id 1) and 3 (name_only, no verdict, edge id 2).
        // Confirm weights 1→2 at the compiler tier (above NameOnly), so 2 outranks 3.
        let conn = conf_conn(3, &[(1, 2, "NameOnly"), (1, 3, "NameOnly")]);
        let effects = HashMap::from([(1_i64, EdgeOracleEffect::Confirm)]);
        let out = important_symbols(&conn, ImportanceOptions {
            limit: 10,
            personalize_to: &[],
            oracle_effects: Some(&effects),
        })
        .unwrap();
        assert!(
            score_of(&out, "a::s2") > score_of(&out, "a::s3"),
            "the confirmed edge outranks an equal-confidence unverified one: {out:?}"
        );
    }

    #[test]
    fn no_oracle_run_is_heuristic_confidence_only() {
        // `None` effects and an empty effects map must rank identically — guards the CPU gate: an
        // absent oracle introduces no behavioral dependency, it's just the confidence-weighted
        // path.
        let conn = conf_conn(3, &[(1, 2, "Exact"), (1, 3, "NameOnly")]);
        let none = important_symbols(&conn, opts(10, &[])).unwrap();
        let empty: HashMap<i64, EdgeOracleEffect> = HashMap::new();
        let empty_map = important_symbols(&conn, ImportanceOptions {
            limit: 10,
            personalize_to: &[],
            oracle_effects: Some(&empty),
        })
        .unwrap();
        assert_eq!(none, empty_map, "None and an empty effects map rank identically");
    }
}
