//! Durable SQLite storage for the memory op-log (phase B, §C4/§5.4).
//!
//! Two layers, persisted by the V052 migration:
//! - **Layer 1** — `oplog_entries`, the opaque signed entry log. [`append`] verifies an entry
//!   (signature + fingerprint binding), gates it on op-bytes decodability, checks per-device chain
//!   continuity against the stored tail, and inserts — all in one `IMMEDIATE` transaction,
//!   idempotent on `entry_hash`. `signed_bytes` is the sole source of truth; every header column
//!   derives from it.
//! - **Layer 2** — the shadow projection (`oplog_projected_nodes` / `oplog_projected_edges`), a
//!   pure full-replay fold of layer 1 via [`super::project::project`], rewritten wholesale inside
//!   the same transaction so the materialized view never lags the log. Never a source of truth; a
//!   `DELETE`-all
//!   + reinsert is the whole update.
//!
//! Nothing here is wired to the live memory write path yet — roster/epochs, immutable stream
//! identity, and transport are later increments; the module is exercised in isolation, mirroring
//! the `content_hash` / op-model / entry-envelope freezes that preceded it.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::device::DevicePublic;
use super::entry::{self, VerifiedEntry};
use super::op::{
    self, DecodedOp, DeviceFingerprint, EdgeKey, EdgeSpec, Entry, NodeContent, NodeId, NodeStatus,
    OpMeta, ResolvedAnchor,
};
use super::project::{self, ProjectedEdge, ProjectedNode, ProjectedState};
use crate::query::memory::EdgeRelation;

/// Bump when the fold's projectable set or LWW semantics change (a new op kind becomes `Known`, a
/// register is added). A shadow projection stamped with an older version is re-folded on demand
/// ([`reproject_if_projector_stale`], the §5.4 upgrade re-fold), never trusted incrementally.
const PROJECTOR_VERSION: i64 = 1;

/// The `oplog_meta` key holding the projector version the shadow tables were last folded by.
const PROJECTOR_VERSION_KEY: &str = "projector_version";

/// The result of an [`append`] attempt. `Appended` / `AlreadyPresent` mean the log now contains the
/// entry; `MissingPredecessor` / `Fork` mean it was rejected WITHOUT mutating the log or
/// projection, so a caller (phase D) can discriminate a retry-after-backfill from an equivocation
/// to quarantine. A cryptographic failure, an undecodable op, or a lamport overflow is an `Err`,
/// not an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppendOutcome {
    /// Verified, chain-continuous, newly inserted; the projection was re-folded in the same txn.
    Appended { entry_hash: [u8; 32] },
    /// `entry_hash` already stored — an idempotent redelivery; nothing changed.
    AlreadyPresent { entry_hash: [u8; 32] },
    /// `lamport` advances past the tail but `prev_hash` does not point at it (a gap): the
    /// predecessor has not arrived. Routine under out-of-order delivery — the caller retries
    /// after backfill.
    MissingPredecessor { entry_hash: [u8; 32] },
    /// The entry conflicts with the stored chain (a second genesis, or a `lamport` at/behind the
    /// tail) — an equivocation. `conflicting` is the stored entry it collides with, kept as
    /// evidence (richer fork forensics — a quarantine table — is a later S2 increment).
    Fork { entry_hash: [u8; 32], conflicting: Vec<u8> },
}

/// The head of one device's chain — its highest-`lamport` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChainTail {
    pub(crate) lamport: u64,
    pub(crate) entry_hash: [u8; 32],
}

