//! The per-node/edge reconcile that keeps the op log a COMPLETE signed mirror of
//! `repo_memories` / `repo_node_edges` (#524, #541, #664).
//!
//! [`backfill_memory_oplog`] is a per-node/edge RECONCILE (#541), not a per-chain gate: it authors
//! every table row MISSING from the accepted-`/3` projection, so a row that entered the tables
//! outside the wired path (a pre-#532 binary, a raw writer, a consolidation import, or pre-existing
//! `/1` history) is signed on the next mutation and no later lifecycle op on it is ever inert.
//! Genesis is just the empty-`/3`-chain case where every row is missing.

use std::collections::HashSet;

use anyhow::Context;
use rag_rat_oplog::{EdgeKey, EdgeSpec, MemoryOp, NodeContent, NodeId, NodeStatus, StreamId};
use rag_rat_query::memory::{
    EdgeRelation, NodeEdge, RepoMemory, memory_repo_scope, tags_for_memory,
};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::authoring::{
    ANCHOR_MATCHES_BINDING_SQL, AuthoredDurability, BOUND_SCOPE_PATH_SQL, anchor_publication_ops,
    prepare_owner_authoring,
};
use super::ownership::{
    StreamSealPolicy, contribution_targets, ensure_not_mirroring_another_account,
    is_contribution_mode, owner_stream_access_mode, stream_seal_policy,
};

/// One memory's projectable content — the columns the op model carries (NOT the identity / anchor /
/// dedup bookkeeping). Read in bulk so the backfill makes one pass over `repo_memories`.
pub(super) struct MemoryRow {
    pub(super) memory_id: String,
    pub(super) kind: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) confidence: String,
    pub(super) status: String,
    pub(super) source: String,
    pub(super) payload_json: Option<String>,
    pub(super) tags: Vec<String>,
}

/// The op model's content register for a reconciled row. It and [`node_content_of_memory`] are the
/// two sources of signed `NodeContent`, so both map every column to its register by NAME: the
/// adjacent `confidence` / `source` tokens are carried verbatim into an append-only log.
pub(super) fn node_content_of_row(row: &MemoryRow) -> NodeContent {
    NodeContent {
        kind: row.kind.clone(),
        title: row.title.clone(),
        body: row.body.clone(),
        confidence: row.confidence.clone(),
        source: row.source.clone(),
        tags: row.tags.clone(),
        payload: row.payload_json.clone(),
    }
}

