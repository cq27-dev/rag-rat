//! The AUTHORED typed-edge mutations (`add_edge` / `remove_edge`) and the complete-history read
//! the op-log reconcile consumes (`unauthored_edges`). Edge types and the read queries live in
//! `rag_rat_query::memory`.

use rag_rat_base::time::now_ms;
use rag_rat_oplog::StreamId;
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
    // Backfill the pre-existing history (idempotent) + the edge INSERT + the EdgeAdd op in ONE
    // transaction (strict-atomic); the write via `conn` participates in the open txn.
    authoring::backfill_memory_oplog(conn, now)?;
    let prepared = authoring::prepare_live_content_authoring(conn, now)?;
    // Authored write: the EdgeAdd op is signed op-log content, so commit durably (#560).
    let _durability = authoring::AuthoredDurability::begin(conn)?;
    let tx = conn.unchecked_transaction()?;
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
        authoring::author_edge_add(
            &tx,
            source_node_id,
            relation,
            &target_repo_id,
            target_kind,
            &target_anchor,
            &owner_repo_id,
            prepared.as_ref(),
            now,
        )?;
    }
    tx.commit()?;
    edge_by_key(conn, &key)?.ok_or_else(|| anyhow::anyhow!("edge `{key}` disappeared after insert"))
}

pub(crate) fn remove_edge(conn: &Connection, edge_key: &str) -> anyhow::Result<bool> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = periphery_edge_scope_clause(&scope);
    let now = now_ms();
    authoring::backfill_memory_oplog(conn, now)?;
    let prepared = authoring::prepare_live_content_authoring(conn, now)?;
    // Authored write: the EdgeRemove tombstone is signed op-log content, so commit durably (#560).
    let _durability = authoring::AuthoredDurability::begin(conn)?;
    let tx = conn.unchecked_transaction()?;
    let n = conn
        .execute(&format!("DELETE FROM repo_node_edges WHERE edge_key = ?1{repo_clause}"), [
            edge_key,
        ])?;
    if n > 0 {
        // Author an EdgeRemove tombstone ONLY when a row was actually removed, in the same txn.
        authoring::author_edge_remove(&tx, edge_key, prepared.as_ref(), now)?;
    }
    tx.commit()?;
    Ok(n > 0)
}

pub(crate) fn unauthored_edges(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<Vec<NodeEdge>> {
    let mut stmt = conn.prepare(&format!(
        "{EDGE_SELECT} e
         WHERE e.repo_id = ?1
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
mod tests {
    use rag_rat_oplog::StreamId;
    use rag_rat_query::memory::edge_key;
    use rusqlite::{Connection, params};

    use super::unauthored_edges;

    const REPO: &str = "repo-a";

    /// A DB with the memory schema, one registered repo, and the connection scoped to it — the
    /// minimal setup `memory_repo_scope` needs to resolve an active repo. Mirrors
    /// `authoring::tests::scoped_conn` (each module's test scaffolding is self-contained).
    fn scoped_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
            [REPO],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [REPO],
        )
        .unwrap();
        conn
    }
    fn insert_memory(conn: &Connection, id: &str, status: &str, created_at_ms: i64) {
        conn.execute(
            "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES (?1, 'Invariant', ?1, 'body', 'high', ?2, 'agent', ?3, ?3, 'agent', 'h', 'v1',
                 ?4)",
            params![id, status, created_at_ms, REPO],
        )
        .unwrap();
    }
    /// Insert a node-edge by RAW SQL, bypassing the wired `add_edge` author — a "ghost edge" that
    /// exists in `repo_node_edges` but was never signed into the op-log. Returns the computed
    /// `edge_key` so callers can seed/inspect the projection by it.
    fn insert_raw_node_edge(
        conn: &Connection,
        source: &str,
        relation: &str,
        target: &str,
    ) -> String {
        let key = edge_key(source, relation, "node", target);
        conn.execute(
            "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation,
                 target_repo_id, target_kind, target_anchor, target_node_id, anchor_status,
                 created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?2, 'node', ?5, ?5, 'current', 100)",
            params![key, REPO, source, relation, target],
        )
        .unwrap();
        key
    }

    /// #541/#664: the reconcile's edge reader anti-joins `repo_node_edges` against the accepted-`/3`
    /// projection `content_projected_edges` and re-resolves what it returns. Proves BOTH halves of
    /// the correctness crux: (a) only the edge absent from the projection comes back, and (b)
    /// its `target_repo_id` — deliberately stale on the stored row, simulating an add-time
    /// snapshot left behind by a repo-id re-point — is repaired to the CURRENT owner before it
    /// would be signed.
    #[test]
    fn unauthored_edges_returns_only_edges_absent_from_the_projection_reresolved() {
        let conn = scoped_conn();
        insert_memory(&conn, "mem_a", "active", 100);
        insert_memory(&conn, "mem_b", "active", 200);
        insert_memory(&conn, "mem_c", "active", 300);
        let authored_key = insert_raw_node_edge(&conn, "mem_a", "relates_to", "mem_b");
        let ghost_key = insert_raw_node_edge(&conn, "mem_a", "depends_on", "mem_c");

        // `stream` is an opaque `StreamId` here — the anti-join only needs seed/query agreement.
        let stream = StreamId::from_bytes([0x11; 32]);
        // Seed the accepted-`/3` projection with the `relates_to` edge only — it is already
        // authored.
        conn.execute(
            "INSERT INTO content_projected_edges(stream_id, edge_key, spec_json, resolved_json)
             VALUES (?1, ?2, '{}', NULL)",
            params![stream.to_bytes().as_slice(), authored_key],
        )
        .unwrap();
        // Simulate an add-time snapshot gone stale: the ghost edge's stored `target_repo_id` no
        // longer matches mem_c's CURRENT owning repo (as if a repo-id re-point happened after the
        // edge row was written).
        conn.execute(
            "UPDATE repo_node_edges SET target_repo_id = 'stale-repo-id' WHERE edge_key = ?1",
            [&ghost_key],
        )
        .unwrap();

        let missing = unauthored_edges(&conn, REPO, stream).unwrap();
        assert_eq!(
            missing.iter().map(|e| e.edge_key.as_str()).collect::<Vec<_>>(),
            [ghost_key.as_str()],
            "only the edge absent from the projection returns"
        );
        assert_eq!(
            missing[0].target_repo_id, REPO,
            "reresolve_on_read must repair the stale stored target_repo_id to the CURRENT owner \
             before the reconcile signs it"
        );
    }
}
