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

/// Per-edge-kind weight: structural dependencies (calls, impls) carry full importance; weaker
/// associations (imports, containment) carry less. Unknown kinds default to `1.0`.
fn edge_weight(kind: &str) -> f64 {
    match kind {
        "calls_name" | "implements" => 1.0,
        "references_type" | "constructs" => 0.7,
        "uses_macro" => 0.5,
        "imports" | "exports" => 0.3,
        "contains" => 0.2,
        _ => 1.0,
    }
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
    // the scope view. `edge_strings` resolves the edge-kind id to its name for weighting.
    let mut stmt = conn.prepare(
        "SELECT d.from_symbol_id, d.to_symbol_id, ek.value
         FROM edges_data d
         JOIN files ON files.id = d.source_file_id
         JOIN edge_strings ek ON ek.id = d.edge_kind_id
         WHERE d.from_symbol_id IS NOT NULL AND d.to_symbol_id IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?))
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
    for (from, to, kind) in &rows {
        let from_idx = intern(*from, &mut index_of, &mut symbol_ids);
        let to_idx = intern(*to, &mut index_of, &mut symbol_ids);
        if out_edges.len() < symbol_ids.len() {
            out_edges.resize_with(symbol_ids.len(), Vec::new);
        }
        out_edges[from_idx].push((to_idx, edge_weight(kind)));
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

    // Top-`limit` indices by score.
    let mut ranked: Vec<usize> = (0..symbol_ids.len()).collect();
    ranked.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(options.limit);

    // Hydrate the winners with symbol metadata in one query.
    let mut out = Vec::with_capacity(ranked.len());
    for idx in ranked {
        let symbol_id = symbol_ids[idx];
        let meta = conn
            .query_row(
                "SELECT qualified_name, kind, file_id FROM symbols WHERE id = ?1",
                [symbol_id],
                |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
                },
            )
            .ok();
        let Some((qualified_name, kind, file_id)) = meta else { continue };
        let path = conn
            .query_row("SELECT path FROM files WHERE id = ?1", [file_id], |row| {
                row.get::<_, String>(0)
            })
            .unwrap_or_default();
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

        let out =
            important_symbols(&conn, ImportanceOptions { limit: 10, personalize_to: &[] }).unwrap();
        assert!(!out.is_empty(), "graph has edges → results");
        assert_eq!(out[0].qualified_name, "a::hub", "the called hub ranks first: {out:?}");
        assert_eq!(out[0].path, "a.rs");
        assert_eq!(out[0].kind, "function");

        // No resolved symbol→symbol edges → empty (not an error).
        let bare = Connection::open_in_memory().unwrap();
        schema::apply(&bare).unwrap();
        assert!(
            important_symbols(&bare, ImportanceOptions { limit: 10, personalize_to: &[] })
                .unwrap()
                .is_empty()
        );
    }
}
