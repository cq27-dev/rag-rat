//! The typed, content-addressed, cross-repo edge set (#464) — `repo_node_edges`. Generalizes the
//! memory→code binding into relation-typed edges from a source memory NODE to another node or an
//! external target (a GitHub issue today). The `anchors` relation is NOT here — memory→code stays
//! `repo_memory_bindings`, its deeply-wired projection; this module is everything else.
//!
//! Invariants (mirrored from the schema): `edge_key` is the stable content-addressed identity (a
//! `rebind` re-resolves the local rowid WITHOUT changing it); `repo_id` is the OWNER repo (the
//! source node's — the scope + adoption key) while `target_repo_id` may differ (cross-repo); the
//! resolved local rowids carry NO FK to volatile graph rows; a cross-repo target whose repo is not
//! present locally is stored `unresolved`, never a hard failure — and node targets are RE-RESOLVED
//! on every read (`reresolve_on_read`) by their globally-unique id, so an edge becomes `current`
//! once its target repo is indexed and its `target_repo_id` self-heals across a repo-id re-point.

use super::*;
use crate::query::memory::authoring;

/// The relation an edge expresses, from a source memory node to its target. Persisted in
/// `repo_node_edges.relation`; the db tokens are the snake_case variant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum EdgeRelation {
    /// A task depends on another task (the task DAG).
    DependsOn,
    /// A mind-map link between two nodes (undirected in spirit; stored source→target).
    RelatesTo,
    /// This node replaces another (e.g. a revised decision).
    Supersedes,
    /// This node was derived from another.
    DerivedFrom,
    /// A task tracks a GitHub issue (the reverse-bindable "issue ← task").
    Tracks,
}

impl EdgeRelation {
    pub(crate) fn as_db_str(self) -> &'static str {
        self.into()
    }

    pub(crate) fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("invalid edge relation `{value}`"))
    }
}

/// Where an edge points. MVP: another NODE (memory / task / concept) or a GitHub issue. More target
/// kinds (symbol / path / chunk / commit / call-path edge) are additive — the same
/// `(kind, repo, anchor)` shape, resolved against the index like a binding.
#[derive(Debug, Clone)]
pub enum EdgeTarget {
    /// Another node. `repo_id: None` means the SAME repo as the source (the common case);
    /// `Some(id)` is a cross-repo edge into a sibling on the shared global DB.
    Node { repo_id: Option<String>, node_id: String },
    /// A GitHub issue (the `tracks` relation): `owner/repo#number`.
    Github { owner: String, repo: String, number: i64 },
}

impl EdgeTarget {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Node { .. } => "node",
            Self::Github { .. } => "github",
        }
    }

    /// The portable, canonical anchor string that (with `kind`) content-addresses the target — the
    /// last component of `edge_key`. Globally unique on its own (a node id, or
    /// `owner/repo#number`).
    pub(crate) fn anchor(&self) -> String {
        match self {
            Self::Node { node_id, .. } => node_id.clone(),
            Self::Github { owner, repo, number } => format!("{owner}/{repo}#{number}"),
        }
    }

    /// The target's repo. A node carries its own; a GitHub ref belongs to the OWNER's repo, so the
    /// caller threads that in.
    pub(crate) fn target_repo_id(&self, owner_repo_id: &str) -> String {
        match self {
            Self::Node { repo_id, .. } =>
                repo_id.clone().unwrap_or_else(|| owner_repo_id.to_string()),
            Self::Github { .. } => owner_repo_id.to_string(),
        }
    }
}

/// One resolved edge row — the boundary shape returned by the edge APIs.
#[derive(Debug, Clone, Serialize)]
pub struct NodeEdge {
    pub edge_key: String,
    pub source_node_id: String,
    pub relation: String,
    pub target_repo_id: String,
    pub target_kind: String,
    pub target_anchor: String,
    /// The resolved local id when the target is a node present in this DB; `None` when unresolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_node_id: Option<String>,
    /// `current` | `gone` | `unresolved`.
    pub anchor_status: String,
}

