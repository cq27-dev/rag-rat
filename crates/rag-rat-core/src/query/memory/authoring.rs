//! Translating persisted memories into signed op-log entries, and the per-node/edge reconcile that
//! keeps the log a COMPLETE signed mirror of `repo_memories` / `repo_node_edges` (#524, #541,
//! #664).
//!
//! This bridges `repo_memories` / `repo_node_edges` (owned by this module) and the op-log MINTING
//! primitives ([`crate::oplog`]) — a ONE-WAY dependency, so `oplog` never depends back on the
//! memory subsystem (a reverse call would cycle the build).
//!
//! OWNER-BOUND `/2`//3 substrate (#664). The live path authors owner-bound `/3` content on the
//! repo's owner-bound `/2` stream, under the store's single local account (minted once, store-
//! global). Each reconcile/mutation ensures the repo's `/2` stream is owned (publishing a
//! `StreamOwn` account op) and authors its ops as owner-authored `/3` content
//! ([`crate::oplog::author_content_batch_in_tx`]), which verify-accepts and reprojects into
//! `content_projected_nodes` / `content_projected_edges`. The completeness predicate is an
//! anti-join against that accepted-`/3` projection; the pre-existing `/1` history is retained but
//! no longer written by the live path (existing `/1` rows are adopted into `/3` by the reconcile).
//!
//! WIRED into the live write path (#532): the memory mutations call [`backfill_memory_oplog`] once
//! (before the first live entry) and the `author_*` seams below INSIDE their own transaction, so
//! the op-append and the table write commit — or roll back — together (strict-atomic). Authoring is
//! a NO-OP under an unstable scope ([`stable_owner_stream`]) or before the local account is minted,
//! leaving scope-less callers untouched.
//!
//! [`backfill_memory_oplog`] is a per-node/edge RECONCILE (#541), not a per-chain gate: it authors
//! every table row MISSING from the accepted-`/3` projection, so a row that entered the tables
//! outside the wired path (a pre-#532 binary, a raw writer, a consolidation import, or pre-existing
//! `/1` history) is signed on the next mutation and no later lifecycle op on it is ever inert.
//! Genesis is just the empty-`/3`-chain case where every row is missing.

use anyhow::Context;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::hydrate::tags_for_memory;

/// Scoped durability bump for an AUTHORED write (#560). The index connection runs
/// `synchronous = NORMAL` — the right policy for the high-frequency, fully reconstructable
/// derived-index writes, where skipping the per-commit WAL fsync is a throughput win and the only
/// cost is that the last committed transaction can roll back on power loss (a re-index recovers
/// it).
///
/// Authored memory / op-log mutations are the OPPOSITE class: irreplaceable, low-frequency, and
/// they return success to the caller. They must not acknowledge under a mode that can silently lose
/// the last commit, so they raise `synchronous = FULL` (fsync the WAL on commit) for the duration
/// of their transaction and restore `NORMAL` on drop. The guard is held ACROSS the authored
/// `BEGIN .. COMMIT` and dropped after, so the commit fsyncs; restore runs on every path (including
/// error/panic), so a shared connection is never stranded at FULL — and a stray failure could only
/// leave it on the *safer*, slower setting, never a less durable one.
pub(super) struct AuthoredDurability<'a> {
    conn: &'a Connection,
}

impl<'a> AuthoredDurability<'a> {
    /// Raise `synchronous = FULL`. MUST be called OUTSIDE a transaction (SQLite only applies a
    /// `synchronous` change to subsequent transactions), i.e. immediately before the authored
    /// `BEGIN`/`unchecked_transaction`.
    pub(super) fn begin(conn: &'a Connection) -> anyhow::Result<Self> {
        conn.execute_batch("PRAGMA synchronous = FULL;")?;
        Ok(Self { conn })
    }
}

impl Drop for AuthoredDurability<'_> {
    fn drop(&mut self) {
        // Best-effort restore of the connection default (see the struct doc for why swallowing is
        // safe). Runs after the authored txn has committed/rolled back, so no transaction is open.
        let _ = self.conn.execute_batch("PRAGMA synchronous = NORMAL;");
    }
}
use super::{EdgeRelation, NodeEdge, RepoMemory, memory_repo_scope};
use crate::oplog::{EdgeKey, EdgeSpec, MemoryOp, NodeContent, NodeId, NodeStatus, StreamId};

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
/// reconcile ([`node_ops`]) and the live create/update authors, so all three agree byte-for-byte.
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

/// A memory's NODE ops: a `NodeCreate` for content, then a `NodeStatus`.
///
/// `elide_active_status = true` (GENESIS on an empty chain, no stale registers) emits the status op
/// ONLY when non-active — the fold's create-time default handles `active`, so genesis stays
/// byte-identical to the pre-#541 backfill. `false` (INCREMENTAL heal on a non-empty chain) ALWAYS
/// emits `NodeStatus`, so a healed node's status wins its register at the new, higher Lamport even
/// if an inert `NodeStatus` from an old binary left a stale value in it (the fold's status register
/// is independent of existence — a `NodeCreate` never touches it — so authoring only the create
/// would let that stale register surface; see decision 6 of #541).
///
/// An unrecognized status token FAILS — a signed op cannot be corrected, and coercing to `active`
/// would permanently mint the wrong status into the immutable history. Code-anchor BINDINGS are
/// excluded — per-device derived resolution state, never part of the shared node graph.
fn node_ops(row: &MemoryRow, elide_active_status: bool) -> anyhow::Result<Vec<MemoryOp>> {
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
    let is_active = row.status == NodeStatus::default().as_db_str();
    if !(elide_active_status && is_active) {
        let status = NodeStatus::from_db_str(&row.status).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot author memory `{}`: unknown status token `{}` (a newer binary must author \
                 this history)",
                row.memory_id,
                row.status
            )
        })?;
        ops.push(MemoryOp::NodeStatus { node_id, status });
    }
    Ok(ops)
}

