//! The AUTHORED typed-edge mutations (`add_edge` / `remove_edge`) and the complete-history read
//! the op-log reconcile consumes (`unauthored_edges`). Edge types and the read queries live in
//! `rag_rat_query::memory`.

use rag_rat_base::time::now_ms;
use rag_rat_oplog::{EdgeSpec, NodeId, StreamId};
use rag_rat_query::memory::{
    EDGE_SELECT, EdgeRelation, EdgeTarget, NodeEdge, edge_by_key, edge_key, edge_row,
    memory_repo_scope, periphery_edge_scope_clause, repo_is_registered, reresolve_on_read,
    resolve_node_target, source_node_owner_repo, validate_edge_len,
};
use rusqlite::{Connection, params};

use super::authoring;

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
    // Byte-cap the free-form edge inputs at the write boundary (#680), the edge twin of the
    // create/update payload cap: the anchor + resolved target repo id are carried verbatim into the
    // signed `EdgeAdd` op, so an oversized one would mint an un-authorable edge that the reconcile
    // must then quarantine forever. Reject it here — cheaply, before the resolution lookups — so
    // the normal API can never persist one. `target_repo_id` is re-checked post-resolution
    // below (an unresolved cross-repo target keeps the caller's raw hint).
    validate_edge_len("target_anchor", &target_anchor)?;
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
    // A resolved node target's repo is the (bounded) owning repo, but an UNRESOLVED explicit
    // cross-repo target keeps the caller's raw hint id — cap it too, on the value actually stored +
    // signed (#680).
    validate_edge_len("target_repo_id", &target_repo_id)?;
    let now = now_ms();
    // Authored write: the EdgeAdd op is signed op-log content, so commit durably (#560); the
    // tombstone check and `edge_by_key` READ before the INSERT, hence IMMEDIATE (#818).
    let write = authoring::AuthoredWrite::begin(conn, now)?;
    // #767: revalidate the removal tombstone INSIDE the write txn — a connection that resolved the
    // source node's owner before `rm` ran must fail closed here rather than INSERT an edge row
    // stamped with the removed `repo_id` after the purge.
    super::assert_repo_not_removed(conn, &owner_repo_id)?;
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
        let edge = EdgeSpec {
            source_node_id: NodeId::from(source_node_id),
            relation,
            target_repo_id,
            target_kind: target_kind.to_string(),
            target_anchor,
            owner_repo_id,
        };
        authoring::author_edge_add(&write.tx, edge, write.prepared.as_ref(), now)?;
    }
    write.commit()?;
    edge_by_key(conn, &key)?.ok_or_else(|| anyhow::anyhow!("edge `{key}` disappeared after insert"))
}

pub(crate) fn remove_edge(conn: &Connection, edge_key: &str) -> anyhow::Result<bool> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = periphery_edge_scope_clause(&scope);
    let now = now_ms();
    // Authored write: the EdgeRemove tombstone is signed op-log content, so commit durably (#560).
    let write = authoring::AuthoredWrite::begin(conn, now)?;
    let n = conn
        .execute(&format!("DELETE FROM repo_node_edges WHERE edge_key = ?1{repo_clause}"), [
            edge_key,
        ])?;
    if n > 0 {
        // Author an EdgeRemove tombstone ONLY when a row was actually removed, in the same txn.
        authoring::author_edge_remove(&write.tx, edge_key, write.prepared.as_ref(), now)?;
    }
    write.commit()?;
    Ok(n > 0)
}

pub(crate) fn unauthored_edges(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<Vec<NodeEdge>> {
    anyhow::ensure!(
        !rag_rat_oplog::content_stream_has_pending_refold(conn, stream)?,
        "owner stream has a pending content refold; settle pending content refolds before reading \
         edge completeness"
    );
    // `origin = 'local'` gates out SYNCED edges (#691 A-pre): only locally-authored edges are the
    // reconcile's to complete. The `NOT EXISTS` also now skips a TOMBSTONED edge — a removed edge
    // is retained in `content_projected_edges` (present=0), so it exists here and is never
    // re-authored; dropping the tombstone let a foreign `EdgeRemove` be re-added at a fresh
    // Lamport (a growth loop).
    let mut stmt = conn.prepare(&format!(
        "{EDGE_SELECT} e
         WHERE e.repo_id = ?1
           AND e.origin = 'local'
           AND NOT EXISTS (
                 SELECT 1 FROM content_projected_edges p
                 WHERE p.stream_id = ?2 AND p.edge_key = e.edge_key)
         ORDER BY e.edge_key"
    ))?;
    let raw = stmt
        .query_map(params![repo_id, stream.to_bytes().as_slice()], edge_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    reresolve_on_read(conn, raw)
}

#[cfg(test)]
#[path = "edges_tests.rs"]
mod tests;