/// The stable content-addressed edge identity. A `rebind` re-resolves the local rowid WITHOUT
/// changing this key, so a sync fold keeps presence/tombstones keyed by it.
///
/// It folds `(source_node_id, relation, target_kind, target_anchor)` — NOT the owner/target repo
/// ids. Those are redundant: a node id is globally unique and repo-folded (its derivation includes
/// the repo scope), so `source_node_id` and a node `target_anchor` already carry repo identity, and
/// a github `target_anchor` is repo-independent by construction. Dropping the repo ids makes the
/// key STABLE across a repo-id re-point (adoption / late-merge move the `repo_id` column but never
/// the node ids), so those paths need NO key recompute — only consolidation, which REMAPS node ids,
/// recomputes it. (A `\u{1e}` record-separator delimiter that cannot appear in any component; the
/// canonical deterministic-CBOR form is deferred to phase B (#404) with the rest of §5.5.)
pub(crate) fn edge_key(
    source_node_id: &str,
    relation: &str,
    target_kind: &str,
    target_anchor: &str,
) -> String {
    hex_sha256(
        format!("{source_node_id}\u{1e}{relation}\u{1e}{target_kind}\u{1e}{target_anchor}")
            .as_bytes(),
    )
}

/// Add a typed edge from `source_node_id` to `target`. Idempotent on `edge_key` — re-adding the
/// same logical edge refreshes its resolution (the `rebind` semantics) without minting a new row.
/// The source must be a node in the ACTIVE repo (its repo is the edge OWNER).
pub(crate) fn add_edge(
    conn: &Connection,
    source_node_id: &str,
    relation: EdgeRelation,
    target: &EdgeTarget,
) -> anyhow::Result<NodeEdge> {
    let owner_repo_id = source_node_owner_repo(conn, source_node_id)?.ok_or_else(|| {
        anyhow::anyhow!("source node `{source_node_id}` not found or is obsolete")
    })?;
    // A node must never edge to ITSELF (a self-loop is meaningless for a DAG / mind-map).
    if let EdgeTarget::Node { node_id, .. } = target
        && node_id == source_node_id
    {
        anyhow::bail!("an edge cannot point a node at itself");
    }
    let hint_repo_id = target.target_repo_id(&owner_repo_id);
    let target_kind = target.kind();
    let target_anchor = target.anchor();
    let key = edge_key(source_node_id, relation.as_db_str(), target_kind, &target_anchor);
    // Resolve the target against the CURRENT db. A node's ACTUAL owning repo is authoritative when
    // it is present (self-healing across a repo-id re-point); an absent cross-repo target keeps
    // the caller's hint and stays `unresolved` until that repo is indexed. A github ref is
    // always current.
    let (target_repo_id, target_node_id, anchor_status) = match target {
        EdgeTarget::Node { repo_id, node_id } => {
            let (repo, node, status) =
                resolve_node_target(conn, node_id, &hint_repo_id, "unresolved")?;
            match repo_id.as_deref() {
                // EXPLICIT cross-repo (`repo_id` names a DIFFERENT repo): a resolved target must
                // actually live in the NAMED repo (else the id points somewhere the caller didn't
                // name); an unresolved one is a legitimate deferred reference ONLY when that repo
                // is not indexed here — if the named repo IS registered, the
                // missing node is a typo.
                Some(named) if named != owner_repo_id => {
                    if status == "current" && repo != named {
                        anyhow::bail!(
                            "edge target node `{node_id}` resolves to repo `{repo}`, not the \
                             named `{named}`"
                        );
                    }
                    if status != "current" && repo_is_registered(conn, named)? {
                        anyhow::bail!(
                            "edge target node `{node_id}` is not a node in repo `{named}`"
                        );
                    }
                },
                // SAME-repo intent (`repo_id` omitted, or equal to the owner): the target must be a
                // PRESENT node in the owner repo — an absent one is a typo, one that resolves to a
                // SIBLING repo is an IMPLICIT cross-repo edge the caller must make explicit.
                _ =>
                    if status != "current" || repo != owner_repo_id {
                        anyhow::bail!(
                            "edge target node `{node_id}` is not a node in this repo (pass an \
                             explicit target_repo_id for a cross-repo edge)"
                        );
                    },
            }
            (repo, node, status)
        },
        EdgeTarget::Github { .. } => (hint_repo_id.clone(), None, "current".to_string()),
    };
    let now = now_ms();
    // Backfill the pre-existing history (idempotent) + the edge INSERT + the EdgeAdd op in ONE
    // transaction (strict-atomic); the write via `conn` participates in the open txn.
    authoring::backfill_memory_oplog(conn, now)?;
    let tx = conn.unchecked_transaction()?;
    // Whether this is a GENUINELY new edge vs an idempotent re-add: the INSERT below is
    // `ON CONFLICT DO UPDATE` refreshing ONLY the per-device resolution columns
    // (`target_repo_id`/`target_node_id`/`anchor_status`) — state the log deliberately excludes (no
    // `Rebind`). A re-add therefore changes nothing log-relevant, so it must NOT author a second
    // `EdgeAdd`: re-asserting presence at a fresh Lamport could resurrect a concurrent remove from
    // another device under sync (the symmetric partner of `remove_edge`'s `n > 0` gate).
    let edge_is_new = edge_by_key(conn, &key)?.is_none();
    conn.execute(
        "
        INSERT INTO repo_node_edges(
            edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
            target_anchor, target_node_id, anchor_status, created_at_ms
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(edge_key) DO UPDATE SET
            target_repo_id = excluded.target_repo_id,
            target_node_id = excluded.target_node_id,
            anchor_status = excluded.anchor_status
        ",
        params![
            key,
            owner_repo_id,
            source_node_id,
            relation.as_db_str(),
            target_repo_id,
            target_kind,
            target_anchor,
            target_node_id,
            anchor_status,
            now
        ],
    )?;
    if edge_is_new {
        authoring::author_edge_add(
            &tx,
            source_node_id,
            relation,
            &target_repo_id,
            target_kind,
            &target_anchor,
            &owner_repo_id,
            now,
        )?;
    }
    tx.commit()?;
    edge_by_key(conn, &key)?.ok_or_else(|| anyhow::anyhow!("edge `{key}` disappeared after insert"))
}

/// Remove an edge by its stable `edge_key`. A no-op (returns `false`) when the key is unknown.
pub(crate) fn remove_edge(conn: &Connection, edge_key: &str) -> anyhow::Result<bool> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = periphery_edge_scope_clause(&scope);
    let now = now_ms();
    authoring::backfill_memory_oplog(conn, now)?;
    let tx = conn.unchecked_transaction()?;
    let n = conn
        .execute(&format!("DELETE FROM repo_node_edges WHERE edge_key = ?1{repo_clause}"), [
            edge_key,
        ])?;
    if n > 0 {
        // Author an EdgeRemove tombstone ONLY when a row was actually removed, in the same txn.
        authoring::author_edge_remove(&tx, edge_key, now)?;
    }
    tx.commit()?;
    Ok(n > 0)
}