/// The op model's content register for a live-authored memory; see [`node_content_of_row`].
pub(super) fn node_content_of_memory(memory: &RepoMemory) -> NodeContent {
    NodeContent {
        kind: memory.kind.clone(),
        title: memory.title.clone(),
        body: memory.body.clone(),
        confidence: memory.confidence.clone(),
        source: memory.source.clone(),
        tags: memory.tags.clone(),
        payload: memory.payload_json.clone(),
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
pub(super) fn node_ops(
    row: &MemoryRow,
    elide_active_status: bool,
) -> anyhow::Result<Vec<MemoryOp>> {
    let node_id = NodeId::from(row.memory_id.as_str());
    let mut ops =
        vec![MemoryOp::NodeCreate { node_id: node_id.clone(), content: node_content_of_row(row) }];
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
pub(super) fn edge_add_op(edge: &NodeEdge, owner_repo_id: &str) -> anyhow::Result<MemoryOp> {
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
pub(super) fn build_reconcile_ops(
    conn: &Connection,
    missing_nodes: &[MemoryRow],
    missing_edges: &[NodeEdge],
    anchor_backfill_ops: &[MemoryOp],
    owner_repo_id: &str,
    policy: StreamSealPolicy,
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
        // The backfill leg has to publish anchors too, or every memory that predates the anchor op
        // replicates with none and a peer seeds nothing for it — permanently, since this anti-join
        // never revisits a node once it exists. That covers the existing corpus on every upgraded
        // store, and `sync publish --seed`, which reconciles a whole index onto a public stream.
        //
        // An unpublishable set is dropped rather than quarantining the node: anchors are
        // decoration, and losing them must never cost a peer the memory itself.
        let publication = anchor_publication_ops(conn, &row.memory_id)?;
        if publication.iter().all(|op| content_op_is_authorable(op, policy)) {
            ops.extend(publication);
        }
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
    // Anchors for memories whose node was authored before this op kind existed — the corpus the
    // node anti-join above can never revisit. Already partitioned by authorability at read time,
    // so an unpublishable set was warned about there and never reaches this batch.
    ops.extend(anchor_backfill_ops.iter().cloned());
    Ok(ops)
}

/// Whether `row`'s `NodeCreate` fits the signed `/3` content envelope. A normal rag-rat memory
/// (title ≤ 160 chars, body ≤ 8 000 chars, payload capped by [`validate_payload`]) always fits;
/// only a raw writer / import / pre-cap ghost with an oversized body or payload can fail this, and
/// such a row is QUARANTINED rather than allowed to wedge the whole batch (#680). The status/edge
/// ops are tiny and never oversized, so the `NodeCreate` alone decides a node's authorability.
pub(super) fn node_is_authorable(row: &MemoryRow, policy: StreamSealPolicy) -> bool {
    let op = MemoryOp::NodeCreate {
        node_id: NodeId::from(row.memory_id.as_str()),
        content: node_content_of_row(row),
    };
    content_op_is_authorable(&op, policy)
}

/// Whether `edge`'s `EdgeAdd` fits the signed `/3` content envelope. The edge twin of
/// [`node_is_authorable`] (#680): a normal edge (short ids + a resolved node/github anchor) always
/// fits, and the write path now caps `target_anchor` / `target_repo_id`, so only a raw writer /
/// pre-cap import / consolidation-remapped ghost with an oversized free-form field can fail this —
/// and such an edge is QUARANTINED rather than allowed to make the whole reconcile `bail!` and
/// wedge every other memory write. An edge whose relation TOKEN this binary can't map is a
/// DIFFERENT (forward-compat) failure — [`build_reconcile_ops`] surfaces it loudly via
/// [`edge_add_op`], exactly as an unknown node status does — so a build error counts as authorable
/// here and defers to that path rather than silently quarantining it.
fn edge_is_authorable(edge: &NodeEdge, owner_repo_id: &str, policy: StreamSealPolicy) -> bool {
    match edge_add_op(edge, owner_repo_id) {
        Ok(op) => content_op_is_authorable(&op, policy),
        Err(_) => true,
    }
}

pub(super) fn content_op_is_authorable(op: &MemoryOp, policy: StreamSealPolicy) -> bool {
    match policy {
        StreamSealPolicy::Plaintext => rag_rat_oplog::content_op_is_authorable(op),
        StreamSealPolicy::Sealed => rag_rat_oplog::content_op_is_sealed_authorable(op),
    }
}

/// The reconcile's missing set, split into what CAN be signed and what must be QUARANTINED (#680).
/// `live_edges` already excludes any edge whose SOURCE node is quarantined — an `EdgeAdd` with no
/// authored `NodeCreate` for its source would project a dangling edge — AND any edge whose OWN
/// `EdgeAdd` is oversized (those go to `quarantined_edges`).
pub(super) struct ReconcileWork {
    pub(super) authorable_nodes: Vec<MemoryRow>,
    pub(super) live_edges: Vec<NodeEdge>,
    quarantined_nodes: Vec<MemoryRow>,
    quarantined_edges: Vec<NodeEdge>,
    /// Snapshots for already-authored memories still owed one — see [`read_anchor_backfill_ids`].
    pub(super) anchor_backfill_ops: Vec<MemoryOp>,
    /// Memories the sweep selected whose snapshot will not fit a signed entry. Like a quarantined
    /// node, these are deliberately NOT work: re-selecting them forever is what would spin the
    /// slow path.
    pub(super) quarantined_anchor_ids: Vec<String>,
}

impl ReconcileWork {
    /// Any AUTHORABLE work remaining. A quarantined row is intentionally NOT work: it never becomes
    /// authorable on its own, so counting it would spin the reconcile's slow path forever (#680).
    pub(super) fn has_authorable_work(&self) -> bool {
        !self.authorable_nodes.is_empty()
            || !self.live_edges.is_empty()
            || !self.anchor_backfill_ops.is_empty()
    }

    /// Surface every quarantined node, edge, and anchor set as a warning naming the repo + the
    /// row's id — the per-row failure the caller can act on, in place of the old fail-loud that
    /// wedged the store.
    pub(super) fn warn_quarantined(&self, repo_id: &str) {
        for row in &self.quarantined_nodes {
            tracing::warn!(
                repo_id,
                memory_id = %row.memory_id,
                "quarantining an un-authorable memory row: its signed /3 envelope exceeds the §18a \
                 256 KiB cap, so it is skipped to keep the memory-write path live; shrink or delete \
                 it through the public API to recover it (#680)",
            );
        }
        for memory_id in &self.quarantined_anchor_ids {
            tracing::warn!(
                repo_id,
                memory_id = %memory_id,
                "not publishing a memory's anchor set: it exceeds the anchor-count or signed-entry \
                 cap, so the memory replicates without its bindings and a peer cannot seed them; \
                 reduce or shorten its bindings through the public API to recover it",
            );
        }
        for edge in &self.quarantined_edges {
            tracing::warn!(
                repo_id,
                edge_key = %edge.edge_key,
                source_node_id = %edge.source_node_id,
                "quarantining an un-authorable node-edge: its signed /3 envelope exceeds the §18a \
                 256 KiB cap, so it is skipped to keep the memory-write path live; remove it \
                 through the public API to recover it (#680)",
            );
        }
    }
}

/// The pending-fold barrier (#698): memory completeness may not be read while the owner stream
/// owes a deferred content refold, because the accepted-`/3` projection is stale until settle.
///
/// The debt is settled INSIDE the caller's own IMMEDIATE transaction, immediately before the
/// authoritative re-read, and never on an autocommit connection beforehand. A foreign `/3`
/// candidate may target the LOCAL owner stream, so a remote peer can re-enqueue debt on the
/// largest stream in the store at will; draining it ahead of the transaction meant an unbudgeted
/// refold of the whole local memory history on the interactive write path, and a trip observed
/// inside an already-open transaction could only hard-error (#798 adversarial findings 2 and 5).
/// Settling in-transaction bounds the cost to ONE fold per local write — work the write's own
/// authoring performs anyway — and lets a mid-write enqueue self-heal. A fold failure propagates
/// and rolls the write back, so the barrier stays fail-closed.
pub(super) fn settle_owner_stream_in_tx(
    tx: &Transaction<'_>,
    stream: StreamId,
    now_ms: i64,
) -> anyhow::Result<()> {
    rag_rat_oplog::settle_pending_content_refold_for_stream_in_tx(tx, stream, now_ms)
        .context("settling the owner stream's pending content refold before reading completeness")
}

/// How many anchor snapshots one reconcile pass PUBLISHES. The pass rides every authored write, so
/// an unbounded sweep would make the first write after an upgrade pay for the whole corpus;
/// bounded, it converges over successive writes and each pass stays cheap.
pub(super) const ANCHOR_BACKFILL_PER_PASS: usize = 64;

/// How many candidates one pass EXAMINES to find that many publishable ones.
///
/// The two differ because a quarantined memory never leaves the match set — `anchors_json` stays
/// NULL by design — and it sorts oldest-first, which is exactly where the window is: an over-cap
/// set can only be a legacy row, since the live write path refuses one. Taking the window as the
/// publish budget would let enough of them permanently occupy it and stall the backfill for the
/// rest of the corpus. Examining wider and stopping at the publish budget means quarantined rows
/// cost a slot in the scan, never one in the batch.
const ANCHOR_BACKFILL_SCAN_PER_PASS: i64 = 512;

/// Memories whose node is ALREADY authored but whose anchors were never published — the corpus
/// that predates the anchor op on any store that was already syncing.
///
/// This is the other half of the backfill. The node anti-join only revisits memories missing from
/// the projection, so a memory authored before the op existed is never reconsidered by it, and
/// without this its bindings would never reach a peer.
///
/// Idempotent by construction: authoring the snapshot refolds the stream in the same transaction,
/// so `anchors_json` stops being NULL and the row drops out of this query. A memory with no
/// bindings never matches at all, so an unanchored memory is not re-examined forever.
///
/// Note what the column tracks: PRESENCE, not currency. This heals `none -> some` exactly once. It
/// cannot heal `some -> different`, and the relocation engine does rewrite portable anchor identity
/// outside any authoring path, so a renamed symbol leaves a peer holding the pre-rename set until
/// an explicit rebind re-authors it. Republish-on-drift is a separate mechanism, not this one.
pub(super) fn read_anchor_backfill_ids(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<Vec<String>> {
    // The scope leg republishes this device's bindings as the memory's set, so it is confined to
    // sets THIS account authored whose bindings still equal the projected set by target: a
    // contributor's set on an owner-created memory, or a sibling device's newer rebind the local
    // rows have yet to catch up with, must not be re-signed here as this device's own.
    let local_account = rag_rat_oplog::read_local_account(conn)?.map(|account| account.to_bytes());
    let mut stmt = conn.prepare(&format!(
        // `origin = 'local'` for the same load-bearing reason as the node anti-join above: a
        // synced row is a peer's to publish, never this device's to author.
        //
        // The second leg is the scope backfill (#1276): a set published before the scope op
        // existed projects symbol anchors with no `scope_hash`. Where such an anchor's binding
        // still resolves a scope, the memory is swept again, so an upgraded author republishes its
        // unchanged set with scopes beside it and a receiver's baseline gains them before the next
        // rebind. It converges: the republished scope lands in the projection, and an anchor whose
        // handle resolves nothing is never selected. Only for a set this account authored whose
        // bindings still equal the projected set by target (see `ANCHOR_MATCHES_BINDING_SQL`).
        "SELECT m.id
         FROM repo_memories m
         JOIN content_projected_nodes p ON p.stream_id = ?2 AND p.node_id = m.id
         WHERE m.repo_id = ?1
           AND m.origin = 'local'
           AND EXISTS (
                 SELECT 1 FROM repo_memory_bindings b
                 WHERE b.memory_id = m.id AND b.repo_id = m.repo_id)
           AND (p.anchors_json IS NULL
                OR (p.anchors_author = ?4
                    AND EXISTS (
                         SELECT 1 FROM json_each(p.anchors_json) a
                         JOIN repo_memory_bindings b
                           ON b.memory_id = m.id AND b.repo_id = m.repo_id
                          AND b.binding_kind = json_extract(a.value, '$.binding_kind')
                          AND b.binding_id = json_extract(a.value, '$.binding_id')
                         WHERE json_extract(a.value, '$.binding_kind')
                                   IN ('symbol', 'logical_symbol')
                           AND json_extract(a.value, '$.scope_hash') IS NULL
                           AND COALESCE({BOUND_SCOPE_PATH_SQL}, '') != '')
                    AND NOT EXISTS (
                         SELECT 1 FROM repo_memory_bindings b
                         WHERE b.memory_id = m.id AND b.repo_id = m.repo_id
                           AND NOT EXISTS (
                                 SELECT 1 FROM json_each(p.anchors_json) a
                                 WHERE {ANCHOR_MATCHES_BINDING_SQL}))
                    AND NOT EXISTS (
                         SELECT 1 FROM json_each(p.anchors_json) a
                         WHERE NOT EXISTS (
                                 SELECT 1 FROM repo_memory_bindings b
                                 WHERE b.memory_id = m.id AND b.repo_id = m.repo_id
                                   AND {ANCHOR_MATCHES_BINDING_SQL}))))
         ORDER BY m.created_at_ms, m.id
         LIMIT ?3"
    ))?;
    let ids = stmt
        .query_map(
            params![
                repo_id,
                stream.to_bytes().as_slice(),
                ANCHOR_BACKFILL_SCAN_PER_PASS,
                local_account.as_ref().map(|bytes| bytes.as_slice()),
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Read the repo's unauthored nodes + edges and partition out the un-authorable (#680): the fast
/// path calls it to decide whether real work remains, the slow path to build the batch from the
/// AUTHORABLE half. Scope-independent like its two readers, so it runs on either an autocommit
/// `Connection` (fast path) or the reconcile `Transaction` (slow path, via deref).
///
/// Callers inside a transaction MUST call [`settle_owner_stream_in_tx`] first: this reads the
/// accepted-`/3` projection, which is stale while the stream owes a deferred refold. The
/// autocommit fast path cannot settle, so it treats outstanding debt as "work may exist" rather
/// than trusting an empty read (see `sync_owner_stream`).
pub(super) fn read_reconcile_work(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
    policy: StreamSealPolicy,
) -> anyhow::Result<ReconcileWork> {
    let (authorable_nodes, quarantined_nodes): (Vec<MemoryRow>, Vec<MemoryRow>) =
        read_unauthored_memory_rows(conn, repo_id, stream)?
            .into_iter()
            .partition(|row| node_is_authorable(row, policy));
    // An edge whose source node is quarantined has no authored `NodeCreate` to hang off, so drop it
    // WITH its source (the source node's quarantine warning is the actionable one — it never
    // reaches `quarantined_edges`).
    let quarantined_node_ids: HashSet<&str> =
        quarantined_nodes.iter().map(|row| row.memory_id.as_str()).collect();
    // Partition the remaining edges by authorability the SAME way nodes are (#680): an edge whose
    // OWN `EdgeAdd` is oversized (a raw / imported / pre-cap ghost carrying an oversized free-form
    // field) is QUARANTINED rather than left in `live_edges` to make `author_content_batch_in_tx`
    // `bail!` and wedge the write path — the exact failure mode the node quarantine removes,
    // reached via an edge instead of a node.
    let (live_edges, quarantined_edges): (Vec<NodeEdge>, Vec<NodeEdge>) =
        super::edges::unauthored_edges(conn, repo_id, stream)?
            .into_iter()
            .filter(|edge| !quarantined_node_ids.contains(edge.source_node_id.as_str()))
            .partition(|edge| edge_is_authorable(edge, repo_id, policy));
    // Partition the anchor sweep the SAME way nodes and edges are, and for the same reason. This
    // leg selects on `anchors_json IS NULL`, a condition authoring is only USUALLY able to clear:
    // an over-cap or oversized set is dropped, never folds, stays NULL, and would be re-selected on
    // every pass — reporting authorable work forever and spinning the reconcile's slow path, the
    // exact #680 property `has_authorable_work` documents. Building the op here keeps
    // `has_authorable_work` implying a non-empty batch.
    let mut anchor_backfill_ops = Vec::new();
    let mut quarantined_anchor_ids = Vec::new();
    let mut swept = 0;
    for memory_id in read_anchor_backfill_ids(conn, repo_id, stream)? {
        // Stop once the pass has its publish budget. Candidates past this point are simply not this
        // pass's work — they are neither authored nor quarantined, and the next pass reaches them.
        // Counted in MEMORIES, not ops, so the source hash riding along below cannot halve it.
        if swept >= ANCHOR_BACKFILL_PER_PASS {
            break;
        }
        // The hash describes exactly the anchors this sweep publishes, so it rides the same batch,
        // ahead of them (see `anchor_publication_ops`). An empty publication is unreachable — the
        // query requires a binding — but counting it as quarantined keeps the partition total
        // rather than silently dropping.
        let publication = anchor_publication_ops(conn, &memory_id)?;
        if !publication.is_empty()
            && publication.iter().all(|op| content_op_is_authorable(op, policy))
        {
            anchor_backfill_ops.extend(publication);
            swept += 1;
        } else {
            quarantined_anchor_ids.push(memory_id);
        }
    }
    Ok(ReconcileWork {
        authorable_nodes,
        live_edges,
        quarantined_nodes,
        quarantined_edges,
        anchor_backfill_ops,
        quarantined_anchor_ids,
    })
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
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        return Ok(());
    }
    // Mint the store's local account BEFORE the durability guard: the mint
    // self-transacts and holds its OWN durability guard that restores `synchronous = NORMAL` on
    // drop — beginning our guard first would let the mint's drop downgrade our authored commit
    // below NORMAL, silently losing the #560 durability. The mint is store-global and
    // idempotent (a re-mint returns the same account).
    rag_rat_oplog::local_account(conn, now_ms)?;
    let stream = ensure_owner_stream(conn, repo_id, now_ms)?;
    let policy = stream_seal_policy(conn, repo_id, stream)?;
    // While the owner stream owes a deferred refold, the accepted-`/3` projection is stale and the
    // completeness readers refuse to run at all (they are the fail-closed barrier) — so the
    // autocommit fast path is SKIPPED entirely rather than consulted and disbelieved. The
    // transaction below settles the debt first and then reads authoritatively. Preparation does not
    // depend on the op set (only the authorability pre-check does, and the in-transaction read
    // quarantines un-authorable rows by construction), so a sentinel drives it exactly as the
    // sealed-enable path does. A false positive costs one otherwise-idle transaction: authoring an
    // empty batch is already skipped below.
    let prepared = if rag_rat_oplog::content_stream_has_pending_refold(conn, stream)? {
        let sentinel =
            MemoryOp::EdgeRemove { edge_key: EdgeKey::from("pending-refold-settle-preparation") };
        prepare_owner_authoring(conn, repo_id, stream, policy, &[sentinel], now_ms)?
    } else {
        let work = read_reconcile_work(conn, repo_id, stream, policy)?;
        if !work.has_authorable_work() {
            work.warn_quarantined(repo_id);
            return Ok(());
        }
        let ops = build_reconcile_ops(
            conn,
            &work.authorable_nodes,
            &work.live_edges,
            &work.anchor_backfill_ops,
            repo_id,
            policy,
            rag_rat_oplog::content_stream_is_empty(conn, stream)?,
        )?;
        prepare_owner_authoring(conn, repo_id, stream, policy, &ops, now_ms)?
    };

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Settle the owner stream's deferred refold debt HERE, inside the write's own transaction and
    // before the authoritative re-read, so completeness is read against a current projection.
    settle_owner_stream_in_tx(&tx, stream, now_ms)?;
    anyhow::ensure!(
        stream_seal_policy(&tx, repo_id, stream)? == policy,
        "memory stream seal policy changed while preparing reconcile; retry"
    );
    // Authoritative re-read UNDER the write lock (TOCTOU): a concurrent author may have healed or
    // added rows between the probe and the lock, so re-read the missing set and re-derive `genesis`
    // here. `genesis` decides the status-elision: an empty `/3` content chain ⇒ no stale registers
    // ⇒ elide `active` (byte-identical to the pre-#541 genesis). This equivalence holds ONLY
    // under the single-local-writer owner stream (see the module header); phase D (foreign
    // devices can populate the stream) must revisit whether local-chain-empty still implies
    // register-clean.
    let genesis = rag_rat_oplog::content_stream_is_empty(&tx, stream)?;
    // Quarantine un-authorable rows (#680): partition the oversized ones out so a single row whose
    // signed `/3` envelope exceeds the §18a cap cannot make the whole batch `bail!` and wedge every
    // other memory write. The authorable rows are signed; the quarantined ones are logged and left
    // for the public API to shrink or delete.
    let work = read_reconcile_work(&tx, repo_id, stream, policy)?;
    work.warn_quarantined(repo_id);
    let ops = build_reconcile_ops(
        &tx,
        &work.authorable_nodes,
        &work.live_edges,
        &work.anchor_backfill_ops,
        repo_id,
        policy,
        genesis,
    )?;
    // Skip the author when there is nothing to author: a fresh repo whose anti-join was empty only
    // needed ownership established (done above), and authoring an empty batch would still refold +
    // reproject for no change.
    if !ops.is_empty() {
        let prepared =
            prepared.as_ref().context("reconcile work unexpectedly prepared as an empty batch")?;
        // Every op here is authorable — `read_reconcile_work` already quarantined any oversized row
        // (#680), so the `/3` author's §18a size check cannot fire on this batch. `with_context`
        // still names the repo so any OTHER authoring failure (a stale `auth_len`, a contested
        // account) is attributable rather than surfacing as a bare rollback.
        rag_rat_oplog::author_prepared_content_batch_in_tx(
            &tx,
            stream,
            &ops,
            prepared.owner_prepared()?,
            now_ms,
        )
        .with_context(|| {
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

pub(super) fn ensure_owner_stream(
    conn: &Connection,
    repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<StreamId> {
    // Resolve the /2 stream under the repo's persisted access-mode intent, so live-write, drain,
    // reconcile, and catch-up all target the SAME stream id (a `PublicRead` stream has a distinct
    // id from the `Private` one). Absent intent = Private, today's behavior.
    let mode = owner_stream_access_mode(conn, repo_id)?;
    if let Some(stream) = rag_rat_oplog::established_owned_stream_v2_with_mode(conn, repo_id, mode)?
    {
        return Ok(stream);
    }
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Establishing a PRIVATE stream here would silently un-serve every repo this store contributes
    // to: an account is servable to a peer only when ALL of its streams are public, content is
    // served by its AUTHOR, and the owner is not enrolled here — so the contributions this store
    // has already authored, and any it authors later, become permanently unreachable.
    //
    // The configure-time check in `set_contribution_owner` cannot cover this: it runs once, and the
    // conflicting stream is created later by ordinary authoring in a DIFFERENT repo. Enforce it
    // where the conflict is actually created, inside the same transaction that would create it.
    //
    // Yes, this means memory authoring in an unrelated private repo fails while this index
    // contributes — or has ever contributed, since the authored entries outlive the configuration.
    // That is the honest ordering: the alternative is authoring memories nobody can ever fetch and
    // discovering it much later. The error names both escapes, and nothing is committed on the way
    // out, so re-running `sync contribute` or publishing the repo unblocks it.
    if mode != rag_rat_oplog::AccessMode::PublicRead
        && let Some(cause) = private_stream_strands_contributions(&tx)?
    {
        return Err(PrivateStreamRefusal(format!(
            "repo `{repo_id}` would need a PRIVATE memory stream, but this index {cause} — and an \
             account is fetchable by a peer only while all of its streams are public, so this \
             would strand those contributions unreachable. Index `{repo_id}` in a separate \
             database, or publish it with `rag-rat sync publish`"
        ))
        .into());
    }
    let stream = rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(&tx, repo_id, mode, now_ms)?;
    tx.commit()?;
    Ok(stream)
}

/// The refusal [`ensure_owner_stream`] raises rather than establish a PRIVATE stream that would
/// strand contributions. Typed, not a bare `bail!`, because this is a stream-establishment POLICY:
/// it belongs on the paths where a user is asking for a memory write, and the INDEX-MAINTENANCE
/// seam that shares the same reconcile ([`heal_memory_oplog_ghosts`]) recognizes it and skips.
/// Left as an opaque error there, an ordinary `sync uncontribute` would fail `rag-rat reconcile`,
/// every watcher pass, and `rag-rat index` — with no ghost to heal in the first place.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct PrivateStreamRefusal(String);

/// Why establishing a PRIVATE stream in this index would strand contributions, phrased as the
/// middle of the refusal sentence — or `None` when nothing is at stake.
///
/// EVIDENCE outranks configuration for the streams this account has ALREADY authored onto: `sync
/// uncontribute` (or any other path that drops the meta row) clears the configured target, but
/// those entries stay on the owner's PublicRead stream, and the owner can fetch them only while
/// this account owns no private stream. Keyed on configuration alone, the unset would open a door
/// that a `StreamOwn` — append-only, never un-authorable — then closes forever.
///
/// Each authored stream is put through the same servability check the serving side applies
/// ([`crate::sync_driver::contribution_stream_is_servable`]), so the refusal fires exactly when
/// something real is at stake. Authorship the owner has since revoked strands nothing — its pull
/// already cannot reach this account for that stream — and blocking on it would be a permanent
/// refusal with no recourse.
fn private_stream_strands_contributions(conn: &Connection) -> anyhow::Result<Option<String>> {
    // A configured target whose owner is pinned here is not at stake either: nothing can be
    // served to or pulled from it (the same rule the evidence half below applies).
    for (contributing_repo, owner) in contribution_targets(conn)? {
        if rag_rat_oplog::account_is_pinned(conn, owner)? {
            continue;
        }
        return Ok(Some(format!(
            "contributes repo `{contributing_repo}`'s memories to account {}",
            rag_rat_base::hash::hex_lower(&owner.to_bytes()),
        )));
    }
    // No local account ⇒ nothing was ever authored anywhere ⇒ nothing to strand.
    let Some(account) = rag_rat_oplog::read_local_account(conn)? else {
        return Ok(None);
    };
    let mut still_servable = 0usize;
    for stream in rag_rat_oplog::authored_foreign_streams(conn, account)? {
        let Some(owner) = rag_rat_oplog::stream_owner_account(conn, stream)? else {
            continue;
        };
        if crate::sync_driver::contribution_stream_is_servable(conn, owner, stream, account)? {
            still_servable += 1;
        }
    }
    if still_servable == 0 {
        return Ok(None);
    }
    Ok(Some(format!(
        "has already authored memories onto {still_servable} stream(s) another account owns and \
         can still serve"
    )))
}

/// Reconcile the ACTIVE repo's owner stream (scope read from the connection) — the idempotent call
/// every live memory/edge mutation makes before authoring (#532), now self-healing per node/edge (a
/// ghost row is authored on the next mutation, so no later lifecycle op on it is inert). A no-op on
/// an unscoped DB.
pub(crate) fn backfill_memory_oplog(conn: &Connection, now_ms: i64) -> anyhow::Result<()> {
    let Some(repo_id) = memory_repo_scope(conn)? else {
        return Ok(());
    };
    // A granted contributor does not own the stream: there is no `StreamOwn` to establish and no
    // local owner history to reconcile — its live authoring goes straight to the owner's stream via
    // the grant (#1164). The owner-only establish/reconcile below would try to author under local
    // ownership and is skipped entirely.
    if is_contribution_mode(conn, &repo_id)? {
        return Ok(());
    }
    sync_owner_stream(conn, &repo_id, now_ms)
}

/// [`backfill_memory_oplog`] as INDEX MAINTENANCE runs it — after an embedding reconcile, on every
/// watcher pass, on every `rag-rat index` — where nobody asked for a memory write.
///
/// The one difference is the stream-establishment refusal ([`PrivateStreamRefusal`]): an
/// ex-contributor still owes the owner a public account, so it provably has no owner stream and
/// never will until it publishes or re-contributes. Propagating that here would fail the whole
/// pass, with zero ghosts required — the refusal belongs on the authoring paths, which keep it.
/// Any OTHER error is a real failure and still propagates.
pub(crate) fn heal_memory_oplog_ghosts(conn: &Connection, now_ms: i64) -> anyhow::Result<()> {
    match backfill_memory_oplog(conn, now_ms) {
        Err(err) if err.downcast_ref::<PrivateStreamRefusal>().is_some() => {
            tracing::warn!(
                error = format!("{err:#}"),
                "skipping the memory op-log ghost heal; memory authoring in this repo stays \
                 refused until it is published or contributing again",
            );
            Ok(())
        },
        other => other,
    }
}

/// Reconcile a SPECIFIC repo's owner stream independent of connection scope — the seam
/// consolidation uses to author freshly-imported (remapped) rows into the TARGET's owner stream
/// under the TARGET's identity (#541). The source's pre-remap signed entries are intentionally NOT
/// carried (they are signed under the source device over pre-remap ids). Wired into consolidation
/// by [`crate::index::consolidate::run`] (#541 Task 5), immediately after the import commits
/// and before the legacy file is renamed away.
pub(crate) fn reconcile_owner_stream_for_repo(
    conn: &Connection,
    repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    // A granted contributor owns no stream for this repo, so this reconcile cannot run: it would
    // establish one and author the imported rows onto a stream nobody reads, while the configured
    // owner — where this repo's memories actually live — never receives them.
    //
    // FAIL, do not skip. Both callers (legacy consolidation, `sync publish --seed`) call this
    // specifically to author freshly-IMPORTED rows. Reporting success without authoring would
    // strand them with no `NodeCreate`, leaving every later update or status op on them inert.
    // Authoring them as a grantee is the real feature; until it exists, say so.
    //
    // This is the BACKSTOP, not the gate. By the time control reaches here the import has already
    // committed, so failing leaves the very half-applied state the refusal exists to prevent —
    // which is why both callers refuse BEFORE their irreversible step
    // (`consolidate::run` before importing, `sync_publish_seed` before publishing).
    // Keep this arm so a future third caller fails loudly instead of silently skipping, and
    // give it the same pre-check.
    //
    // (The live-write path skips silently instead, and correctly: `backfill_memory_oplog` has
    // nothing to reconcile because each mutation already authors onto the owner's stream.)
    ensure_not_mirroring_another_account(conn, repo_id, "importing memories into this repo")?;
    sync_owner_stream(conn, repo_id, now_ms)
}

/// The active repo's owner-bound `/2` stream, but ONLY when the scope is a STABLE identity to root
/// an IMMUTABLE stream on — the SAME gate the backfill uses: `Some`, not the `__unassigned__`
/// placeholder, not a `local:` shallow-clone id (both get re-pointed later) — AND the store's local
/// account is minted (the `/2` id is derived under it). `None` otherwise, and the `author_*` seams
/// SKIP authoring on `None`, so a scope-less mutation (most tests) or a store whose account is not
/// yet minted never touches the log. Derivation-only (no fact check), so it is safe inside the
/// caller's open txn — unlike the reconcile's autocommit `established_owned_stream_v2` probe.
pub(super) fn stable_owner_stream(conn: &Connection) -> anyhow::Result<Option<StreamId>> {
    let Some(repo_id) = memory_repo_scope(conn)? else {
        return Ok(None);
    };
    stable_owner_stream_for_repo(conn, &repo_id)
}

pub(super) fn stable_owner_stream_for_repo(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<Option<StreamId>> {
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        return Ok(None);
    }
    let mode = owner_stream_access_mode(conn, repo_id)?;
    rag_rat_oplog::owned_stream_v2_id_with_mode(conn, repo_id, mode)
}

/// The repo's memories with NO projected node on `stream` in the accepted-`/3` projection — the
/// rows the signed log is MISSING — in deterministic `(created_at_ms, id)` order, tags attached. On
/// an EMPTY projection this is every memory (genesis); on a populated one, the ghosts a raw writer,
/// an old binary, or pre-existing `/1` history left behind (#541, #664). Reuses the memory
/// subsystem's own tag reader (the op encoder sorts + dedupes anyway).
///
/// This trusts `content_projected_nodes` to mirror the `accepted` flag exactly. Every writer of
/// `accepted` refreshes the projection in the same txn: local authoring reprojects, trusted/local
/// account folds finalize each affected stream immediately, and deferred remote content/account
/// work reprojects at settle before clearing its mark. A future acceptance writer must uphold the
/// same coupling or this anti-join re-authors/skips rows.
pub(super) fn read_unauthored_memory_rows(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<Vec<MemoryRow>> {
    anyhow::ensure!(
        !rag_rat_oplog::content_stream_has_pending_refold(conn, stream)?,
        "owner stream has a pending content refold; settle pending content refolds before reading \
         memory completeness"
    );
    let mut stmt = conn.prepare(
        // `origin = 'local'` is load-bearing (#691 A-pre): a SYNCED row (projected from a
        // sibling's /3) must never be re-authored as local /3 — even if its acceptance is
        // later revoked and its projection row vanishes — or the local device would forge
        // authorship of, and re-legitimize, content the account revoked. Only
        // locally-authored rows are the reconcile's to complete.
        "SELECT m.id, m.kind, m.title, m.body, m.confidence, m.status, m.source, m.payload_json
         FROM repo_memories m
         WHERE m.repo_id = ?1
           AND m.origin = 'local'
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