/// One `EdgeAdd` — presence + the durable, RE-RESOLVED spec only. `edge.target_repo_id` is already
/// current: `unauthored_edges`'s `reresolve_on_read` repaired the add-time snapshot before the
/// reconcile signs it (a signed op cannot be corrected later). Deliberately NO `Rebind`: the
/// `Rebind` op's resolved dimension (`target_node_id`, `anchor_status`) is PER-DEVICE derived state
/// recomputed on every read by `reresolve_on_read`, so signing it would bake one device's view into
/// the immutable shared history — excluded for the same reason code-anchor BINDINGS are.
fn edge_add_op(edge: &NodeEdge, owner_repo_id: &str) -> anyhow::Result<MemoryOp> {
    Ok(MemoryOp::EdgeAdd {
        edge: EdgeSpec {
            source_node_id: NodeId::from(edge.source_node_id.as_str()),
            relation: EdgeRelation::from_db_str(&edge.relation)?,
            target_repo_id: edge.target_repo_id.clone(),
            target_kind: edge.target_kind.clone(),
            target_anchor: edge.target_anchor.clone(),
            owner_repo_id: owner_repo_id.to_string(),
        },
    })
}

/// The ordered reconcile batch. Missing edges are grouped by `source_node_id`; for each missing
/// memory in `(created_at_ms, id)` order it emits [`node_ops`] then that memory's missing edges (in
/// the `edge_key` order [`unauthored_edges`] returned), then a final pass for edges whose source is
/// an already-authored node. On an EMPTY projection with `elide_active_status = true` this is
/// byte-identical to today's genesis sequence: every source memory is missing (`FK ON DELETE
/// CASCADE` on `source_node_id` rules out an orphan edge), so the final pass is empty and each
/// memory's edges follow its `NodeCreate`/`NodeStatus` in `edge_key` order.
fn build_reconcile_ops(
    missing_nodes: &[MemoryRow],
    missing_edges: &[NodeEdge],
    owner_repo_id: &str,
    elide_active_status: bool,
) -> anyhow::Result<Vec<MemoryOp>> {
    use std::collections::BTreeMap;
    let mut by_source: BTreeMap<&str, Vec<&NodeEdge>> = BTreeMap::new();
    for edge in missing_edges {
        by_source.entry(edge.source_node_id.as_str()).or_default().push(edge);
    }
    let mut ops = Vec::new();
    for row in missing_nodes {
        ops.extend(node_ops(row, elide_active_status)?);
        if let Some(group) = by_source.remove(row.memory_id.as_str()) {
            for edge in group {
                ops.push(edge_add_op(edge, owner_repo_id)?);
            }
        }
    }
    // Lone ghost edges whose source node was already authored (absent on a genesis projection).
    for (_source, group) in by_source {
        for edge in group {
            ops.push(edge_add_op(edge, owner_repo_id)?);
        }
    }
    Ok(ops)
}