/// Every edge OUT of `source_node_id` (its outgoing graph — deps, mind-map links, tracks).
pub(crate) fn edges_from(conn: &Connection, source_node_id: &str) -> anyhow::Result<Vec<NodeEdge>> {
    edges_from_source(conn, source_node_id, SourceScope::LiveOnly)
}

/// Every edge FROM `source_node_id`, INCLUDING those whose source memory is `obsolete`/`rejected` —
/// the complete-history read the op-log backfill needs. It is exactly [`edges_from`] MINUS the
/// `LIVE_SOURCE_PREDICATE` (a non-live source's edges stay visible); it KEEPS `reresolve_on_read`,
/// so a node target's `target_repo_id` is repaired to CURRENT before it is signed into the op-log
/// `EdgeSpec` (the stored column is only an add-time snapshot — stale after a repo-id re-point or
/// if the target was `unresolved` at add time, and a signed op cannot be corrected later).
/// `edge_key` itself folds node ids, not repo ids, so it is stable regardless.
pub(crate) fn all_edges_from(
    conn: &Connection,
    source_node_id: &str,
) -> anyhow::Result<Vec<NodeEdge>> {
    edges_from_source(conn, source_node_id, SourceScope::All)
}

/// Whether an outgoing-edge read is filtered to a LIVE source (the recall surface) or spans EVERY
/// source regardless of its memory's status (the complete-history backfill).
enum SourceScope {
    LiveOnly,
    All,
}

