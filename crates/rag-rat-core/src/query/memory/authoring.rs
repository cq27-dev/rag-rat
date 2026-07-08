//! Translating persisted memories into signed op-log entries, and the one-time full backfill of a
//! store's pre-existing memories into the log (#524).
//!
//! This bridges `repo_memories` / `repo_node_edges` (owned by this module) and the op-log MINTING
//! primitives ([`crate::oplog`]) — a ONE-WAY dependency, so `oplog` never depends back on the
//! memory subsystem (the next increment adds the reverse call, `create_memory` → author, and a
//! cycle would break the build).
//!
//! NOT wired into the live write path yet: [`backfill_memory_oplog`] is exercised in isolation. The
//! next increment calls it before the first live-authored mutation and reuses [`memory_to_ops`] for
//! the mutations themselves.

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::hydrate::tags_for_memory;
use super::{EdgeRelation, NodeEdge, all_edges_from, memory_repo_scope};
use crate::oplog::{
    EdgeSpec, MemoryOp, NodeContent, NodeId, NodeStatus, author_genesis_in_tx, chain_tail,
    local_device, owner_stream,
};

/// One memory's projectable content — the columns the op model carries (NOT the identity / anchor /
/// dedup bookkeeping). Read in bulk so the backfill makes one pass over `repo_memories`.
struct MemoryRow {
    memory_id: String,
    kind: String,
    title: String,
    body: String,
    confidence: String,
    status: String,
    source: String,
    payload_json: Option<String>,
    tags: Vec<String>,
}

/// Translate one memory into its op sequence: a `NodeCreate` for content, a `NodeStatus` ONLY when
/// the status is not the fold's `active` create-time default, and an `EdgeAdd` per outgoing typed
/// node-edge (in `edge_key` order, so the authored chain is reproducible). Code-anchor BINDINGS are
/// excluded — they are per-device derived resolution state, re-validated locally, and never part of
/// the shared node graph the projection models.
fn memory_to_ops(
    row: &MemoryRow,
    owner_repo_id: &str,
    edges: &[NodeEdge],
) -> anyhow::Result<Vec<MemoryOp>> {
    let node_id = NodeId::from(row.memory_id.as_str());
    let mut ops = vec![MemoryOp::NodeCreate {
        node_id: node_id.clone(),
        content: NodeContent {
            kind: row.kind.clone(),
            title: row.title.clone(),
            body: row.body.clone(),
            confidence: row.confidence.clone(),
            source: row.source.clone(),
            tags: row.tags.clone(),
            payload: row.payload_json.clone(),
        },
    }];
    // `active` is the fold's create-time default, so it needs no op. A status token this binary
    // does NOT recognize (a newer binary's future status) must NOT be silently dropped — that would
    // permanently mint this row as `active` in the SIGNED history, and a signed op cannot be
    // corrected later. FAIL the backfill instead, so a binary that understands the token authors
    // it.
    if row.status != NodeStatus::default().as_db_str() {
        let status = NodeStatus::from_db_str(&row.status).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot backfill memory `{}`: unknown status token `{}` (a newer binary must \
                 author this history)",
                row.memory_id,
                row.status
            )
        })?;
        ops.push(MemoryOp::NodeStatus { node_id: node_id.clone(), status });
    }
    // An `EdgeAdd` ONLY — deliberately NO `Rebind`. `EdgeAdd` carries the edge's presence and the
    // DURABLE spec (incl. the `target_repo_id` `all_edges_from` re-resolved to current). The
    // `Rebind` op's resolved dimension (`target_node_id`, `anchor_status`) is PER-DEVICE
    // derived state: `anchor_status` (current/gone/unresolved — is the target present in THIS
    // db) differs by device and is recomputed on every read by `reresolve_on_read`. Signing it
    // into the immutable shared history would bake one device's view into the log — excluded
    // for the same reason code-anchor BINDINGS are. The projection's `resolved` column stays
    // NULL for a backfilled edge; the reader recomputes it locally, exactly as the live edge
    // table does.
    let mut edges = edges.to_vec();
    edges.sort_by(|a, b| a.edge_key.cmp(&b.edge_key));
    for edge in &edges {
        ops.push(MemoryOp::EdgeAdd {
            edge: EdgeSpec {
                source_node_id: NodeId::from(edge.source_node_id.as_str()),
                relation: EdgeRelation::from_db_str(&edge.relation)?,
                target_repo_id: edge.target_repo_id.clone(),
                target_kind: edge.target_kind.clone(),
                target_anchor: edge.target_anchor.clone(),
                owner_repo_id: owner_repo_id.to_string(),
            },
        });
    }
    Ok(ops)
}