/// Reconcile the repo's owner-bound `/2` stream against its tables: establish ownership (mint the
/// local account + publish a `StreamOwn` if needed) and author every `repo_memories` /
/// `repo_node_edges` row MISSING from the accepted-`/3` projection as owner-authored `/3` content.
/// Genesis (empty `/3` chain) authors the full history; a populated chain authors only the ghosts.
/// Idempotent and scope-gated (LEGACY / `local:` ids never root an immutable stream).
/// Scope-EXPLICIT — `repo_id` is passed, and the readers + re-resolution are scope-independent, so
/// the consolidation importer's unscoped connection can call it. Concurrency: two racing callers
/// serialize on the IMMEDIATE lock; the loser re-reads under the lock and authors only what the
/// winner left missing.
fn sync_owner_stream(conn: &Connection, repo_id: &str, now_ms: i64) -> anyhow::Result<()> {
    // Only a STABLE id may root an IMMUTABLE owner stream. Two ids get re-pointed later, which
    // would strand a stream signed under the old id: the legacy `__unassigned__` placeholder
    // (an unadopted DB, re-pointed on adoption) and a machine-local `local:` shallow-clone id
    // (upgraded to a portable id when the clone is deepened). No-op until a stable id is active
    // — as if unscoped.
    if repo_id == crate::index::schema::LEGACY_REPO_ID
        || repo_id.starts_with(crate::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        return Ok(());
    }
    // Cheap autocommit fast-path: return WITHOUT the write lock only when the `/3` log is fully
    // ESTABLISHED (the local account is minted AND its `StreamOwn` folded effective) AND nothing is
    // missing. `established_owned_stream_v2` returns `None` for a fresh repo that has never
    // published ownership — even one with an empty anti-join — so such a repo FALLS THROUGH to
    // the slow path to mint + publish ownership rather than early-returning on an empty set it
    // could not yet author into. `established_owned_stream_v2` is autocommit-only (it opens its
    // own read txn), so it runs here, not under the write lock.
    if let Some(stream) = crate::oplog::established_owned_stream_v2(conn, repo_id)?
        && read_unauthored_memory_rows(conn, repo_id, stream)?.is_empty()
        && crate::query::memory::edges::unauthored_edges(conn, repo_id, stream)?.is_empty()
    {
        return Ok(());
    }

    // Slow path. Mint the store's local account BEFORE the durability guard: the mint
    // self-transacts and holds its OWN durability guard that restores `synchronous = NORMAL` on
    // drop — beginning our guard first would let the mint's drop downgrade our authored commit
    // below NORMAL, silently losing the #560 durability. The mint is store-global and
    // idempotent (a re-mint returns the same account).
    crate::oplog::local_account(conn, now_ms)?;
    // Authored, irreplaceable data → durable (#560). FULL for this txn only, restored on drop; set
    // OUTSIDE the txn (SQLite applies a `synchronous` change to SUBSEQUENT transactions only).
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Publish ownership of the repo's `/2` stream and resolve its id. Idempotent +
    // check-fact-first: a re-ensure (or a racer serialized on this IMMEDIATE lock) authors no
    // second `StreamOwn`.
    let stream = crate::oplog::ensure_owned_stream_v2_in_tx(&tx, repo_id, now_ms)?;
    // Authoritative re-read UNDER the write lock (TOCTOU): a concurrent author may have healed or
    // added rows between the probe and the lock, so re-read the missing set and re-derive `genesis`
    // here. `genesis` decides the status-elision: an empty `/3` content chain ⇒ no stale registers
    // ⇒ elide `active` (byte-identical to the pre-#541 genesis). This equivalence holds ONLY
    // under the single-local-writer owner stream (see the module header); phase D (foreign
    // devices can populate the stream) must revisit whether local-chain-empty still implies
    // register-clean.
    let genesis = crate::oplog::content_stream_is_empty(&tx, stream)?;
    let missing_nodes = read_unauthored_memory_rows(&tx, repo_id, stream)?;
    let missing_edges = crate::query::memory::edges::unauthored_edges(&tx, repo_id, stream)?;
    let ops = build_reconcile_ops(&missing_nodes, &missing_edges, repo_id, genesis)?;
    // Skip the author when there is nothing to author: a fresh repo whose anti-join was empty only
    // needed ownership established (done above), and authoring an empty batch would still refold +
    // reproject for no change.
    if !ops.is_empty() {
        // Fail-loud on an oversized envelope (a raw/adversarial memory body over the §18a 256 KiB
        // cap): the `/3` author `bail!`s rather than dropping irreplaceable data, and we surface
        // the failure with the repo it belongs to instead of wedging the store silently. A
        // normal rag-rat memory (body ≤ 8 KiB) can never hit this; it is an
        // adversarial/raw-writer backstop.
        crate::oplog::author_content_batch_in_tx(&tx, stream, &ops, now_ms).with_context(|| {
            format!(
                "reconciling the /3 owner log for repo `{repo_id}` failed while authoring {} \
                 pre-existing memory op(s)",
                ops.len()
            )
        })?;
    }
    tx.commit()?;
    Ok(())
}

/// Reconcile the ACTIVE repo's owner stream (scope read from the connection) — the idempotent call
/// every live memory/edge mutation makes before authoring (#532), now self-healing per node/edge (a
/// ghost row is authored on the next mutation, so no later lifecycle op on it is inert). A no-op on
/// an unscoped DB.
pub(crate) fn backfill_memory_oplog(conn: &Connection, now_ms: i64) -> anyhow::Result<()> {
    let Some(repo_id) = memory_repo_scope(conn)? else {
        return Ok(());
    };
    sync_owner_stream(conn, &repo_id, now_ms)
}

/// Reconcile a SPECIFIC repo's owner stream independent of connection scope — the seam
/// consolidation uses to author freshly-imported (remapped) rows into the TARGET's owner stream
/// under the TARGET's identity (#541). The source's pre-remap signed entries are intentionally NOT
/// carried (they are signed under the source device over pre-remap ids). Wired into consolidation
/// by [`crate::index::consolidate::run`] (#541 Task 5), immediately after the import commits and
/// before the legacy file is renamed away.
pub(crate) fn reconcile_owner_stream_for_repo(
    conn: &Connection,
    repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    sync_owner_stream(conn, repo_id, now_ms)
}

/// The active repo's owner-bound `/2` stream, but ONLY when the scope is a STABLE identity to root
/// an IMMUTABLE stream on — the SAME gate the backfill uses: `Some`, not the `__unassigned__`
/// placeholder, not a `local:` shallow-clone id (both get re-pointed later) — AND the store's local
/// account is minted (the `/2` id is derived under it). `None` otherwise, and the `author_*` seams
/// SKIP authoring on `None`, so a scope-less mutation (most tests) or a store whose account is not
/// yet minted never touches the log. Derivation-only (no fact check), so it is safe inside the
/// caller's open txn — unlike the reconcile's autocommit `established_owned_stream_v2` probe.
fn stable_owner_stream(conn: &Connection) -> anyhow::Result<Option<StreamId>> {
    let Some(repo_id) = memory_repo_scope(conn)? else {
        return Ok(None);
    };
    if repo_id == crate::index::schema::LEGACY_REPO_ID
        || repo_id.starts_with(crate::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        return Ok(None);
    }
    crate::oplog::owned_stream_v2_id(conn, &repo_id)
}