/// Verify and durably append one signed entry under `pubkey`, keeping the shadow projection in sync
/// atomically. `now_ms` is the injected local receipt time (not protocol ordering). See
/// [`AppendOutcome`] for the accept/reject cases; a tampered/wrong-keyed entry, an undecodable op,
/// a lamport that overflows `i64`, or a store whose projection a NEWER rag-rat already owns is an
/// `Err`.
pub(crate) fn append(
    conn: &Connection,
    signed_bytes: &[u8],
    pubkey: &DevicePublic,
    now_ms: i64,
) -> anyhow::Result<AppendOutcome> {
    // 1. Cryptographic verification: signature over the canonical body + the pubkey↔fingerprint
    //    binding. A tampered body/header/signature or a wrong key is a hard error.
    let verified = entry::verify_signed(signed_bytes, pubkey)?;

    // 2. Poison guard (§5.4): the op bytes must be DECODABLE before accepting an entry whose
    //    projection will decode them. `Known` AND `Unknown` both pass — an unknown kind/relation/
    //    status stays retained-but-unprojected. Only a HARD decode error (structural corruption, or
    //    a deliberate `rag-rat/op/2` domain bump this binary can't read) is rejected here, so one
    //    poison entry can never wedge every future reproject.
    op::decode(&verified.op_bytes)
        .context("op-log entry carries undecodable op bytes; refusing to append")?;

    // 3. Lamport must fit SQLite's signed INTEGER; `as i64` would wrap a >= 2^63 value negative and
    //    silently corrupt the chain order. No realistic Lamport reaches this — reject, don't trust.
    let lamport = i64::try_from(verified.lamport)
        .map_err(|_| anyhow::anyhow!("op-log lamport {} exceeds i64", verified.lamport))?;

    let device = verified.device_fingerprint;
    // IMMEDIATE so the tail read and the insert are one write transaction — no TOCTOU between the
    // continuity check and the append. Dropping the txn (an early return below) rolls back.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;

    // Refuse to touch a store whose projection a NEWER rag-rat already owns: reprojecting with this
    // older decoder would drop ops the newer binary knows and stamp the version DOWN (§5.4
    // projector monotonicity). Read inside the txn so the check and the write are atomic.
    assert_projector_not_newer(&tx)?;

    // Idempotency BEFORE continuity (a re-delivered mid-chain entry must not read as a fork).
    if entry_exists(&tx, &verified.entry_hash)? {
        return Ok(AppendOutcome::AlreadyPresent { entry_hash: verified.entry_hash });
    }

    let tail = chain_tail(&tx, device)?;
    match classify_chain(&tx, device, &verified, tail.as_ref())? {
        ChainVerdict::MissingPredecessor => {
            return Ok(AppendOutcome::MissingPredecessor { entry_hash: verified.entry_hash });
        },
        ChainVerdict::Fork => {
            let conflicting = conflicting_entry(&tx, device, lamport)?;
            return Ok(AppendOutcome::Fork { entry_hash: verified.entry_hash, conflicting });
        },
        ChainVerdict::Continuous => {},
    }

    insert_entry(&tx, &verified, lamport, signed_bytes, now_ms)?;
    reproject(&tx)?;
    stamp_projector_version(&tx)?;
    tx.commit()?;
    Ok(AppendOutcome::Appended { entry_hash: verified.entry_hash })
}

/// Where an incoming entry sits relative to a device's stored chain.
enum ChainVerdict {
    /// Genesis onto an empty chain, or `prev_hash == tail` with `lamport` advancing.
    Continuous,
    /// References a predecessor this device's log does NOT hold — a genuine gap the predecessor may
    /// still backfill. (A predecessor we DO hold but that isn't the head is a `Fork`, not this.)
    MissingPredecessor,
    /// A second genesis, a `lamport` at/behind the head, or a branch off a present non-head entry —
    /// an equivocation.
    Fork,
}

/// Classify `verified` against the device's `tail`. The rule is [`super::entry::verify_chain`]'s
/// per-device chain, applied one entry at a time: genesis has `prev_hash == None`; a follow-on
/// points `prev_hash` at the head and strictly advances `lamport`. Only ONE rejection is a
/// retryable gap — a `lamport` PAST the tail whose (absent) predecessor may still backfill;
/// everything else that can never become a valid extension is a `Fork`. The present-vs-absent
/// predecessor split needs a DB read.
fn classify_chain(
    conn: &Connection,
    device: DeviceFingerprint,
    verified: &VerifiedEntry,
    tail: Option<&ChainTail>,
) -> anyhow::Result<ChainVerdict> {
    let Some(tail) = tail else {
        // Empty chain: genesis is continuous; anything naming a predecessor is a gap (the
        // predecessor, genesis included, has not arrived yet).
        return Ok(match verified.prev_hash {
            None => ChainVerdict::Continuous,
            Some(_) => ChainVerdict::MissingPredecessor,
        });
    };
    let Some(prev) = verified.prev_hash else {
        // A second genesis for a device that already has a chain — an equivocation.
        return Ok(ChainVerdict::Fork);
    };
    // A `lamport` at or below the tail can NEVER become a valid extension (a continuation must
    // strictly advance past the tail), so it is a permanent equivocation regardless of whether its
    // predecessor is present — decide this BEFORE the gap check.
    if verified.lamport <= tail.lamport {
        return Ok(ChainVerdict::Fork);
    }
    Ok(if prev == tail.entry_hash {
        ChainVerdict::Continuous // extends the head
    } else if predecessor_present(conn, device, &prev)? {
        ChainVerdict::Fork // branches off a present non-head entry
    } else {
        ChainVerdict::MissingPredecessor // predecessor genuinely absent — a later entry may backfill
    })
}

