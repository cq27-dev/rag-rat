//! `dependent_of_dead_source` findings (#1444): a live memory derived — directly or through other
//! derived memories — from a memory that no longer stands. A summary built from a withdrawn fact is
//! stale the moment the fact is withdrawn, yet nothing else says so.
//!
//! Truth-maintenance semantics: a dependent is only *suspect*. Nothing is marked obsolete; the
//! finding names the dead source (and what superseded it, when something did) so a reviewer can
//! re-check the dependent, and it resolves by itself once the source stands again, the edge is
//! dropped, or the dependent is retired — the pass re-evaluates every live memory on every run.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rusqlite::Connection;

use super::DreamFinding;
use super::findings::FindingKind;

/// Why a source memory no longer stands. "All bindings gone" reads the stored `anchor_status`, the
/// last validation's verdict in whichever checkout ran it — the same basis `stale_reference` and
/// `memory_doctor` use.
fn death_reason(status: &str, bindings: i64, gone: i64) -> Option<&'static str> {
    match status {
        "obsolete" => Some("obsolete"),
        "rejected" => Some("rejected"),
        _ if bindings > 0 && gone == bindings => Some("anchored only to code that is gone"),
        _ => None,
    }
}

/// Whether a node an edge touches still stands. A memory absent from this store is unknown, never
/// dead: local memories are retired, not deleted, and the rows that do vanish are the sync drain's
/// (a quarantined peer update, an edge that arrived before its target, a target consolidation left
/// behind) — memories that still stand elsewhere, which a reviewer must not be told to act on.
struct Node {
    dead: Option<&'static str>,
}

/// One finding per live memory in the active repo that reaches a dead memory along `derived_from`
/// edges. The walk follows edges owned by the active repo, so a chain through another repo's memory
/// stops there. Edges are read by `target_anchor` — the target's globally unique node id — not
/// `target_node_id`, which is left unset on edges received by sync. The evidence names memories by
/// id only, so renaming one does not re-open a reviewed finding; adding a successor does.
pub(super) fn dependent_of_dead_source_findings(
    conn: &Connection,
) -> rusqlite::Result<Vec<DreamFinding>> {
    let scope = rag_rat_db::schema::periphery_repo_scope(conn, "repo_memories")?;
    let repo_clause = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "repo_memories");
    let edge_clause = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "repo_node_edges");
    let live = rag_rat_query::memory::live_memory_status_sql("repo_memories");

    // Derived-from edges out of the active repo's live memories, to any memory node.
    let mut parents: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut stmt = conn.prepare(&format!(
        "SELECT repo_node_edges.source_node_id, repo_node_edges.target_anchor
         FROM repo_node_edges
         JOIN repo_memories ON repo_memories.id = repo_node_edges.source_node_id
         WHERE repo_node_edges.relation = 'derived_from'
           AND repo_node_edges.target_kind = 'node'
           AND {live}{repo_clause}{edge_clause}"
    ))?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (child, parent) = row?;
        parents.entry(child).or_default().push(parent);
    }
    if parents.is_empty() {
        return Ok(Vec::new());
    }
    for list in parents.values_mut() {
        list.sort();
        list.dedup();
    }

    // Every node an edge touches and whether it still stands. A target in another repo is read
    // wherever it lives — its standing does not depend on which repo cites it.
    let ids: HashSet<&String> = parents.keys().chain(parents.values().flatten()).collect();
    let ids_json = serde_json::to_string(&ids)
        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
    let nodes: HashMap<String, Node> = conn
        .prepare(
            "SELECT m.id, m.status,
                    (SELECT COUNT(*) FROM repo_memory_bindings b WHERE b.memory_id = m.id),
                    (SELECT COUNT(*) FROM repo_memory_bindings b
                      WHERE b.memory_id = m.id AND b.anchor_status = 'gone')
             FROM repo_memories m WHERE m.id IN (SELECT value FROM json_each(?1))",
        )?
        .query_map([ids_json], |r| {
            let status: String = r.get(1)?;
            Ok((r.get::<_, String>(0)?, Node { dead: death_reason(&status, r.get(2)?, r.get(3)?) }))
        })?
        .collect::<rusqlite::Result<_>>()?;

    // Live memories in the active repo that `supersede` another: `(target, successor)`, ordered so
    // the smallest successor id is named for each target.
    let supersedes: BTreeSet<(String, String)> = conn
        .prepare(&format!(
            "SELECT repo_node_edges.target_anchor, repo_memories.id
             FROM repo_node_edges
             JOIN repo_memories ON repo_memories.id = repo_node_edges.source_node_id
             WHERE repo_node_edges.relation = 'supersedes'
               AND repo_node_edges.target_kind = 'node'
               AND {live}{repo_clause}"
        ))?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut out = Vec::new();
    for child in parents.keys() {
        if nodes.get(child).is_none_or(|node| node.dead.is_some()) {
            continue;
        }
        let Some((via, root)) = dead_root(child, &parents, &nodes, &mut HashSet::new()) else {
            continue;
        };
        // A rewrite recorded as both `derived_from` and `supersedes` its source is the replacement,
        // not a dependent left standing on it.
        if supersedes.contains(&(root.clone(), child.clone())) {
            continue;
        }
        let reason = nodes[&root].dead.unwrap_or("dead");
        let mut evidence = if via == root {
            format!("derived from {root}, which is {reason}")
        } else {
            format!("derived from {via}, which rests on {root}, which is {reason}")
        };
        if let Some((_, successor)) = supersedes
            .range((root.clone(), String::new())..)
            .next()
            .filter(|(target, _)| *target == root)
        {
            evidence.push_str(&format!("; {successor} supersedes it"));
        }
        evidence.push_str(
            ": re-check this memory against it, update or retire it, or drop the edge [E0]",
        );
        out.push(DreamFinding {
            kind: FindingKind::DependentOfDeadSource,
            subject: child.clone(),
            evidence,
            rank: FindingKind::DependentOfDeadSource.base_rank(),
        });
    }
    Ok(out)
}

/// The first parent (in id order) with a route to a dead memory, and the dead memory that route
/// reaches first.
/// Depth-first with a visited set, so a `derived_from` cycle terminates. Not memoized: a result
/// cached mid-cycle could be wrong, and the `derived_from` graph is small.
fn dead_root(
    node: &String,
    parents: &BTreeMap<String, Vec<String>>,
    nodes: &HashMap<String, Node>,
    visited: &mut HashSet<String>,
) -> Option<(String, String)> {
    if !visited.insert(node.clone()) {
        return None;
    }
    for parent in parents.get(node).into_iter().flatten() {
        let Some(parent_node) = nodes.get(parent) else {
            continue;
        };
        if parent_node.dead.is_some() {
            return Some((parent.clone(), parent.clone()));
        }
        if let Some((_, root)) = dead_root(parent, parents, nodes, visited) {
            return Some((parent.clone(), root));
        }
    }
    None
}