/// Author `ops` as owner-authored `/3` content on the active repo's owner-bound `/2` stream WITHIN
/// the caller's mutation txn — the strict-atomic live seam.
/// [`crate::oplog::author_content_batch_in_tx`] mints, inserts, refolds, and verify-accepts the
/// batch (no open/commit), so an authoring error propagates via `?` and the caller's txn rolls the
/// table write back with it. A NO-OP under an unstable scope or before the local account is minted.
/// The caller MUST have run `backfill_memory_oplog` first, so the store's account plus `StreamOwn`
/// are established (else the batch's verify-accepted rolls back) and the pre-existing history
/// precedes this live entry.
fn author_in_owner_stream(
    tx: &Transaction<'_>,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<()> {
    let Some(stream) = stable_owner_stream(tx)? else {
        return Ok(());
    };
    // Skip a no-op mutation: an empty batch would still refold + reproject the whole stream for no
    // change. (The four live seams only reach here with non-empty ops today, but the guard keeps a
    // change-free `author_update` from doing O(chain) work.)
    if ops.is_empty() {
        return Ok(());
    }
    crate::oplog::author_content_batch_in_tx(tx, stream, ops, now_ms)?;
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

/// The repo's memories with NO projected node on `stream` in the accepted-`/3` projection — the
/// rows the signed log is MISSING — in deterministic `(created_at_ms, id)` order, tags attached. On
/// an EMPTY projection this is every memory (genesis); on a populated one, the ghosts a raw writer,
/// an old binary, or pre-existing `/1` history left behind (#541, #664). Reuses the memory
/// subsystem's own tag reader (the op encoder sorts + dedupes anyway).
fn read_unauthored_memory_rows(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<Vec<MemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.kind, m.title, m.body, m.confidence, m.status, m.source, m.payload_json
         FROM repo_memories m
         WHERE m.repo_id = ?1
           AND NOT EXISTS (
                 SELECT 1 FROM content_projected_nodes p
                 WHERE p.stream_id = ?2 AND p.node_id = m.id)
         ORDER BY m.created_at_ms, m.id",
    )?;
    let mut rows = stmt
        .query_map(params![repo_id, stream.to_bytes().as_slice()], |row| {
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

    /// Insert an active memory with a CUSTOM body — used to plant an adversarial oversized body
    /// that a normal rag-rat memory (body ≤ 8 KiB) could never carry.
    fn insert_memory_with_body(conn: &Connection, id: &str, body: &str) {
        conn.execute(
            "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES (?1, 'Invariant', ?1, ?2, 'high', 'active', 'agent', 100, 100, 'agent', 'h',
                 'v1', ?3)",
            params![id, body, REPO],
        )
        .unwrap();
    }

    /// Point a freshly-opened connection at `repo` via the per-connection TEMP `connection_context`
    /// (temp tables are connection-local, so each thread of the race test scopes its own).
    fn set_scope(conn: &Connection, repo: &str) {
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [repo],
        )
        .unwrap();
    }

    /// The projected `/3` status of `node_id` on the store's owner stream — the completeness mirror
    /// the reconcile heals into. A test DB holds one repo/stream, so no stream filter is needed;
    /// panics if the node is not projected (callers assert presence).
    fn projected_node_status(conn: &Connection, node_id: &str) -> String {
        conn.query_row(
            "SELECT status FROM content_projected_nodes WHERE node_id = ?1",
            [node_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Insert a node-edge by RAW SQL, bypassing the wired `add_edge` author — a "ghost edge" that
    /// exists in `repo_node_edges` but was never signed into the op-log.
    fn insert_raw_node_edge(conn: &Connection, source: &str, relation: &str, target: &str) {
        let key = crate::query::memory::edge_key(source, relation, "node", target);
        conn.execute(
            "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
             target_repo_id,
                 target_kind, target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?2, 'node', ?5, ?5, 'current', 100)",
            params![key, REPO, source, relation, target],
        )
        .unwrap();
    }

    /// Author a BARE `/3` NodeStatus for a node with NO `NodeCreate` — the `/3` analog of the INERT
    /// op a pre-fix (#532) binary authored when it `mark_obsolete`'d a still-ghost memory. The
    /// shared projector emits no node without content, so this leaves a stale status register
    /// with no projected node. Requires the store's account + owner stream already established
    /// (via a prior live create); it authors through the real `/3` seam so the register truly
    /// lands.
    fn author_inert_status_op(conn: &Connection, node_id: &str, status: NodeStatus) {
        let stream = crate::oplog::owned_stream_v2_id(conn, REPO)
            .unwrap()
            .expect("account minted by a prior live create");
        let tx = conn.unchecked_transaction().unwrap();
        crate::oplog::author_content_batch_in_tx(
            &tx,
            stream,
            &[MemoryOp::NodeStatus { node_id: NodeId::from(node_id), status }],
            100,
        )
        .unwrap();
        tx.commit().unwrap();
    }

    /// The `/3` content chain length — one entry per authored op, the retarget's signed op-log
    /// (account genesis + `StreamOwn` live in `account_entries`, not counted here).
    fn entry_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM content_entries", [], |r| r.get(0)).unwrap()
    }

    fn projected_node_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM content_projected_nodes", [], |r| r.get(0)).unwrap()
    }

    /// The number of local accounts minted (the single-row pointer table): 0 before any established
    /// reconcile, 1 after — proves a no-op / scope-gated path mints NO account.
    fn local_account_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM oplog_local_account", [], |r| r.get(0)).unwrap()
    }

    /// The number of folded `StreamOwn` ownership facts — one per owned `/2` stream.
    fn owned_stream_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM account_stream_ownership", [], |r| r.get(0)).unwrap()
    }

    /// #560 durability split: an authored write commits under `synchronous = FULL`, and the guard
    /// restores the connection's `NORMAL` default on drop so derived-index writes are unaffected.
    /// Uses a file-backed index connection (WAL + NORMAL, like every real open) because an
    /// in-memory database ignores the `synchronous` setting and would not report the change.
    #[test]
    fn authored_durability_raises_full_then_restores_normal() {
        let dir = std::env::temp_dir().join(format!("ragrat-authdur-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let storage = crate::storage::IndexConnection::open(&dir.join("index.db")).unwrap();
        let conn = storage.connection();
        let synchronous = |c: &Connection| -> i64 {
            c.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap()
        };

        assert_eq!(synchronous(conn), 1, "an index connection defaults to synchronous=NORMAL (=1)");
        {
            let _durability = AuthoredDurability::begin(conn).unwrap();
            assert_eq!(
                synchronous(conn),
                2,
                "an authored write must raise synchronous=FULL (=2) for its commit"
            );
        }
        assert_eq!(
            synchronous(conn),
            1,
            "the authored-durability guard must restore synchronous=NORMAL (=1) on drop"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A `MemoryRow` with the given status, no payload, one tag — the fixture the ported op-split
    /// tests translate.
    fn op_row(status: &str) -> MemoryRow {
        MemoryRow {
            memory_id: "mem_a".to_string(),
            kind: "Invariant".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            confidence: "high".to_string(),
            status: status.to_string(),
            source: "agent".to_string(),
            payload_json: None,
            tags: vec!["x".to_string()],
        }
    }

    #[test]
    fn node_ops_and_edge_add_op_translate_content_status_and_an_edge() {
        // GENESIS (elide=true) on a non-active memory: NodeCreate then a NodeStatus (obsolete is
        // not the active default). `edge_add_op` yields one EdgeAdd and DELIBERATELY no
        // Rebind — the per-device resolved dimension (target_node_id / anchor_status) is
        // recomputed on read, never signed into the log.
        let ops = node_ops(&op_row("obsolete"), true).unwrap();
        assert_eq!(ops.len(), 2);
        assert!(
            matches!(&ops[0], MemoryOp::NodeCreate { node_id, .. } if node_id.as_str() == "mem_a")
        );
        assert!(
            matches!(&ops[1], MemoryOp::NodeStatus { status, .. } if status.as_db_str() == "obsolete")
        );
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
        let edge_op = edge_add_op(&edge, REPO).unwrap();
        assert!(matches!(&edge_op, MemoryOp::EdgeAdd { .. }));
        let all_ops: Vec<MemoryOp> = ops.into_iter().chain(std::iter::once(edge_op)).collect();
        assert!(
            !all_ops.iter().any(|op| matches!(op, MemoryOp::Rebind { .. })),
            "the reconcile omits the per-device resolved dimension"
        );
    }

    #[test]
    fn genesis_node_ops_for_an_active_memory_emit_no_status_op() {
        // elide=true (genesis, no stale registers): an active, edgeless memory is just its
        // NodeCreate — the fold's create-time default handles `active`.
        let ops = node_ops(&op_row("active"), true).unwrap();
        assert_eq!(ops.len(), 1, "an active memory on genesis is just its NodeCreate");
        assert!(matches!(&ops[0], MemoryOp::NodeCreate { .. }));
    }

    #[test]
    fn incremental_node_ops_for_an_active_memory_do_emit_an_explicit_status() {
        // elide=false (incremental heal on a non-empty chain): ALWAYS emit NodeStatus, even
        // `active`, so a healed node's status wins its register at the new Lamport and overrides
        // any stale register a prior inert op left behind (decision 6 of #541).
        let ops = node_ops(&op_row("active"), false).unwrap();
        assert_eq!(ops.len(), 2, "an active memory on a heal emits an explicit NodeStatus");
        assert!(matches!(&ops[0], MemoryOp::NodeCreate { .. }));
        assert!(
            matches!(&ops[1], MemoryOp::NodeStatus { status, .. } if status.as_db_str() == "active"),
            "the incremental branch emits NodeStatus{{active}} to win the register"
        );
    }

    #[test]
    fn node_ops_fails_on_an_unknown_status() {
        // A status token this binary can't map must FAIL, not silently default the signed history
        // to `active`. Holds in either branch — an unknown token is never the active
        // default.
        let err = node_ops(&op_row("future_status_from_a_newer_binary"), true).unwrap_err();
        assert!(err.to_string().contains("unknown status"), "an unknown status fails authoring");
    }

    /// #541/#664: the reconcile's memory reader anti-joins `repo_memories` against the accepted-`/3`
    /// projection `content_projected_nodes` — only a row with no projected node (never signed)
    /// comes back. `stream` is an opaque `StreamId` here (the anti-join only needs seed/query
    /// agreement).
    #[test]
    fn read_unauthored_memory_rows_returns_only_rows_absent_from_the_projection() {
        let conn = scoped_conn();
        insert_memory(&conn, "mem_live", "active", 100);
        insert_memory(&conn, "mem_ghost", "active", 200);
        let stream = StreamId::from_bytes([0x11; 32]);
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_live', '{}', 'active')",
            params![stream.to_bytes().as_slice()],
        )
        .unwrap();
        let missing = read_unauthored_memory_rows(&conn, REPO, stream).unwrap();
        assert_eq!(missing.iter().map(|r| r.memory_id.as_str()).collect::<Vec<_>>(), ["mem_ghost"]);
    }

    // --- the per-node/edge self-healing reconcile (#541) ---

    #[test]
    fn a_ghost_memory_is_authored_on_the_next_reconcile() {
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap(); // roots the chain via genesis
        insert_memory(&conn, "mem_ghost", "obsolete", 500); // raw, un-authored ghost
        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert_eq!(
            projected_node_status(&conn, "mem_ghost"),
            NodeStatus::Obsolete.as_db_str(),
            "the ghost is now authored with its create-time status",
        );
    }

    #[test]
    fn heal_overrides_a_stale_status_register_left_by_an_inert_op() {
        // The decision-6 divergence: an inert NodeStatus{obsolete} exists, the row is now active,
        // the heal must author an explicit NodeStatus{active} so the projection matches the table.
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap(); // establishes the account + owner stream
        // Author an inert `/3` NodeStatus for a NOT-yet-created ghost (the /3 analog of the old
        // binary's inert op): a bare status register with no projected node.
        let ghost = "mem_ghost";
        author_inert_status_op(&conn, ghost, NodeStatus::Obsolete);
        insert_memory(&conn, ghost, "active", 500); // the table says active
        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert_eq!(
            projected_node_status(&conn, ghost),
            NodeStatus::Active.as_db_str(),
            "explicit NodeStatus{{active}} overrode the stale register",
        );
    }

    #[test]
    fn a_ghost_edge_on_a_live_node_is_authored_on_the_next_reconcile() {
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let b = create_concept(&conn, "b").unwrap().memory.memory_id;
        insert_raw_node_edge(&conn, &a, "relates_to", &b); // writes repo_node_edges directly
        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert_eq!(projected_edge_count(&conn), 1, "ghost edge now signed");
    }

    #[test]
    fn reconcile_is_idempotent_and_a_clean_repo_authors_nothing() {
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap();
        let before = entry_count(&conn);
        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert_eq!(entry_count(&conn), before, "no ghost → no new /3 entry");
    }

    #[test]
    fn an_unreadable_status_ghost_fails_the_mutation_path_loudly() {
        // Blast-radius pin: a ghost carrying a status token THIS binary cannot decode makes the
        // whole reconcile (hence the mutation that triggered it) fail, rather than silently minting
        // `active`.
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap();
        insert_memory(&conn, "mem_future", "some_future_status", 500);
        assert!(backfill_memory_oplog(&conn, 9_000).is_err());
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
        // 3 NodeCreate + 1 NodeStatus (mem_b obsolete) + 1 EdgeAdd (from obsolete mem_b) = 5 `/3`
        // content entries; 3 projected nodes.
        assert_eq!(entry_count(&conn), 5);
        assert_eq!(projected_node_count(&conn), 3);
        assert_eq!(projected_node_status(&conn, "mem_b"), "obsolete");
        let edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM content_projected_edges", [], |r| r.get(0))
            .unwrap();
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
        assert_eq!(
            local_account_count(&conn),
            0,
            "the scope gate no-ops BEFORE the mint — a placeholder repo mints no account",
        );
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
        assert_eq!(local_account_count(&conn), 0, "a local: repo mints no account");
    }

    #[test]
    fn backfill_is_a_noop_on_an_unscoped_db() {
        // No repos row, no connection scope → memory_repo_scope is None → nothing to root a stream.
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(entry_count(&conn), 0);
        assert_eq!(local_account_count(&conn), 0, "an unscoped DB mints no account");
    }

    #[test]
    fn backfill_of_an_empty_scoped_repo_establishes_ownership_but_authors_no_content() {
        // A fresh scoped repo with no memories: the reconcile falls through the fast-path probe
        // (ownership not yet established), mints the account, and publishes the `/2` StreamOwn —
        // but authors NO `/3` content (nothing is missing). So the content chain stays
        // empty while ownership is now live (the first live op will chain off an empty
        // content chain = genesis).
        let conn = scoped_conn();
        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(entry_count(&conn), 0, "no memories ⇒ no /3 content entries");
        assert_eq!(local_account_count(&conn), 1, "a scoped repo mints the store's local account");
        assert_eq!(owned_stream_count(&conn), 1, "and publishes exactly one /2 StreamOwn");
    }

    // --- live write-path wiring (#532) ---

    fn projected_edge_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM content_projected_edges", [], |r| r.get(0)).unwrap()
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
                "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = ?1",
                [&r.memory.memory_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "the created memory is a projected /3 node");
        assert_eq!(projected_node_count(&conn), 1);
    }

    #[test]
    fn a_scope_less_create_authors_nothing() {
        // No repos row / no active-repo context → the scope gate skips authoring entirely.
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        create_concept(&conn, "t1").unwrap();
        assert_eq!(entry_count(&conn), 0, "a scope-less create never touches the log");
        assert_eq!(local_account_count(&conn), 0, "a scope-less create mints no account");
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
                "SELECT content_json, status FROM content_projected_nodes WHERE node_id = ?1",
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
        assert_eq!(projected_node_status(&conn, &id), "obsolete");
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

    // --- live mutation seam self-heals a ghost end to end (#541 Task 4) ---

    #[test]
    fn mark_obsolete_on_a_ghost_authors_a_create_not_an_inert_status() {
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap(); // roots the chain
        insert_memory(&conn, "mem_ghost", "active", 500); // raw, un-authored ghost
        // mark_obsolete reconciles first (heals NodeCreate + NodeStatus{active}), THEN authors the
        // obsolete NodeStatus — so it is NOT inert and the node projects obsolete.
        crate::query::memory::mark_obsolete(&conn, "mem_ghost").unwrap();
        assert_eq!(
            projected_node_status(&conn, "mem_ghost"),
            NodeStatus::Obsolete.as_db_str(),
            "ghost healed then obsoleted",
        );
    }

    #[test]
    fn remove_edge_on_a_ghost_edge_heals_then_tombstones_not_an_inert_remove() {
        // The EdgeRemove path: `remove_edge` calls backfill (edges.rs) BEFORE its delete txn, so a
        // raw ghost edge is first healed (EdgeAdd authored), then the delete authors EdgeRemove —
        // the signed history is add→remove (complete), and the projection ends with the
        // edge ABSENT (not an inert tombstone with no matching add).
        //
        // `remove_edge` authors its `EdgeRemove` unconditionally once the raw row is deleted
        // (edges.rs gates it on `n > 0`, NOT on whether an `EdgeAdd` was ever signed) — so
        // `edges.is_empty()` alone is satisfied whether or not the heal ran (a
        // never-authored edge and a healed-then-removed edge both project empty). The
        // `entry_count` delta is what actually distinguishes them: it is +2 (heal's
        // `EdgeAdd` + `remove_edge`'s own `EdgeRemove`) only when the reconcile fired; a
        // disabled reconcile would author just the bare `EdgeRemove` (+1).
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let b = create_concept(&conn, "b").unwrap().memory.memory_id;
        insert_raw_node_edge(&conn, &a, "relates_to", &b);
        let key = crate::query::memory::edge_key(&a, "relates_to", "node", &b);
        let before = entry_count(&conn);
        crate::query::memory::remove_edge(&conn, &key).unwrap();
        assert_eq!(
            entry_count(&conn),
            before + 2,
            "the heal's EdgeAdd + remove_edge's own EdgeRemove — not a bare, inert tombstone"
        );
        assert_eq!(projected_edge_count(&conn), 0, "healed then tombstoned → edge absent");
    }

    #[test]
    fn a_failed_author_rolls_back_the_memory_write() {
        let conn = scoped_conn();
        // One good create so the account + owner stream are established and the first create
        // stamped the `/3` content projector version (the second create's backfill
        // fast-paths, isolating the failure to the live author's reproject).
        create_concept(&conn, "first").unwrap();
        let before: i64 =
            conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
        // Poison the `/3` projector guard: pretend a NEWER binary already folded this store's `/3`
        // projection, so `reproject_accepted_content_stream`'s `assert_content_projector_not_newer`
        // errors and the second create's `/3` author fails (this doubles as the #664 `/3`
        // projector-stamp poison test).
        conn.execute(
            "UPDATE oplog_meta SET value = '999' WHERE key = 'content_projector_version'",
            [],
        )
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
                "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = 'old'",
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

    // --- owner-bound /2//3 retarget (#664) ---

    #[test]
    fn a_fresh_repo_first_create_mints_the_account_publishes_ownership_and_projects_the_node() {
        let conn = scoped_conn();
        let id = create_concept(&conn, "first").unwrap().memory.memory_id;
        // The first create on a fresh scoped repo mints exactly one local account and folds exactly
        // one `/2` StreamOwn effective — the ownership the owner-authored `/3` content accepts
        // under.
        assert_eq!(local_account_count(&conn), 1, "the first create mints one local account");
        assert_eq!(owned_stream_count(&conn), 1, "exactly one /2 StreamOwn folds effective");
        // And the memory is an accepted, projected `/3` node.
        assert_eq!(projected_node_count(&conn), 1, "the created memory is a projected /3 node");
        assert_eq!(projected_node_status(&conn, &id), "active", "a fresh create projects active");
    }

    #[test]
    fn a_second_repo_first_create_establishes_its_own_ownership_despite_the_shared_account() {
        // Multi-repo store, the distinguishing case for the fast-path probe: repo A's create mints
        // the STORE-GLOBAL account and establishes A's ownership. Repo B is then fresh with ZERO
        // memories. B's first create must STILL publish B's own `/2` StreamOwn — the account
        // already being minted is NOT enough. This is why the probe checks
        // `established_owned_stream_v2` (StreamOwn folded EFFECTIVE) and not merely
        // "account minted": with the weaker check, B's empty anti-join would early-return,
        // B would author `/3` content under an unowned stream, and verify-accepted would
        // roll the create back (Risk trap #1). A single-repo test cannot catch this — there
        // the account is unminted, so both checks agree.
        let conn = scoped_conn(); // repo-a: registered + scoped
        create_concept(&conn, "a1").unwrap();
        assert_eq!(local_account_count(&conn), 1, "repo-a's create mints the store account");
        assert_eq!(owned_stream_count(&conn), 1, "repo-a owns its /2 stream");

        // Register a SECOND repo in the same store and scope the connection to it.
        const REPO_B: &str = "repo-b";
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
            [REPO_B],
        )
        .unwrap();
        set_scope(&conn, REPO_B);

        // B's first create: the account is already minted, B has no StreamOwn and no memories.
        let id_b = create_concept(&conn, "b1").unwrap().memory.memory_id;
        assert_eq!(
            local_account_count(&conn),
            1,
            "the account is store-global — still exactly one"
        );
        assert_eq!(
            owned_stream_count(&conn),
            2,
            "repo-b established its OWN /2 StreamOwn rather than riding repo-a's",
        );
        assert_eq!(
            projected_node_status(&conn, &id_b),
            "active",
            "b1 accepted under repo-b's freshly-published ownership",
        );
    }

    #[test]
    fn pre_existing_memories_are_adopted_into_v3_and_the_v1_tables_are_left_untouched() {
        let conn = scoped_conn();
        // Pre-existing history a pre-#664 binary / raw writer left in the tables (never signed).
        insert_memory(&conn, "old_a", "active", 100);
        insert_memory(&conn, "old_b", "obsolete", 200);
        insert_memory(&conn, "old_c", "active", 300);
        // One live mutation triggers the reconcile: the three pre-existing rows are authored into
        // `/3` as the genesis batch, then the new memory is authored live.
        create_concept(&conn, "trigger").unwrap();
        for id in ["old_a", "old_b", "old_c"] {
            let projected: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(projected, 1, "pre-existing memory {id} was adopted into /3");
        }
        assert_eq!(projected_node_status(&conn, "old_b"), "obsolete", "the adopted status carried");
        // The retained `/1` tables are UNTOUCHED — the live path no longer writes them (issue J1).
        let v1_entries: i64 =
            conn.query_row("SELECT COUNT(*) FROM oplog_entries", [], |r| r.get(0)).unwrap();
        let v1_nodes: i64 =
            conn.query_row("SELECT COUNT(*) FROM oplog_projected_nodes", [], |r| r.get(0)).unwrap();
        assert_eq!(v1_entries, 0, "the /1 entry log is not written by the retargeted live path");
        assert_eq!(v1_nodes, 0, "the /1 shadow projection is not written by the retarget");
    }

    #[test]
    fn racing_backfills_converge_on_one_stream_own_and_no_duplicate_content() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.db");
        // Setup once: schema, the registered repo, and one pre-existing memory so there is content
        // for the reconcile to author (the racers must converge on authoring it exactly once).
        let setup = Connection::open(&path).unwrap();
        setup.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::index::schema::apply(&setup).unwrap();
        setup
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
                [REPO],
            )
            .unwrap();
        set_scope(&setup, REPO);
        insert_memory(&setup, "old", "active", 100);
        drop(setup);

        let barrier = Arc::new(Barrier::new(2));
        let spawn = || {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                set_scope(&conn, REPO);
                barrier.wait();
                backfill_memory_oplog(&conn, 9_000).unwrap();
            })
        };
        let a = spawn();
        let b = spawn();
        a.join().unwrap();
        b.join().unwrap();

        let conn = Connection::open(&path).unwrap();
        assert_eq!(local_account_count(&conn), 1, "the racers converge on one local account");
        assert_eq!(owned_stream_count(&conn), 1, "exactly one /2 StreamOwn survives the race");
        assert_eq!(entry_count(&conn), 1, "the pre-existing memory is authored exactly once");
        let old_nodes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_nodes, 1, "the adopted memory projects exactly once, no duplicate");
    }

    #[test]
    fn an_oversized_memory_body_fails_the_reconcile_loudly_and_surfaces_the_repo() {
        // Fail-loud posture (#664): a raw/adversarial memory body over the §18a 256 KiB envelope
        // cap makes the `/3` author bail rather than drop irreplaceable data, and the
        // reconcile surfaces the failure (naming the repo) instead of silently skipping it.
        // A normal rag-rat memory (body ≤ 8 KiB) can never reach this; it is an
        // adversarial/raw-writer backstop.
        let conn = scoped_conn();
        let oversized = "x".repeat(300 * 1024); // > 256 KiB ⇒ the signed envelope exceeds the cap
        insert_memory_with_body(&conn, "mem_big", &oversized);
        let err = backfill_memory_oplog(&conn, 1_000).unwrap_err();
        assert!(
            format!("{err:#}").contains(REPO),
            "the fail-loud error surfaces the reconcile failure for the repo, not a silent skip",
        );
        assert_eq!(entry_count(&conn), 0, "the oversized batch rolled back — no /3 content stored");
    }
}