/// Shared body of [`edges_from`] / [`all_edges_from`]: the only difference is whether the
/// `LIVE_SOURCE_PREDICATE` is applied. Both are owner-scoped and both re-resolve node targets on
/// read.
fn edges_from_source(
    conn: &Connection,
    source_node_id: &str,
    scope_kind: SourceScope,
) -> anyhow::Result<Vec<NodeEdge>> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = periphery_edge_scope_clause(&scope);
    let live_clause = match scope_kind {
        SourceScope::LiveOnly => LIVE_SOURCE_PREDICATE,
        SourceScope::All => "",
    };
    let mut stmt = conn.prepare(&format!(
        "{EDGE_SELECT} WHERE source_node_id = ?1{repo_clause}{live_clause} ORDER BY relation, \
         target_anchor"
    ))?;
    let rows = stmt.query_map([source_node_id], edge_row)?.collect::<rusqlite::Result<_>>()?;
    reresolve_on_read(conn, rows)
}

/// Every edge INTO a target — the reverse traversal (e.g. "tasks that `track` issue N", "notes that
/// `depend_on` task X"). Cross-repo aware: matches on the portable `(target_repo_id, kind,
/// anchor)`.
pub(crate) fn edges_into(conn: &Connection, target: &EdgeTarget) -> anyhow::Result<Vec<NodeEdge>> {
    // A node/github target's anchor is globally unique, so match on `(target_kind, target_anchor)`
    // alone — no `target_repo_id` needed (which also means a repo-id re-point never breaks reverse
    // traversal). Owner-scoped like the other memory reads; cross-repo reverse traversal (a sibling
    // repo's inbound edge) is a follow-up.
    let scope = memory_repo_scope(conn)?;
    let repo_clause = periphery_edge_scope_clause(&scope);
    let mut stmt = conn.prepare(&format!(
        "{EDGE_SELECT} WHERE target_kind = ?1 AND target_anchor = \
         ?2{repo_clause}{LIVE_SOURCE_PREDICATE} ORDER BY source_node_id, relation"
    ))?;
    let rows = stmt
        .query_map(params![target.kind(), target.anchor()], edge_row)?
        .collect::<rusqlite::Result<_>>()?;
    reresolve_on_read(conn, rows)
}

/// Read one edge by its `edge_key` (active-repo scoped).
pub(crate) fn edge_by_key(conn: &Connection, edge_key: &str) -> anyhow::Result<Option<NodeEdge>> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = periphery_edge_scope_clause(&scope);
    conn.query_row(&format!("{EDGE_SELECT} WHERE edge_key = ?1{repo_clause}"), [edge_key], edge_row)
        .optional()
        .map_err(Into::into)
}

const UNASSIGNED_REPO: &str = "__unassigned__";

const EDGE_SELECT: &str = "
    SELECT edge_key, source_node_id, relation, target_repo_id, target_kind, target_anchor,
           target_node_id, anchor_status
    FROM repo_node_edges";

/// Edges are surfaced only for a LIVE source node — one whose memory is still surfaceable by
/// recall. That is `status IN ('active', 'stale')`, exactly as the binding reads
/// (`memories_for_symbol` etc.) filter: a `stale` node is live (its anchor drifted, not its
/// memory), only `obsolete`/`rejected` are dead. A subquery, not a join, so the `EDGE_SELECT`
/// column list / `edge_row` mapper are unchanged.
const LIVE_SOURCE_PREDICATE: &str =
    " AND source_node_id IN (SELECT id FROM repo_memories WHERE status IN ('active', 'stale'))";

fn edge_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeEdge> {
    Ok(NodeEdge {
        edge_key: row.get("edge_key")?,
        source_node_id: row.get("source_node_id")?,
        relation: row.get("relation")?,
        target_repo_id: row.get("target_repo_id")?,
        target_kind: row.get("target_kind")?,
        target_anchor: row.get("target_anchor")?,
        target_node_id: row.get("target_node_id")?,
        anchor_status: row.get("anchor_status")?,
    })
}

/// The ` AND repo_node_edges.repo_id = '…'` OWNER-scope predicate, or `""` when unscoped (pre-A5).
/// The edge is owned by / authored on its source node's repo, so reads/deletes scope by `repo_id`
/// exactly like the other periphery tables.
fn periphery_edge_scope_clause(scope: &Option<String>) -> String {
    crate::index::schema::periphery_repo_scope_clause(scope, "repo_node_edges")
}