/// Author every one of the active repo's pre-existing memories into its owner stream — the one-time
/// full backfill that makes the op-log the complete signed history from genesis. Idempotent and
/// scope-gated:
/// - a NO-OP on an unscoped database (no active repo → no owner stream to root the log on);
/// - a NO-OP once the owner chain is non-empty: because [`crate::oplog::author_batch`] is atomic, a
///   non-empty chain is a COMPLETED backfill (no partial state to resume). The next increment's
///   "backfill before the first live author" ordering keeps this gate correct once live ops share
///   the chain.
///
/// Memories are ordered deterministically `(created_at_ms, memory_id)` — a reproducible Lamport
/// assignment. Each contributes NodeCreate, then a NodeStatus when non-active, then its EdgeAdds.
/// ALL statuses are backfilled (obsolete/rejected included): the log is the whole history.
pub(crate) fn backfill_memory_oplog(conn: &Connection, now_ms: i64) -> anyhow::Result<()> {
    let Some(repo_id) = memory_repo_scope(conn)? else {
        return Ok(());
    };
    // Only a STABLE repo id may root an IMMUTABLE owner stream. Two ids get re-pointed later, which
    // would strand a stream signed under the old id: the legacy `__unassigned__` placeholder (an
    // unadopted DB, re-pointed on adoption) and a machine-local `local:` shallow-clone id (upgraded
    // to a portable id when the clone is deepened). No-op until a stable id is active — as if
    // unscoped.
    if repo_id == crate::index::schema::LEGACY_REPO_ID
        || repo_id.starts_with(crate::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        return Ok(());
    }
    let stream = owner_stream(&repo_id)?;
    let device = local_device(conn, now_ms)?;
    // Cheap fast-path: skip opening the write txn when a chain already exists. NOT the correctness
    // gate — `author_genesis_in_tx` re-checks emptiness inside the txn below.
    if chain_tail(conn, stream, device.fingerprint())?.is_some() {
        return Ok(());
    }
    // ATOMIC snapshot: open the write txn FIRST — the `IMMEDIATE` lock blocks concurrent memory
    // writers — THEN read the memory/edge snapshot and author it, so a memory created between the
    // read and the batch cannot be silently omitted from the complete history (after which the
    // idempotency gate would hide it forever). The empty-chain gate is re-checked inside this txn.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut ops = Vec::new();
    for row in read_memory_rows(&tx, &repo_id)? {
        // `all_edges_from`, not `edges_from`: a backfilled obsolete/rejected memory still carries
        // its persisted edges into the complete-history log (the live reader hides them).
        let edges = all_edges_from(&tx, &row.memory_id)?;
        ops.extend(memory_to_ops(&row, &repo_id, &edges)?);
    }
    author_genesis_in_tx(&tx, stream, &device, &ops, now_ms)?;
    tx.commit()?;
    Ok(())
}