/// Whether this device's log already holds the entry `prev_hash` points at.
fn predecessor_present(
    conn: &Connection,
    device: DeviceFingerprint,
    prev_hash: &[u8; 32],
) -> rusqlite::Result<bool> {
    let device_bytes = device.to_bytes();
    Ok(conn
        .query_row(
            "SELECT 1 FROM oplog_entries WHERE device_fingerprint = ?1 AND entry_hash = ?2",
            params![device_bytes.as_slice(), prev_hash.as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn insert_entry(
    tx: &Transaction<'_>,
    verified: &VerifiedEntry,
    lamport: i64,
    signed_bytes: &[u8],
    now_ms: i64,
) -> rusqlite::Result<()> {
    let device_bytes = verified.device_fingerprint.to_bytes();
    let prev_hash: Option<Vec<u8>> = verified.prev_hash.map(|h| h.to_vec());
    tx.execute(
        "INSERT INTO oplog_entries(
             entry_hash, device_fingerprint, lamport, prev_hash, signed_bytes, received_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            verified.entry_hash.as_slice(),
            device_bytes.as_slice(),
            lamport,
            prev_hash,
            signed_bytes,
            now_ms,
        ],
    )?;
    Ok(())
}

/// The device's highest-`lamport` entry, or `None` for an empty chain.
pub(crate) fn chain_tail(
    conn: &Connection,
    device: DeviceFingerprint,
) -> anyhow::Result<Option<ChainTail>> {
    let device_bytes = device.to_bytes();
    let row = conn
        .query_row(
            "SELECT lamport, entry_hash FROM oplog_entries
             WHERE device_fingerprint = ?1 ORDER BY lamport DESC LIMIT 1",
            params![device_bytes.as_slice()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    row.map(|(lamport, hash)| {
        Ok(ChainTail {
            lamport: u64::try_from(lamport).context("stored lamport is negative")?,
            entry_hash: hash_from_vec(hash)?,
        })
    })
    .transpose()
}

fn entry_exists(conn: &Connection, entry_hash: &[u8; 32]) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM oplog_entries WHERE entry_hash = ?1",
            params![entry_hash.as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The stored entry an incoming one collides with: prefer the entry in the same `(device, lamport)`
/// slot (the direct equivocation), else the device's current head. Best-effort evidence.
fn conflicting_entry(
    conn: &Connection,
    device: DeviceFingerprint,
    lamport: i64,
) -> anyhow::Result<Vec<u8>> {
    let device_bytes = device.to_bytes();
    let at_slot: Option<Vec<u8>> = conn
        .query_row(
            "SELECT signed_bytes FROM oplog_entries
             WHERE device_fingerprint = ?1 AND lamport = ?2",
            params![device_bytes.as_slice(), lamport],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(bytes) = at_slot {
        return Ok(bytes);
    }
    let head: Option<Vec<u8>> = conn
        .query_row(
            "SELECT signed_bytes FROM oplog_entries
             WHERE device_fingerprint = ?1 ORDER BY lamport DESC LIMIT 1",
            params![device_bytes.as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(head.unwrap_or_default())
}

/// Rebuild the shadow projection from the whole log in one `IMMEDIATE` txn, and stamp the projector
/// version. The standalone entry point for the §5.4 upgrade re-fold and for a batch-append caller.
/// Refuses (like [`append`]) if a NEWER projector already owns the projection.
pub(crate) fn rebuild_projection(conn: &Connection) -> anyhow::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    assert_projector_not_newer(&tx)?;
    reproject(&tx)?;
    stamp_projector_version(&tx)?;
    tx.commit()?;
    Ok(())
}

/// Re-fold the shadow projection iff it was last folded by a STRICTLY OLDER (or missing) projector
/// version (§5.4: a binary that learns a new op kind must re-fold WITHOUT waiting for a write).
/// Returns whether it re-folded. A projection stamped by the current or a NEWER projector is left
/// intact — never downgraded. (The mechanism; wiring this into the index open path lands when the
/// store meets the live read path.)
pub(crate) fn reproject_if_projector_stale(conn: &Connection) -> anyhow::Result<bool> {
    match stored_projector_version(conn)? {
        Some(version) if version >= PROJECTOR_VERSION => Ok(false),
        _ => {
            rebuild_projection(conn)?;
            Ok(true)
        },
    }
}

/// Error if a NEWER projector already folded this store's projection — an older binary must not
/// reproject (it would drop ops the newer binary knows) or stamp the version down. Mirrors the
/// schema ladder's "newer rag-rat" refusal, at the projection layer (a projector bump need not
/// carry a schema bump, so the schema guard does not cover it).
fn assert_projector_not_newer(conn: &Connection) -> anyhow::Result<()> {
    if let Some(stored) = stored_projector_version(conn)?
        && stored > PROJECTOR_VERSION
    {
        anyhow::bail!(
            "op-log projection was folded by a newer rag-rat (projector v{stored} > \
             v{PROJECTOR_VERSION}); upgrade to write this store"
        );
    }
    Ok(())
}

/// The full-replay fold: decode the projectable entries, `project`, and rewrite BOTH shadow tables.
/// Deterministic and side-effect-free beyond the two tables; the O(n) cost per call is bounded
/// later by snapshot compaction (§5.6).
fn reproject(tx: &Transaction<'_>) -> anyhow::Result<()> {
    let entries = load_known_entries(tx)?;
    let state = project::project(&entries);
    tx.execute("DELETE FROM oplog_projected_nodes", [])?;
    tx.execute("DELETE FROM oplog_projected_edges", [])?;
    for (node_id, node) in &state.nodes {
        let content_json = serde_json::to_string(&NodeContentRow::from(&node.content))
            .context("serialize projected node content")?;
        tx.execute(
            "INSERT INTO oplog_projected_nodes(node_id, content_json, status) VALUES (?1, ?2, ?3)",
            params![node_id.as_str(), content_json, node.status.as_db_str()],
        )?;
    }
    for (edge_key, edge) in &state.edges {
        let spec_json = serde_json::to_string(&EdgeSpecRow::from(&edge.spec))
            .context("serialize projected edge spec")?;
        let resolved_json = edge
            .resolved
            .as_ref()
            .map(|resolved| serde_json::to_string(&ResolvedAnchorRow::from(resolved)))
            .transpose()
            .context("serialize projected edge resolved anchor")?;
        tx.execute(
            "INSERT INTO oplog_projected_edges(edge_key, spec_json, resolved_json)
             VALUES (?1, ?2, ?3)",
            params![edge_key.as_str(), spec_json, resolved_json],
        )?;
    }
    Ok(())
}

/// Load the PROJECTABLE entries — every stored op decoded to `Known`. An `Unknown` op is retained
/// in the log but skipped here (§5.4), so this is a strict subset of the full log (hence
/// `_known_`). A hard decode failure is unreachable given [`append`]'s accept-gate and surfaces as
/// a loud error (corruption at rest), never a silent skip.
fn load_known_entries(tx: &Transaction<'_>) -> anyhow::Result<Vec<Entry>> {
    let mut stmt =
        tx.prepare("SELECT device_fingerprint, lamport, signed_bytes FROM oplog_entries")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (device_bytes, lamport, signed_bytes) = row?;
        let device = DeviceFingerprint::from_bytes(hash_from_vec(device_bytes)?);
        let lamport = u64::try_from(lamport).context("stored lamport is negative")?;
        // `decode_signed` is structure-only (no crypto) — recovers the opaque op bytes without
        // re-verifying the signature. `signed_bytes` is the single source of truth.
        let op_bytes = entry::decode_signed(&signed_bytes)
            .context("stored signed entry failed to decode")?
            .entry
            .op_bytes;
        match op::decode(&op_bytes).context("stored op bytes failed to decode")? {
            DecodedOp::Known(op) => entries.push(Entry { meta: OpMeta { lamport, device }, op }),
            DecodedOp::Unknown { .. } => {}, // retained in the log, not projected (§5.4)
        }
    }
    Ok(entries)
}

/// Reconstruct the converged projection from the shadow tables — the read the eventual live path
/// consumes, and the round-trip for idempotency tests (compare parsed `ProjectedState`, never JSON
/// text, so serde_json key order is irrelevant).
pub(crate) fn load_projection(conn: &Connection) -> anyhow::Result<ProjectedState> {
    let mut state = ProjectedState::default();
    {
        let mut stmt =
            conn.prepare("SELECT node_id, content_json, status FROM oplog_projected_nodes")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        for row in rows {
            let (node_id, content_json, status) = row?;
            let content: NodeContentRow = serde_json::from_str(&content_json)
                .context("deserialize projected node content")?;
            let status = NodeStatus::from_db_str(&status)
                .ok_or_else(|| anyhow::anyhow!("unknown projected node status: {status}"))?;
            state.nodes.insert(NodeId::from(node_id.as_str()), ProjectedNode {
                content: NodeContent::from(content),
                status,
            });
        }
    }
    {
        let mut stmt =
            conn.prepare("SELECT edge_key, spec_json, resolved_json FROM oplog_projected_edges")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (edge_key, spec_json, resolved_json) = row?;
            let spec: EdgeSpecRow =
                serde_json::from_str(&spec_json).context("deserialize projected edge spec")?;
            let spec = EdgeSpec::try_from(spec)?;
            let resolved = resolved_json
                .map(|json| {
                    serde_json::from_str::<ResolvedAnchorRow>(&json)
                        .context("deserialize projected resolved anchor")
                        .map(ResolvedAnchor::from)
                })
                .transpose()?;
            state.edges.insert(EdgeKey::from(edge_key), ProjectedEdge { spec, resolved });
        }
    }
    Ok(state)
}

fn stamp_projector_version(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![PROJECTOR_VERSION_KEY, PROJECTOR_VERSION.to_string()],
    )?;
    Ok(())
}

fn stored_projector_version(conn: &Connection) -> anyhow::Result<Option<i64>> {
    conn.query_row(
        "SELECT value FROM oplog_meta WHERE key = ?1",
        params![PROJECTOR_VERSION_KEY],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|value| value.parse::<i64>().context("oplog projector_version is not an integer"))
    .transpose()
}

fn hash_from_vec(bytes: Vec<u8>) -> anyhow::Result<[u8; 32]> {
    let len = bytes.len();
    bytes.try_into().map_err(|_| anyhow::anyhow!("expected a 32-byte value, got {len} bytes"))
}

// The shadow-table serialization DTOs. serde lives HERE, never on the frozen op-wire types (whose
// wire is minicbor via `op::encode`); these tables are local, derived, and rebuilt wholesale.

#[derive(Serialize, Deserialize)]
struct NodeContentRow {
    kind: String,
    title: String,
    body: String,
    confidence: String,
    source: String,
    tags: Vec<String>,
    payload: Option<String>,
}

impl From<&NodeContent> for NodeContentRow {
    fn from(content: &NodeContent) -> Self {
        Self {
            kind: content.kind.clone(),
            title: content.title.clone(),
            body: content.body.clone(),
            confidence: content.confidence.clone(),
            source: content.source.clone(),
            tags: content.tags.clone(),
            payload: content.payload.clone(),
        }
    }
}

impl From<NodeContentRow> for NodeContent {
    fn from(row: NodeContentRow) -> Self {
        Self {
            kind: row.kind,
            title: row.title,
            body: row.body,
            confidence: row.confidence,
            source: row.source,
            tags: row.tags,
            payload: row.payload,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct EdgeSpecRow {
    source_node_id: String,
    relation: String,
    target_repo_id: String,
    target_kind: String,
    target_anchor: String,
    owner_repo_id: String,
}

impl From<&EdgeSpec> for EdgeSpecRow {
    fn from(edge: &EdgeSpec) -> Self {
        Self {
            source_node_id: edge.source_node_id.as_str().to_string(),
            relation: edge.relation.as_db_str().to_string(),
            target_repo_id: edge.target_repo_id.clone(),
            target_kind: edge.target_kind.clone(),
            target_anchor: edge.target_anchor.clone(),
            owner_repo_id: edge.owner_repo_id.clone(),
        }
    }
}

impl TryFrom<EdgeSpecRow> for EdgeSpec {
    type Error = anyhow::Error;

    fn try_from(row: EdgeSpecRow) -> anyhow::Result<Self> {
        Ok(Self {
            source_node_id: NodeId::from(row.source_node_id.as_str()),
            relation: EdgeRelation::from_db_str(&row.relation)?,
            target_repo_id: row.target_repo_id,
            target_kind: row.target_kind,
            target_anchor: row.target_anchor,
            owner_repo_id: row.owner_repo_id,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct ResolvedAnchorRow {
    target_repo_id: String,
    target_node_id: Option<String>,
    anchor_status: String,
}

impl From<&ResolvedAnchor> for ResolvedAnchorRow {
    fn from(resolved: &ResolvedAnchor) -> Self {
        Self {
            target_repo_id: resolved.target_repo_id.clone(),
            target_node_id: resolved.target_node_id.clone(),
            anchor_status: resolved.anchor_status.clone(),
        }
    }
}

impl From<ResolvedAnchorRow> for ResolvedAnchor {
    fn from(row: ResolvedAnchorRow) -> Self {
        Self {
            target_repo_id: row.target_repo_id,
            target_node_id: row.target_node_id,
            anchor_status: row.anchor_status,
        }
    }
}

#[cfg(test)]
mod tests {
    use minicbor::Encoder;
    use rusqlite::Connection;

    use super::super::device::DeviceSecret;
    use super::super::entry;
    use super::super::op::MemoryOp;
    use super::*;
    use crate::index::schema;

    /// The frozen op-wire domain tag — hardcoded here (it is private to `op`) to build the
    /// unknown-op and poison payloads the typed `sign_entry` can't.
    const OP_DOMAIN: &str = "rag-rat/op/1";

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn secret(seed: u8) -> DeviceSecret {
        DeviceSecret::from_seed(&[seed; 32])
    }

    fn content(title: &str) -> NodeContent {
        NodeContent {
            kind: "Invariant".to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            source: "agent".to_string(),
            tags: Vec::new(),
            payload: None,
        }
    }

    fn create(id: &str, title: &str) -> MemoryOp {
        MemoryOp::NodeCreate { node_id: NodeId::from(id), content: content(title) }
    }

    /// The `op::Entry` an appended signed entry projects as — for building the expected `project`.
    fn entry_of(secret: &DeviceSecret, lamport: u64, op: MemoryOp) -> Entry {
        Entry { meta: OpMeta { lamport, device: secret.public().fingerprint() }, op }
    }

    fn entry_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM oplog_entries", [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn append_genesis_then_chain_roundtrips() {
        let conn = db();
        let s = secret(1);
        let g = entry::sign_entry(&s, None, 1, &create("mem_a", "first"));
        let out = append(&conn, &g.signed_bytes, &s.public(), 1_000).unwrap();
        assert!(matches!(out, AppendOutcome::Appended { .. }));

        let e2 = entry::sign_entry(&s, Some(g.entry.entry_hash), 4, &create("mem_b", "second"));
        append(&conn, &e2.signed_bytes, &s.public(), 1_001).unwrap();
        let e3 = entry::sign_entry(&s, Some(e2.entry.entry_hash), 9, &create("mem_c", "third"));
        append(&conn, &e3.signed_bytes, &s.public(), 1_002).unwrap();

        let tail = chain_tail(&conn, s.public().fingerprint()).unwrap().expect("chain has a head");
        assert_eq!(tail.lamport, 9);
        assert_eq!(tail.entry_hash, e3.entry.entry_hash);
        assert_eq!(entry_count(&conn), 3);

        let projected = load_projection(&conn).unwrap();
        assert_eq!(projected.nodes.len(), 3);
        assert!(projected.nodes.contains_key(&NodeId::from("mem_a")));
    }

    #[test]
    fn reappend_is_idempotent() {
        let conn = db();
        let s = secret(2);
        let g = entry::sign_entry(&s, None, 1, &create("mem_a", "first"));
        append(&conn, &g.signed_bytes, &s.public(), 1_000).unwrap();
        let again = append(&conn, &g.signed_bytes, &s.public(), 1_005).unwrap();
        assert_eq!(again, AppendOutcome::AlreadyPresent { entry_hash: g.entry.entry_hash });
        assert_eq!(entry_count(&conn), 1);
    }

    #[test]
    fn missing_predecessor_rejections() {
        let conn = db();
        let s = secret(3);
        // A follow-on that references a predecessor we don't hold (no genesis appended yet).
        let orphan = entry::sign_entry(&s, Some([7u8; 32]), 2, &create("mem_a", "orphan"));
        assert!(matches!(
            append(&conn, &orphan.signed_bytes, &s.public(), 1_000).unwrap(),
            AppendOutcome::MissingPredecessor { .. }
        ));
        assert_eq!(entry_count(&conn), 0);

        // Genesis, then a follow-on that advances lamport but points prev at the wrong hash (a
        // gap).
        let g = entry::sign_entry(&s, None, 1, &create("mem_a", "first"));
        append(&conn, &g.signed_bytes, &s.public(), 1_001).unwrap();
        let gap = entry::sign_entry(&s, Some([9u8; 32]), 5, &create("mem_b", "gap"));
        assert!(matches!(
            append(&conn, &gap.signed_bytes, &s.public(), 1_002).unwrap(),
            AppendOutcome::MissingPredecessor { .. }
        ));
        assert_eq!(entry_count(&conn), 1);
    }

    #[test]
    fn fork_rejections_keep_evidence() {
        let conn = db();
        let s = secret(4);
        let g = entry::sign_entry(&s, None, 5, &create("mem_a", "first"));
        append(&conn, &g.signed_bytes, &s.public(), 1_000).unwrap();

        // A second genesis for the same device is an equivocation.
        let second_genesis = entry::sign_entry(&s, None, 6, &create("mem_b", "genesis-2"));
        let out = append(&conn, &second_genesis.signed_bytes, &s.public(), 1_001).unwrap();
        match out {
            AppendOutcome::Fork { conflicting, .. } => {
                assert_eq!(conflicting, g.signed_bytes, "the stored genesis is the evidence");
            },
            other => panic!("expected Fork, got {other:?}"),
        }

        // A follow-on that chains onto the head but does not advance lamport rewrites history.
        let stale = entry::sign_entry(&s, Some(g.entry.entry_hash), 5, &create("mem_c", "stale"));
        assert!(matches!(
            append(&conn, &stale.signed_bytes, &s.public(), 1_002).unwrap(),
            AppendOutcome::Fork { .. }
        ));

        // A stale entry (lamport at/below the tail) whose predecessor we don't even hold can never
        // become a valid extension either — it is a permanent equivocation, NOT a retryable gap.
        let stale_absent =
            entry::sign_entry(&s, Some([3u8; 32]), 4, &create("mem_d", "stale-absent"));
        assert!(matches!(
            append(&conn, &stale_absent.signed_bytes, &s.public(), 1_003).unwrap(),
            AppendOutcome::Fork { .. }
        ));

        assert_eq!(entry_count(&conn), 1, "no fork was stored");
    }

    #[test]
    fn branch_off_a_present_non_head_entry_is_a_fork_not_a_gap() {
        let conn = db();
        let s = secret(13);
        // Build a chain A -> B.
        let a = entry::sign_entry(&s, None, 1, &create("mem_a", "a"));
        append(&conn, &a.signed_bytes, &s.public(), 1).unwrap();
        let b = entry::sign_entry(&s, Some(a.entry.entry_hash), 2, &create("mem_b", "b"));
        append(&conn, &b.signed_bytes, &s.public(), 2).unwrap();

        // C forks off A — a PRESENT predecessor that is not the head B — at a higher lamport. The
        // predecessor exists, so backfill can never resolve it: this is an equivocation, not a gap.
        let c = entry::sign_entry(&s, Some(a.entry.entry_hash), 3, &create("mem_c", "c"));
        match append(&conn, &c.signed_bytes, &s.public(), 3).unwrap() {
            AppendOutcome::Fork { conflicting, .. } => {
                assert!(!conflicting.is_empty(), "the colliding head is kept as evidence");
            },
            other => panic!("expected Fork for a branch off a present entry, got {other:?}"),
        }
        assert_eq!(entry_count(&conn), 2, "the fork was not stored");
    }

    #[test]
    fn tamper_and_wrong_key_are_hard_errors() {
        let conn = db();
        let s = secret(5);
        let g = entry::sign_entry(&s, None, 1, &create("mem_a", "first"));

        let mut tampered = g.signed_bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(append(&conn, &tampered, &s.public(), 1_000).is_err());

        // A valid entry verified under the wrong device key.
        let wrong = secret(6);
        assert!(append(&conn, &g.signed_bytes, &wrong.public(), 1_000).is_err());
        assert_eq!(entry_count(&conn), 0);
    }

    #[test]
    fn poison_op_bytes_are_rejected_at_accept() {
        let conn = db();
        let s = secret(7);
        // A validly-SIGNED entry whose op bytes are structurally undecodable.
        let poison = entry::sign_entry_from_op_bytes(&s, None, 1, vec![0xFF]);
        assert!(
            append(&conn, &poison.signed_bytes, &s.public(), 1_000).is_err(),
            "an undecodable op must not enter the log (it would wedge every future reproject)"
        );
        assert_eq!(entry_count(&conn), 0);
    }

    #[test]
    fn unknown_op_stores_and_chains_but_is_not_projected() {
        let conn = db();
        let s = secret(8);
        // A canonical `rag-rat/op/1` envelope with a kind tag this binary doesn't know → decodes to
        // `Unknown`, so it passes the accept-gate, stores, and chains — but never projects (§5.4).
        let mut op_bytes = Vec::new();
        {
            let mut enc = Encoder::new(&mut op_bytes);
            enc.array(3).unwrap();
            enc.str(OP_DOMAIN).unwrap();
            enc.str("future_kind").unwrap();
            enc.null().unwrap();
        }
        let unknown = entry::sign_entry_from_op_bytes(&s, None, 1, op_bytes);
        assert!(matches!(
            append(&conn, &unknown.signed_bytes, &s.public(), 1_000).unwrap(),
            AppendOutcome::Appended { .. }
        ));
        // It chains: a follow-on onto it is accepted.
        let follow =
            entry::sign_entry(&s, Some(unknown.entry.entry_hash), 2, &create("mem_a", "x"));
        append(&conn, &follow.signed_bytes, &s.public(), 1_001).unwrap();

        assert_eq!(entry_count(&conn), 2, "both entries are in the log");
        let projected = load_projection(&conn).unwrap();
        assert_eq!(projected.nodes.len(), 1, "only the known op projected");
        assert!(projected.nodes.contains_key(&NodeId::from("mem_a")));
    }

    #[test]
    fn projection_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let s = secret(9);

        // Two devices interleave; a non-null payload round-trips; a status flip is folded.
        let t = secret(10);
        let g = entry::sign_entry(&s, None, 1, &{
            let mut c = content("payload node");
            c.payload = Some(r#"{"schema_version":1,"n":42}"#.to_string());
            MemoryOp::NodeCreate { node_id: NodeId::from("mem_a"), content: c }
        });
        let g_other = entry::sign_entry(&t, None, 2, &create("mem_b", "other-device"));
        let status = entry::sign_entry(&s, Some(g.entry.entry_hash), 3, &MemoryOp::NodeStatus {
            node_id: NodeId::from("mem_a"),
            status: NodeStatus::Stale,
        });

        let expected;
        {
            let conn = Connection::open(&path).unwrap();
            schema::apply(&conn).unwrap();
            append(&conn, &g.signed_bytes, &s.public(), 1).unwrap();
            append(&conn, &g_other.signed_bytes, &t.public(), 2).unwrap();
            append(&conn, &status.signed_bytes, &s.public(), 3).unwrap();
            expected = load_projection(&conn).unwrap();
        }

        // Reopen a fresh connection to the same file: the projection persisted verbatim.
        let reopened = Connection::open(&path).unwrap();
        let loaded = load_projection(&reopened).unwrap();
        assert_eq!(loaded, expected);

        // …and it equals a from-scratch in-memory fold of the same ops.
        let in_memory = project::project(&[
            {
                let mut c = content("payload node");
                c.payload = Some(r#"{"schema_version":1,"n":42}"#.to_string());
                Entry {
                    meta: OpMeta { lamport: 1, device: s.public().fingerprint() },
                    op: MemoryOp::NodeCreate { node_id: NodeId::from("mem_a"), content: c },
                }
            },
            entry_of(&t, 2, create("mem_b", "other-device")),
            Entry {
                meta: OpMeta { lamport: 3, device: s.public().fingerprint() },
                op: MemoryOp::NodeStatus {
                    node_id: NodeId::from("mem_a"),
                    status: NodeStatus::Stale,
                },
            },
        ]);
        assert_eq!(loaded, in_memory);
    }

    #[test]
    fn upgrade_refold_rebuilds_a_stale_projection() {
        let conn = db();
        let s = secret(11);
        let g = entry::sign_entry(&s, None, 1, &create("mem_a", "first"));
        append(&conn, &g.signed_bytes, &s.public(), 1_000).unwrap();
        assert_eq!(load_projection(&conn).unwrap().nodes.len(), 1);

        // Simulate an old binary that folded under an earlier projector version and left the shadow
        // tables stale (here: emptied).
        conn.execute_batch("DELETE FROM oplog_projected_nodes;").unwrap();
        conn.execute("UPDATE oplog_meta SET value = '0' WHERE key = ?1", params![
            PROJECTOR_VERSION_KEY
        ])
        .unwrap();

        assert!(reproject_if_projector_stale(&conn).unwrap(), "a stale stamp re-folds");
        assert_eq!(load_projection(&conn).unwrap().nodes.len(), 1, "the node is back");
        assert!(!reproject_if_projector_stale(&conn).unwrap(), "current stamp is a no-op");
    }

    #[test]
    fn append_refuses_to_downgrade_a_newer_projection() {
        let conn = db();
        let s = secret(14);
        let g = entry::sign_entry(&s, None, 1, &create("mem_a", "first"));
        append(&conn, &g.signed_bytes, &s.public(), 1).unwrap();

        // Simulate a NEWER binary having folded + stamped a higher projector version.
        conn.execute("UPDATE oplog_meta SET value = ?1 WHERE key = ?2", params![
            (PROJECTOR_VERSION + 1).to_string(),
            PROJECTOR_VERSION_KEY
        ])
        .unwrap();

        // This older projector must refuse to append (it would drop newer-known ops and stamp
        // down).
        let e2 = entry::sign_entry(&s, Some(g.entry.entry_hash), 2, &create("mem_b", "second"));
        assert!(
            append(&conn, &e2.signed_bytes, &s.public(), 2).is_err(),
            "an older projector must not write a newer-projected store"
        );
        assert_eq!(entry_count(&conn), 1, "the refused append wrote nothing");
        // The stale-check also leaves the newer projection intact (never downgrades).
        assert!(
            !reproject_if_projector_stale(&conn).unwrap(),
            "a newer projection is not re-folded"
        );
    }

    #[test]
    fn removed_edge_leaves_no_shadow_row() {
        let conn = db();
        let s = secret(12);
        let edge = EdgeSpec {
            source_node_id: NodeId::from("mem_a"),
            relation: EdgeRelation::DependsOn,
            target_repo_id: "repo".to_string(),
            target_kind: "node".to_string(),
            target_anchor: "mem_b".to_string(),
            owner_repo_id: "repo".to_string(),
        };
        let key = edge.edge_key();
        let g = entry::sign_entry(&s, None, 1, &create("mem_a", "first"));
        append(&conn, &g.signed_bytes, &s.public(), 1).unwrap();
        let add = entry::sign_entry(&s, Some(g.entry.entry_hash), 2, &MemoryOp::EdgeAdd {
            edge: edge.clone(),
        });
        let add_hash = add.entry.entry_hash;
        append(&conn, &add.signed_bytes, &s.public(), 2).unwrap();
        assert_eq!(load_projection(&conn).unwrap().edges.len(), 1);

        let remove = entry::sign_entry(&s, Some(add_hash), 3, &MemoryOp::EdgeRemove {
            edge_key: key.clone(),
        });
        append(&conn, &remove.signed_bytes, &s.public(), 3).unwrap();
        assert!(
            load_projection(&conn).unwrap().edges.is_empty(),
            "the DELETE-all reproject drops the tombstoned edge's row"
        );
    }
}