/// Whether `repo_id` is a repo REGISTERED (indexed) in this DB — a row in `repos`. An explicit
/// cross-repo edge target in a REGISTERED repo must resolve (else it's a typo); one in an unknown
/// repo is a legitimate deferred `unresolved` reference until that repo is indexed.
fn repo_is_registered(conn: &Connection, repo_id: &str) -> anyhow::Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM repos WHERE repo_id = ?1)", [repo_id], |r| r.get(0))
        .map_err(Into::into)
}

/// The active-repo owner of `source_node_id` (its `repo_id`), or `None` when the node is not a LIVE
/// memory in the active repo. New edges are authored on — and owned by — this repo; a dead
/// (`obsolete`/`rejected`) node is treated as absent so you cannot author a relationship FROM it
/// (the add-time twin of the `LIVE_SOURCE_PREDICATE` read filter — a `stale` node is still live).
fn source_node_owner_repo(conn: &Connection, node_id: &str) -> anyhow::Result<Option<String>> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    let owner = scope.clone().unwrap_or_else(|| UNASSIGNED_REPO.to_string());
    let exists: bool = conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM repo_memories WHERE id = ?1 AND status IN ('active', \
             'stale'){repo_clause})"
        ),
        [node_id],
        |r| r.get(0),
    )?;
    Ok(exists.then_some(owner))
}

/// Resolve a NODE target by its GLOBALLY-UNIQUE id → `(target_repo_id, resolved local node id,
/// anchor_status)`. When the node is present its ACTUAL owning `repo_id` is authoritative (so the
/// result self-heals across a repo-id re-point / adoption — the id never changes, only the column);
/// when absent, keep `hint_repo_id` (an unresolved cross-repo edge remembers which repo it names).
/// `prior_status` distinguishes a target that was NEVER present (`unresolved`) from one that was
/// and is now deleted (`gone`) — a fresh add passes `"unresolved"`.
fn resolve_node_target(
    conn: &Connection,
    node_id: &str,
    hint_repo_id: &str,
    prior_status: &str,
) -> anyhow::Result<(String, Option<String>, String)> {
    let actual_repo: Option<String> = conn
        .query_row(
            "SELECT COALESCE(repo_id, '__unassigned__') FROM repo_memories WHERE id = ?1",
            [node_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match actual_repo {
        Some(repo_id) => (repo_id, Some(node_id.to_string()), "current".to_string()),
        None => {
            let status = if prior_status == "current" || prior_status == "gone" {
                "gone" // was resolved before, target since deleted
            } else {
                "unresolved" // never resolved — the target repo is not indexed here (yet)
            };
            (hint_repo_id.to_string(), None, status.to_string())
        },
    })
}

/// Re-resolve every NODE edge's target against the CURRENT db (the stored resolution is only an
/// add-time snapshot). This is what makes an `unresolved` cross-repo edge become `current` once its
/// target repo is indexed, and refreshes `target_repo_id` from the live node — so a read always
/// reflects the live graph regardless of any repo-id re-point since the edge was authored. A github
/// (external) target is not db-backed and is left as stored (`current`).
fn reresolve_on_read(conn: &Connection, mut edges: Vec<NodeEdge>) -> anyhow::Result<Vec<NodeEdge>> {
    for edge in &mut edges {
        if edge.target_kind != "node" {
            continue;
        }
        let (repo_id, node_id, status) = resolve_node_target(
            conn,
            &edge.target_anchor,
            &edge.target_repo_id,
            &edge.anchor_status,
        )?;
        edge.target_repo_id = repo_id;
        edge.target_node_id = node_id;
        edge.anchor_status = status;
    }
    Ok(edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The strum-derived tokens are PERSISTED in `repo_node_edges.relation` — pin them exactly so a
    /// rename or reorder can't silently repoint stored rows (the `as_db_str`/`from_db_str`
    /// contract).
    #[test]
    fn edge_relation_db_tokens_are_stable_and_round_trip() {
        for (relation, token) in [
            (EdgeRelation::DependsOn, "depends_on"),
            (EdgeRelation::RelatesTo, "relates_to"),
            (EdgeRelation::Supersedes, "supersedes"),
            (EdgeRelation::DerivedFrom, "derived_from"),
            (EdgeRelation::Tracks, "tracks"),
        ] {
            assert_eq!(relation.as_db_str(), token);
            assert_eq!(EdgeRelation::from_db_str(token).unwrap(), relation);
        }
        assert!(EdgeRelation::from_db_str("bogus").is_err(), "an unknown token must not resolve");
    }
}