/// The active repo's memories in deterministic `(created_at_ms, memory_id)` order, tags attached.
fn read_memory_rows(conn: &Connection, repo_id: &str) -> anyhow::Result<Vec<MemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, title, body, confidence, status, source, payload_json
         FROM repo_memories WHERE repo_id = ?1 ORDER BY created_at_ms, id",
    )?;
    let mut rows = stmt
        .query_map(params![repo_id], |row| {
            Ok(MemoryRow {
                memory_id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                body: row.get(3)?,
                confidence: row.get(4)?,
                status: row.get(5)?,
                source: row.get(6)?,
                payload_json: row.get(7)?,
                tags: Vec::new(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for row in &mut rows {
        // Reuse the memory subsystem's own tag reader (the op encoder sorts + dedupes anyway).
        row.tags = tags_for_memory(conn, &row.memory_id)?;
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "repo-a";

    /// A DB with the memory schema, one registered repo, and the connection scoped to it — the
    /// minimal setup `memory_repo_scope` needs to resolve an active repo.
    fn scoped_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::index::schema::apply(&conn).unwrap();
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

    fn entry_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM oplog_entries", [], |r| r.get(0)).unwrap()
    }

    fn projected_node_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM oplog_projected_nodes", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn memory_to_ops_translates_content_status_and_edges() {
        let row = MemoryRow {
            memory_id: "mem_a".to_string(),
            kind: "Invariant".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            confidence: "high".to_string(),
            status: "obsolete".to_string(),
            source: "agent".to_string(),
            payload_json: None,
            tags: vec!["x".to_string()],
        };
        let edge = NodeEdge {
            edge_key: "k1".to_string(),
            source_node_id: "mem_a".to_string(),
            relation: "relates_to".to_string(),
            target_repo_id: REPO.to_string(),
            target_kind: "node".to_string(),
            target_anchor: "mem_b".to_string(),
            target_node_id: Some("mem_b".to_string()),
            anchor_status: "current".to_string(),
        };
        let ops = memory_to_ops(&row, REPO, std::slice::from_ref(&edge)).unwrap();
        // NodeCreate, then a NodeStatus (obsolete is not the active default), then the EdgeAdd.
        assert_eq!(ops.len(), 3);
        assert!(
            matches!(&ops[0], MemoryOp::NodeCreate { node_id, .. } if node_id.as_str() == "mem_a")
        );
        assert!(
            matches!(&ops[1], MemoryOp::NodeStatus { status, .. } if status.as_db_str() == "obsolete")
        );
        // An EdgeAdd, and DELIBERATELY no Rebind — the per-device resolved dimension
        // (target_node_id / anchor_status) is recomputed on read, never signed into the log.
        assert!(matches!(&ops[2], MemoryOp::EdgeAdd { .. }));
        assert!(
            !ops.iter().any(|op| matches!(op, MemoryOp::Rebind { .. })),
            "the backfill omits the per-device resolved dimension"
        );
    }

    #[test]
    fn an_active_memory_emits_no_status_op() {
        let row = MemoryRow {
            memory_id: "mem_a".to_string(),
            kind: "Invariant".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            confidence: "high".to_string(),
            status: "active".to_string(),
            source: "agent".to_string(),
            payload_json: None,
            tags: Vec::new(),
        };
        let ops = memory_to_ops(&row, REPO, &[]).unwrap();
        assert_eq!(ops.len(), 1, "an active, edgeless memory is just its NodeCreate");
    }

    #[test]
    fn memory_to_ops_fails_on_an_unknown_status() {
        // A status token this binary can't map must FAIL the backfill, not silently default the
        // signed history to `active`.
        let row = MemoryRow {
            memory_id: "mem_a".to_string(),
            kind: "Invariant".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            confidence: "high".to_string(),
            status: "future_status_from_a_newer_binary".to_string(),
            source: "agent".to_string(),
            payload_json: None,
            tags: Vec::new(),
        };
        let err = memory_to_ops(&row, REPO, &[]).unwrap_err();
        assert!(err.to_string().contains("unknown status"), "an unknown status fails the backfill");
    }

    #[test]
    fn backfill_authors_every_memory_and_is_idempotent() {
        let conn = scoped_conn();
        insert_memory(&conn, "mem_a", "active", 100);
        insert_memory(&conn, "mem_b", "active", 200);
        insert_memory(&conn, "mem_c", "active", 300);
        // Author a typed edge FROM mem_b (add_edge requires a live source), THEN mark mem_b
        // obsolete. The persisted edge must still be backfilled — the P2 regression guard: the
        // complete-history log keeps a non-live source's relationships (unlike the live reader).
        crate::query::memory::add_edge(
            &conn,
            "mem_b",
            EdgeRelation::RelatesTo,
            &crate::query::memory::EdgeTarget::Node { repo_id: None, node_id: "mem_c".to_string() },
        )
        .unwrap();
        conn.execute("UPDATE repo_memories SET status = 'obsolete' WHERE id = 'mem_b'", [])
            .unwrap();

        backfill_memory_oplog(&conn, 1_000).unwrap();
        // 3 NodeCreate + 1 NodeStatus (mem_b obsolete) + 1 EdgeAdd (from obsolete mem_b) = 5
        // entries; 3 projected nodes.
        assert_eq!(entry_count(&conn), 5);
        assert_eq!(projected_node_count(&conn), 3);
        let status: String = conn
            .query_row(
                "SELECT status FROM oplog_projected_nodes WHERE node_id = 'mem_b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "obsolete");
        let edges: i64 =
            conn.query_row("SELECT COUNT(*) FROM oplog_projected_edges", [], |r| r.get(0)).unwrap();
        assert_eq!(edges, 1);

        // A second backfill is a no-op — the atomic batch already completed (chain non-empty).
        backfill_memory_oplog(&conn, 2_000).unwrap();
        assert_eq!(entry_count(&conn), 5, "re-running backfill authors nothing more");
    }

    #[test]
    fn backfill_is_a_noop_on_the_placeholder_repo() {
        // An unadopted DB scoped to the legacy `__unassigned__` placeholder: backfilling would sign
        // an immutable owner stream that adoption can never re-point, so it must no-op even with
        // memories present.
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [crate::index::schema::LEGACY_REPO_ID],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES ('mem_a', 'Invariant', 'mem_a', 'body', 'high', 'active', 'agent', 1, 1,
                 'agent', 'h', 'v1', ?1)",
            [crate::index::schema::LEGACY_REPO_ID],
        )
        .unwrap();

        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(entry_count(&conn), 0, "the placeholder repo is not backfilled");
    }

    #[test]
    fn backfill_is_a_noop_on_a_local_only_repo() {
        // A machine-local `local:` shallow-clone id is upgraded to a portable id when the clone is
        // deepened, re-pointing the rows — so an immutable owner stream must not be rooted on it.
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        let local_id = format!("{}deadbeef", crate::repo_identity::LOCAL_ONLY_ID_PREFIX);
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [&local_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES ('mem_a', 'Invariant', 'mem_a', 'body', 'high', 'active', 'agent', 1, 1,
                 'agent', 'h', 'v1', ?1)",
            [&local_id],
        )
        .unwrap();

        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(entry_count(&conn), 0, "a local: repo is not backfilled");
    }

    #[test]
    fn backfill_is_a_noop_on_an_unscoped_db() {
        // No repos row, no connection scope → memory_repo_scope is None → nothing to root a stream.
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(entry_count(&conn), 0);
    }

    #[test]
    fn backfill_of_an_empty_repo_leaves_the_chain_empty() {
        let conn = scoped_conn();
        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(entry_count(&conn), 0, "no memories ⇒ no entries; the first live op is genesis");
    }
}
