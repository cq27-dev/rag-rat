//! Translating persisted memories into signed op-log entries, and the one-time full backfill of a
//! store's pre-existing memories into the log (#524).
//!
//! This bridges `repo_memories` / `repo_node_edges` (owned by this module) and the op-log MINTING
//! primitives ([`crate::oplog`]) — a ONE-WAY dependency, so `oplog` never depends back on the
//! memory subsystem (a reverse call would cycle the build).
//!
//! WIRED into the live write path (#532): the memory mutations call [`backfill_memory_oplog`] once
//! (before the first live entry) and the `author_*` seams below INSIDE their own transaction, so
//! the op-append and the table write commit — or roll back — together (strict-atomic). Authoring is
//! a NO-OP under an unstable scope ([`stable_owner_stream`]), leaving scope-less callers untouched.

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::hydrate::tags_for_memory;
use super::{EdgeRelation, NodeEdge, RepoMemory, all_edges_from, memory_repo_scope};
use crate::oplog::{
    EdgeKey, EdgeSpec, MemoryOp, NodeContent, NodeId, NodeStatus, StreamId, author_genesis_in_tx,
    author_in_tx, chain_tail, local_device, owner_stream,
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

/// Build the op model's content register from a memory's projectable columns — shared by the
/// backfill ([`memory_to_ops`]) and the live create/update authors, so all three agree
/// byte-for-byte.
fn node_content(
    kind: &str,
    title: &str,
    body: &str,
    confidence: &str,
    source: &str,
    tags: &[String],
    payload_json: Option<&str>,
) -> NodeContent {
    NodeContent {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        confidence: confidence.to_string(),
        source: source.to_string(),
        tags: tags.to_vec(),
        payload: payload_json.map(str::to_string),
    }
}

/// Translate one memory into its BACKFILL op sequence: a `NodeCreate` for content, a `NodeStatus`
/// ONLY when the status is not the fold's `active` create-time default, and an `EdgeAdd` per
/// outgoing typed node-edge (in `edge_key` order, so the authored chain is reproducible).
/// Code-anchor BINDINGS are excluded — per-device derived resolution state, never part of the
/// shared node graph.
fn memory_to_ops(
    row: &MemoryRow,
    owner_repo_id: &str,
    edges: &[NodeEdge],
) -> anyhow::Result<Vec<MemoryOp>> {
    let node_id = NodeId::from(row.memory_id.as_str());
    let mut ops = vec![MemoryOp::NodeCreate {
        node_id: node_id.clone(),
        content: node_content(
            &row.kind,
            &row.title,
            &row.body,
            &row.confidence,
            &row.source,
            &row.tags,
            row.payload_json.as_deref(),
        ),
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

/// The active repo's owner stream, but ONLY when the scope is a STABLE identity to root an
/// IMMUTABLE stream on — the SAME gate the backfill uses: `Some`, not the `__unassigned__`
/// placeholder, not a `local:` shallow-clone id (both get re-pointed later). `None` otherwise, and
/// the `author_*` seams SKIP authoring on `None`, so a scope-less mutation (most tests) never
/// touches the log.
fn stable_owner_stream(conn: &Connection) -> anyhow::Result<Option<StreamId>> {
    let Some(repo_id) = memory_repo_scope(conn)? else {
        return Ok(None);
    };
    if repo_id == crate::index::schema::LEGACY_REPO_ID
        || repo_id.starts_with(crate::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        return Ok(None);
    }
    Ok(Some(owner_stream(&repo_id)?))
}

/// Author `ops` onto the active repo's owner stream WITHIN the caller's mutation txn — the strict-
/// atomic live seam. Each op is minted + inserted + folded by `author_in_tx` (no open/commit), so
/// an authoring error propagates via `?` and the caller's txn rolls the table write back with it. A
/// NO-OP under an unstable scope. The caller MUST have run [`backfill_memory_oplog`] first (so the
/// pre-existing history precedes this live entry).
fn author_in_owner_stream(
    tx: &Transaction<'_>,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<()> {
    let Some(stream) = stable_owner_stream(tx)? else {
        return Ok(());
    };
    let device = local_device(tx, now_ms)?;
    for op in ops {
        author_in_tx(tx, stream, &device, op, now_ms)?;
    }
    Ok(())
}

/// Author a live memory CREATE (`NodeCreate`) inside the caller's mutation txn. A fresh memory has
/// no node-edges yet, so this is a single op.
pub(crate) fn author_create(
    tx: &Transaction<'_>,
    memory: &RepoMemory,
    now_ms: i64,
) -> anyhow::Result<()> {
    let op = MemoryOp::NodeCreate {
        node_id: NodeId::from(memory.memory_id.as_str()),
        content: content_of(memory),
    };
    author_in_owner_stream(tx, &[op], now_ms)
}

/// Author a live memory UPDATE inside the caller's mutation txn: a `NodeUpdate` ONLY when the
/// content actually changed, plus a `NodeStatus` ONLY when the status changed (even to `active`,
/// since the fold needs an explicit op to override a prior non-active status). Content and status
/// are INDEPENDENT LWW registers, so a status-only change must NOT emit a `NodeUpdate` — in a
/// synced multi-writer stream that lifecycle op would re-assert this device's content snapshot at a
/// new Lamport and could revert a concurrent body/title edit from another device. An unknown new
/// status token errors (the write path validates status first, so this is defensive).
pub(crate) fn author_update(
    tx: &Transaction<'_>,
    memory: &RepoMemory,
    content_changed: bool,
    status_changed: bool,
    now_ms: i64,
) -> anyhow::Result<()> {
    let node_id = NodeId::from(memory.memory_id.as_str());
    let mut ops = Vec::new();
    if content_changed {
        ops.push(MemoryOp::NodeUpdate { node_id: node_id.clone(), content: content_of(memory) });
    }
    if status_changed {
        let status = NodeStatus::from_db_str(&memory.status).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown status token `{}` (a newer binary must author this)",
                memory.status
            )
        })?;
        ops.push(MemoryOp::NodeStatus { node_id, status });
    }
    author_in_owner_stream(tx, &ops, now_ms)
}

/// Author a live edge ADD (`EdgeAdd`) inside the caller's mutation txn — presence + the durable
/// spec only (no `Rebind`; edge resolution is per-device, recomputed on read).
#[allow(clippy::too_many_arguments)]
pub(crate) fn author_edge_add(
    tx: &Transaction<'_>,
    source_node_id: &str,
    relation: EdgeRelation,
    target_repo_id: &str,
    target_kind: &str,
    target_anchor: &str,
    owner_repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    let op = MemoryOp::EdgeAdd {
        edge: EdgeSpec {
            source_node_id: NodeId::from(source_node_id),
            relation,
            target_repo_id: target_repo_id.to_string(),
            target_kind: target_kind.to_string(),
            target_anchor: target_anchor.to_string(),
            owner_repo_id: owner_repo_id.to_string(),
        },
    };
    author_in_owner_stream(tx, &[op], now_ms)
}

/// Author a live edge REMOVE (`EdgeRemove` tombstone) inside the caller's mutation txn.
pub(crate) fn author_edge_remove(
    tx: &Transaction<'_>,
    edge_key: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    author_in_owner_stream(
        tx,
        &[MemoryOp::EdgeRemove { edge_key: EdgeKey::from(edge_key) }],
        now_ms,
    )
}

/// The op-model content register for a persisted memory.
fn content_of(memory: &RepoMemory) -> NodeContent {
    node_content(
        &memory.kind,
        &memory.title,
        &memory.body,
        &memory.confidence,
        &memory.source,
        &memory.tags,
        memory.payload_json.as_deref(),
    )
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
        // Insert a typed edge FROM mem_b DIRECTLY (not via the now-live `add_edge`, which would
        // author it eagerly and defeat this isolation test), THEN mark mem_b obsolete — so the
        // explicit backfill below is the SOLE authoring path and must still capture the edge from a
        // now-non-live source (the complete-history guard: the live reader hides it).
        let ek = crate::query::memory::edge_key("mem_b", "relates_to", "node", "mem_c");
        conn.execute(
            "INSERT INTO repo_node_edges(
                 edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                 target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, ?2, 'mem_b', 'relates_to', ?2, 'node', 'mem_c', 'mem_c', 'current', 0)",
            params![ek, REPO],
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

    // --- live write-path wiring (#532) ---

    fn projected_edge_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM oplog_projected_edges", [], |r| r.get(0)).unwrap()
    }

    /// Create an unanchored `Concept` (needs no code binding) through the LIVE `create_memory`.
    fn create_concept(
        conn: &Connection,
        title: &str,
    ) -> anyhow::Result<crate::query::memory::RepoMemoryCreateResult> {
        crate::query::memory::create_memory(conn, crate::query::memory::RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: crate::query::memory::RepoMemoryBindTarget::default(),
        })
    }

    #[test]
    fn create_memory_authors_a_projected_node() {
        let conn = scoped_conn();
        let r = create_concept(&conn, "t1").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM oplog_projected_nodes WHERE node_id = ?1",
                [&r.memory.memory_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "the created memory is a projected node");
        assert_eq!(projected_node_count(&conn), 1);
    }

    #[test]
    fn a_scope_less_create_authors_nothing() {
        // No repos row / no active-repo context → the scope gate skips authoring entirely.
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        create_concept(&conn, "t1").unwrap();
        assert_eq!(entry_count(&conn), 0, "a scope-less create never touches the log");
    }

    #[test]
    fn update_memory_authors_node_update_and_a_status_change() {
        let conn = scoped_conn();
        let id = create_concept(&conn, "t1").unwrap().memory.memory_id;
        crate::query::memory::update_memory(&conn, crate::query::memory::RepoMemoryUpdate {
            memory_id: id.clone(),
            kind: None,
            title: None,
            body: Some("a new body".to_string()),
            confidence: None,
            status: Some("obsolete".to_string()),
            tags: None,
            payload_json: None,
        })
        .unwrap();
        let (content_json, status): (String, String) = conn
            .query_row(
                "SELECT content_json, status FROM oplog_projected_nodes WHERE node_id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(content_json.contains("a new body"), "the NodeUpdate replaced the content");
        assert_eq!(status, "obsolete", "the status change authored a NodeStatus");
    }

    #[test]
    fn mark_obsolete_authors_only_a_status_op_no_node_update() {
        let conn = scoped_conn();
        let id = create_concept(&conn, "t1").unwrap().memory.memory_id;
        // The NodeCreate is the sole entry so far.
        assert_eq!(entry_count(&conn), 1);
        crate::query::memory::mark_obsolete(&conn, &id).unwrap();
        let status: String = conn
            .query_row("SELECT status FROM oplog_projected_nodes WHERE node_id = ?1", [&id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "obsolete");
        // A status-only change authors EXACTLY ONE op (a NodeStatus) — NOT a NodeUpdate, which in a
        // synced stream could revert a concurrent content edit (content/status are independent
        // LWW).
        assert_eq!(
            entry_count(&conn),
            2,
            "status-only update authors one NodeStatus, no NodeUpdate"
        );
    }

    #[test]
    fn a_no_op_update_authors_nothing() {
        let conn = scoped_conn();
        let id = create_concept(&conn, "t1").unwrap().memory.memory_id;
        assert_eq!(entry_count(&conn), 1);
        // An update that changes neither content nor status is a complete no-op in the log.
        crate::query::memory::update_memory(&conn, crate::query::memory::RepoMemoryUpdate {
            memory_id: id,
            kind: None,
            title: None,
            body: None,
            confidence: None,
            status: None,
            tags: None,
            payload_json: None,
        })
        .unwrap();
        assert_eq!(entry_count(&conn), 1, "a change-free update authors no op");
    }

    #[test]
    fn add_and_remove_edge_author_edge_presence() {
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let b = create_concept(&conn, "b").unwrap().memory.memory_id;
        let edge = crate::query::memory::add_edge(&conn, &a, EdgeRelation::RelatesTo, &{
            crate::query::memory::EdgeTarget::Node { repo_id: None, node_id: b }
        })
        .unwrap();
        assert_eq!(projected_edge_count(&conn), 1, "add_edge authored an EdgeAdd");
        assert!(crate::query::memory::remove_edge(&conn, &edge.edge_key).unwrap());
        assert_eq!(projected_edge_count(&conn), 0, "remove_edge authored an EdgeRemove tombstone");
    }

    #[test]
    fn a_failed_author_rolls_back_the_memory_write() {
        let conn = scoped_conn();
        // One good create so the owner chain is non-empty (the second create's backfill fast-paths,
        // isolating the failure to the live author).
        create_concept(&conn, "first").unwrap();
        let before: i64 =
            conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
        // Poison: pretend a NEWER projector owns the store — `author_in_tx`'s
        // `assert_projector_not_newer` now errors, so the second create's op-append fails.
        conn.execute("UPDATE oplog_meta SET value = '999' WHERE key = 'projector_version'", [])
            .unwrap();
        assert!(create_concept(&conn, "second").is_err(), "the authoring failure fails the create");
        let after: i64 =
            conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
        assert_eq!(after, before, "strict-atomic: the failed create's row rolled back with it");
    }

    #[test]
    fn a_live_create_backfills_pre_existing_memories_first() {
        let conn = scoped_conn();
        // A memory inserted by RAW SQL (never authored) — the pre-existing history.
        insert_memory(&conn, "old", "active", 100);
        // The first LIVE create backfills `old` (a NodeCreate) BEFORE authoring the new memory.
        let new_id = create_concept(&conn, "new").unwrap().memory.memory_id;
        assert_eq!(
            projected_node_count(&conn),
            2,
            "old (backfilled) + new (live) are both projected"
        );
        let old_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM oplog_projected_nodes WHERE node_id = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_present, 1, "the pre-existing memory was backfilled");
        assert_ne!(new_id, "old");
    }

    /// Create an unanchored `Concept` with tags through the LIVE `create_memory`.
    fn create_concept_tagged(conn: &Connection, title: &str, tags: Vec<String>) -> String {
        crate::query::memory::create_memory(conn, crate::query::memory::RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags,
            payload_json: None,
            bind: crate::query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap()
        .memory
        .memory_id
    }

    fn update_tags(conn: &Connection, id: &str, tags: Vec<String>) {
        crate::query::memory::update_memory(conn, crate::query::memory::RepoMemoryUpdate {
            memory_id: id.to_string(),
            kind: None,
            title: None,
            body: None,
            confidence: None,
            status: None,
            tags: Some(tags),
            payload_json: None,
        })
        .unwrap();
    }

    #[test]
    fn a_normalization_only_tag_change_authors_nothing() {
        let conn = scoped_conn();
        let id = create_concept_tagged(&conn, "t", vec!["x".to_string()]);
        let before = entry_count(&conn);
        // Tags that normalize to the SAME set: trailing space, duplicate, and an empty string.
        update_tags(&conn, &id, vec!["x ".to_string(), "x".to_string(), String::new()]);
        assert_eq!(
            entry_count(&conn),
            before,
            "a whitespace/duplicate-only re-tag is not a content change → no NodeUpdate"
        );
    }

    #[test]
    fn a_real_tag_change_authors_a_node_update() {
        let conn = scoped_conn();
        let id = create_concept_tagged(&conn, "t", vec!["x".to_string()]);
        let before = entry_count(&conn);
        update_tags(&conn, &id, vec!["x".to_string(), "y".to_string()]);
        assert_eq!(entry_count(&conn), before + 1, "adding a real tag authors a NodeUpdate");
    }

    #[test]
    fn re_adding_an_edge_authors_no_duplicate() {
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let b = create_concept(&conn, "b").unwrap().memory.memory_id;
        let target = |node: &str| crate::query::memory::EdgeTarget::Node {
            repo_id: None,
            node_id: node.to_string(),
        };
        crate::query::memory::add_edge(&conn, &a, EdgeRelation::RelatesTo, &target(&b)).unwrap();
        let after_first = entry_count(&conn);
        assert_eq!(projected_edge_count(&conn), 1);
        // Re-adding the SAME edge is an idempotent resolution-refresh — it must NOT author a second
        // EdgeAdd (which could resurrect a concurrent remove under sync).
        crate::query::memory::add_edge(&conn, &a, EdgeRelation::RelatesTo, &target(&b)).unwrap();
        assert_eq!(
            entry_count(&conn),
            after_first,
            "an idempotent edge re-add authors no duplicate EdgeAdd"
        );
        assert_eq!(projected_edge_count(&conn), 1);
    }
}
