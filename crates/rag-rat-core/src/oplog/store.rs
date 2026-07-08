//! Durable SQLite storage for the memory op-log (phase B, layer 1 + shadow projection).
//!
//! Two layers, persisted by the V052/V053 migrations, both scoped by the immutable
//! [`super::stream`] identity — one signed chain per `(stream_id, device)`:
//! - **Layer 1** — `oplog_entries`, the opaque signed entry log. [`append`] verifies an entry
//!   (signature + fingerprint binding + stream binding), gates it on op-bytes decodability, checks
//!   per-`(stream, device)` chain continuity against the stored tail, and inserts — all in one
//!   `IMMEDIATE` transaction, idempotent on `entry_hash`. `signed_bytes` is the sole source of
//!   truth; every header column derives from it. A detected fork durably quarantines the rejected
//!   head in `oplog_fork_evidence`, so BOTH heads of an equivocation survive.
//! - **Layer 2** — the shadow projection (`oplog_projected_nodes` / `oplog_projected_edges`), a
//!   pure full-replay fold of layer 1 via [`super::project::project`], rewritten per stream inside
//!   the same transaction so the materialized view never lags the log. Never a source of truth; a
//!   per-stream `DELETE` + reinsert is the whole update.
//!
//! Nothing here is wired to the live memory write path yet — roster/epochs and transport are later
//! increments; the module is exercised in isolation, mirroring the `content_hash` / op-model /
//! entry-envelope / stream-identity freezes that preceded it.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::device::DevicePublic;
use super::entry::{self, VerifiedEntry};
use super::identity::LocalDevice;
use super::op::{
    self, DecodedOp, DeviceFingerprint, EdgeKey, EdgeSpec, Entry, MemoryOp, NodeContent, NodeId,
    NodeStatus, OpMeta, ResolvedAnchor,
};
use super::project::{self, ProjectedEdge, ProjectedNode, ProjectedState};
use super::stream::StreamId;
use crate::query::memory::EdgeRelation;

/// Bump when the fold's projectable set or LWW semantics change (a new op kind becomes `Known`, a
/// register is added). A shadow projection stamped with an older version is re-folded on demand
/// ([`reproject_if_projector_stale`], the upgrade re-fold), never trusted incrementally.
const PROJECTOR_VERSION: i64 = 1;

/// The `oplog_meta` key holding the projector version the shadow tables were last folded by.
const PROJECTOR_VERSION_KEY: &str = "projector_version";

/// The result of an [`append`] attempt. `Appended` / `AlreadyPresent` mean the log now contains the
/// entry; `MissingPredecessor` / `Fork` mean it was rejected WITHOUT mutating the log or
/// projection, so a caller (phase D) can discriminate a retry-after-backfill from an equivocation.
/// A cryptographic failure, a stream mismatch, an undecodable op, or a lamport overflow is an
/// `Err`, not an outcome.
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
    /// tail) — an equivocation. `conflicting` is the stored entry it collides with. The rejected
    /// entry is durably quarantined in `oplog_fork_evidence` (BOTH heads must survive a process
    /// exit — a cloned/restored device is exactly the case where forensics happen later); the
    /// log and projection stay untouched.
    Fork { entry_hash: [u8; 32], conflicting: Vec<u8> },
}

/// The head of one `(stream, device)` chain — its highest-`lamport` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChainTail {
    pub(crate) lamport: u64,
    pub(crate) entry_hash: [u8; 32],
}

/// Verify and durably append one signed entry under `pubkey` onto `expected_stream`, keeping the
/// shadow projection in sync atomically. `now_ms` is the injected local receipt time (not protocol
/// ordering). See [`AppendOutcome`] for the accept/reject cases; a tampered/wrong-keyed entry, an
/// entry whose signed body names a DIFFERENT stream, an undecodable op, a lamport that overflows
/// `i64`, or a store whose projection a NEWER rag-rat already owns is an `Err`.
pub(crate) fn append(
    conn: &Connection,
    expected_stream: StreamId,
    signed_bytes: &[u8],
    pubkey: &DevicePublic,
    now_ms: i64,
) -> anyhow::Result<AppendOutcome> {
    // 1. Cryptographic verification: signature over the canonical body + the pubkey↔fingerprint
    //    binding. A tampered body/header/signature or a wrong key is a hard error.
    let verified = entry::verify_signed(signed_bytes, pubkey)?;

    // 2. Stream binding: the signed body names the stream it belongs to; the caller names the
    //    stream it is accepting for. A mismatch is the cross-stream replay the in-body stream_id
    //    exists to stop (an entry re-homed onto another stream would contaminate that stream's
    //    projection past any visibility filtering) — a hard error, never stored.
    if verified.stream_id != expected_stream {
        anyhow::bail!("op-log entry belongs to a different stream than it was offered for");
    }

    // 3. Poison guard: the op bytes must be DECODABLE before accepting an entry whose projection
    //    will decode them. `Known` AND `Unknown` both pass — an unknown kind/relation/status stays
    //    retained-but-unprojected. Only a HARD decode error (structural corruption, or a deliberate
    //    `rag-rat/op/2` domain bump this binary can't read) is rejected here, so one poison entry
    //    can never wedge every future reproject.
    op::decode(&verified.op_bytes)
        .context("op-log entry carries undecodable op bytes; refusing to append")?;

    // 4. Lamport must fit SQLite's signed INTEGER; `as i64` would wrap a >= 2^63 value negative and
    //    silently corrupt the chain order. No realistic Lamport reaches this — reject, don't trust.
    let lamport = i64::try_from(verified.lamport)
        .map_err(|_| anyhow::anyhow!("op-log lamport {} exceeds i64", verified.lamport))?;

    let device = verified.device_fingerprint;
    let stream = verified.stream_id;
    // IMMEDIATE so the tail read and the insert are one write transaction — no TOCTOU between the
    // continuity check and the append. Dropping the txn (an early return below) rolls back.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;

    // Refuse to touch a store whose projection a NEWER rag-rat already owns: reprojecting with this
    // older decoder would drop ops the newer binary knows and stamp the version DOWN (projector
    // monotonicity). Read inside the txn so the check and the write are atomic.
    assert_projector_not_newer(&tx)?;

    // Idempotency BEFORE continuity (a re-delivered mid-chain entry must not read as a fork).
    if entry_exists(&tx, &verified.entry_hash)? {
        return Ok(AppendOutcome::AlreadyPresent { entry_hash: verified.entry_hash });
    }

    let tail = chain_tail(&tx, stream, device)?;
    match classify_chain(&tx, stream, device, &verified, tail.as_ref())? {
        ChainVerdict::MissingPredecessor => {
            return Ok(AppendOutcome::MissingPredecessor { entry_hash: verified.entry_hash });
        },
        ChainVerdict::Fork => {
            let conflicting = conflicting_entry(&tx, stream, device, lamport)?;
            // Quarantine the rejected head durably, then COMMIT only that: the log and projection
            // stay unmutated, but the equivocation's second head survives a process exit for later
            // forensics instead of living only in this return value.
            record_fork_evidence(
                &tx,
                &verified,
                lamport,
                signed_bytes,
                conflicting.as_ref().map(|c| c.entry_hash),
                now_ms,
            )?;
            tx.commit()?;
            return Ok(AppendOutcome::Fork {
                entry_hash: verified.entry_hash,
                conflicting: conflicting.map(|c| c.signed_bytes).unwrap_or_default(),
            });
        },
        ChainVerdict::Continuous => {},
    }

    insert_entry(&tx, &verified, lamport, signed_bytes, now_ms)?;
    reproject_after_write(&tx, stream)?;
    stamp_projector_version(&tx)?;
    tx.commit()?;
    Ok(AppendOutcome::Appended { entry_hash: verified.entry_hash })
}

/// Re-fold after a write, then the caller stamps: only the touched `stream` when the projector
/// stamp is already current, else EVERY stream. An older (or missing) stamp means every OTHER
/// stream's fold is stale too, and the store-global stamp written next would mark them all current
/// — so the write must sweep every stream, not just its own, or a quiet stream keeps its stale rows
/// forever. Shared by [`append`] and the authoring path.
fn reproject_after_write(tx: &Transaction<'_>, stream: StreamId) -> anyhow::Result<()> {
    match stored_projector_version(tx)? {
        Some(version) if version == PROJECTOR_VERSION => reproject(tx, stream)?,
        _ => reproject_all_streams(tx)?,
    }
    Ok(())
}

/// Author one NEW entry on `stream` WITHIN a caller-provided transaction: allocate the next Lamport
/// from the local chain tail, sign, insert, and re-fold the projection — but neither open nor
/// commit the txn. This is the seam a live memory mutation uses to make its row write and this
/// op-append ONE atomic unit: it cannot call the self-transacting [`author_op`] from inside its own
/// `IMMEDIATE` txn (SQLite has no nested transactions), and splitting them across two txns leaves a
/// crash window where only one side commits. Genesis when the chain is empty. Unlike [`append`]
/// (which accepts a foreign, pre-signed entry and may Fork), this MINTS a valid continuation from
/// the tail it just read, so under the single local writer it is always continuous. Returns the new
/// `entry_hash`.
pub(crate) fn author_in_tx(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: &LocalDevice,
    op: &MemoryOp,
    now_ms: i64,
) -> anyhow::Result<[u8; 32]> {
    assert_projector_not_newer(tx)?;
    let entry_hash = author_one(tx, stream, device, op, now_ms)?;
    reproject_after_write(tx, stream)?;
    stamp_projector_version(tx)?;
    Ok(entry_hash)
}

/// Author one entry in its OWN `IMMEDIATE` txn — the standalone wrapper over [`author_in_tx`] for a
/// caller that is NOT already inside a transaction (the op-append is the whole unit of work).
pub(crate) fn author_op(
    conn: &Connection,
    stream: StreamId,
    device: &LocalDevice,
    op: &MemoryOp,
    now_ms: i64,
) -> anyhow::Result<[u8; 32]> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let entry_hash = author_in_tx(&tx, stream, device, op, now_ms)?;
    tx.commit()?;
    Ok(entry_hash)
}

/// Author `ops` as the GENESIS batch of `stream` WITHIN a caller-provided txn — gate on an empty
/// chain, then chain every entry from genesis with a SINGLE projection re-fold at the end, but
/// NEITHER open NOR commit. The empty-chain gate lives INSIDE the txn (the caller's), so two
/// concurrent first-authoring callers converge: the winner authors genesis→N, and the loser —
/// serialized behind the winner's write — sees a non-empty chain and no-ops. Returns whether it
/// authored (`false` = a chain already existed). The backfill opens its OWN `IMMEDIATE` txn, reads
/// the memory snapshot UNDER that write lock, and calls this so the snapshot read + gate + write
/// are one atomic unit (no memory created between the read and the batch is lost from the history).
pub(crate) fn author_genesis_in_tx(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: &LocalDevice,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<bool> {
    assert_projector_not_newer(tx)?;
    if chain_tail(tx, stream, device.fingerprint())?.is_some() {
        return Ok(false);
    }
    for op in ops {
        // The tail read sees prior in-txn inserts, so each op chains off the one before it.
        author_one(tx, stream, device, op, now_ms)?;
    }
    reproject_after_write(tx, stream)?;
    stamp_projector_version(tx)?;
    Ok(true)
}

/// [`author_genesis_in_tx`] in its OWN `IMMEDIATE` txn — the standalone wrapper for a caller that
/// does not need to read a snapshot under the same write lock. Empty `ops` is a no-op.
pub(crate) fn author_batch(
    conn: &Connection,
    stream: StreamId,
    device: &LocalDevice,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<bool> {
    if ops.is_empty() {
        return Ok(false);
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let authored = author_genesis_in_tx(&tx, stream, device, ops, now_ms)?;
    tx.commit()?;
    Ok(authored)
}

/// Read the `(stream, device)` tail, mint the next entry for `op` (genesis when the chain is
/// empty), and INSERT it — no projection (the caller re-folds once). MUST run inside a txn so the
/// tail read sees prior in-txn inserts, which is what lets [`author_batch`] chain a whole sequence.
fn author_one(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: &LocalDevice,
    op: &MemoryOp,
    now_ms: i64,
) -> anyhow::Result<[u8; 32]> {
    let (lamport, prev_hash) = match chain_tail(tx, stream, device.fingerprint())? {
        Some(tail) => (tail.lamport + 1, Some(tail.entry_hash)),
        None => (0, None),
    };
    let signed = entry::sign_entry(device.secret(), stream, prev_hash, lamport, op);
    let lamport = i64::try_from(lamport)
        .map_err(|_| anyhow::anyhow!("op-log lamport {lamport} exceeds i64"))?;
    insert_entry(tx, &signed.entry, lamport, &signed.signed_bytes, now_ms)?;
    Ok(signed.entry.entry_hash)
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

/// Classify `verified` against the `(stream, device)` chain's `tail`. The rule is
/// [`super::entry::verify_chain`]'s per-`(stream, device)` chain, applied one entry at a time:
/// genesis has `prev_hash == None`; a follow-on points `prev_hash` at the head and strictly
/// advances `lamport`. Only ONE rejection is a retryable gap — a `lamport` PAST the tail whose
/// (absent) predecessor may still backfill; everything else that can never become a valid
/// extension is a `Fork`. The present-vs-absent predecessor split needs a DB read.
fn classify_chain(
    conn: &Connection,
    stream: StreamId,
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
    } else if predecessor_present(conn, stream, device, &prev)? {
        ChainVerdict::Fork // branches off a present non-head entry
    } else {
        ChainVerdict::MissingPredecessor // predecessor genuinely absent — a later entry may backfill
    })
}

/// Whether this `(stream, device)` chain already holds the entry `prev_hash` points at.
fn predecessor_present(
    conn: &Connection,
    stream: StreamId,
    device: DeviceFingerprint,
    prev_hash: &[u8; 32],
) -> rusqlite::Result<bool> {
    let stream_bytes = stream.to_bytes();
    let device_bytes = device.to_bytes();
    Ok(conn
        .query_row(
            "SELECT 1 FROM oplog_entries
             WHERE stream_id = ?1 AND device_fingerprint = ?2 AND entry_hash = ?3",
            params![stream_bytes.as_slice(), device_bytes.as_slice(), prev_hash.as_slice()],
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
    let stream_bytes = verified.stream_id.to_bytes();
    let device_bytes = verified.device_fingerprint.to_bytes();
    let prev_hash: Option<Vec<u8>> = verified.prev_hash.map(|h| h.to_vec());
    tx.execute(
        "INSERT INTO oplog_entries(
             entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
             received_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            verified.entry_hash.as_slice(),
            stream_bytes.as_slice(),
            device_bytes.as_slice(),
            lamport,
            prev_hash,
            signed_bytes,
            now_ms,
        ],
    )?;
    Ok(())
}

/// The `(stream, device)` chain's highest-`lamport` entry, or `None` for an empty chain.
pub(crate) fn chain_tail(
    conn: &Connection,
    stream: StreamId,
    device: DeviceFingerprint,
) -> anyhow::Result<Option<ChainTail>> {
    let stream_bytes = stream.to_bytes();
    let device_bytes = device.to_bytes();
    let row = conn
        .query_row(
            "SELECT lamport, entry_hash FROM oplog_entries
             WHERE stream_id = ?1 AND device_fingerprint = ?2 ORDER BY lamport DESC LIMIT 1",
            params![stream_bytes.as_slice(), device_bytes.as_slice()],
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

/// The stored entry a rejected fork collides with — its wire bytes plus its `entry_hash` (the
/// quarantine row points at it).
struct ConflictingEntry {
    signed_bytes: Vec<u8>,
    entry_hash: [u8; 32],
}

/// The stored entry an incoming one collides with: prefer the entry in the same `(stream, device,
/// lamport)` slot (the direct equivocation), else the chain's current head. Best-effort evidence.
fn conflicting_entry(
    conn: &Connection,
    stream: StreamId,
    device: DeviceFingerprint,
    lamport: i64,
) -> anyhow::Result<Option<ConflictingEntry>> {
    let stream_bytes = stream.to_bytes();
    let device_bytes = device.to_bytes();
    let at_slot: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT signed_bytes, entry_hash FROM oplog_entries
             WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport = ?3",
            params![stream_bytes.as_slice(), device_bytes.as_slice(), lamport],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let row = match at_slot {
        Some(row) => Some(row),
        None => conn
            .query_row(
                "SELECT signed_bytes, entry_hash FROM oplog_entries
                 WHERE stream_id = ?1 AND device_fingerprint = ?2 ORDER BY lamport DESC LIMIT 1",
                params![stream_bytes.as_slice(), device_bytes.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?,
    };
    row.map(|(signed_bytes, entry_hash)| {
        Ok(ConflictingEntry { signed_bytes, entry_hash: hash_from_vec(entry_hash)? })
    })
    .transpose()
}

/// Durably quarantine the rejected head of a detected fork. Idempotent on `(stream, entry_hash)`
/// (a redelivered fork re-reports, never duplicates); never touches the log or projection.
fn record_fork_evidence(
    tx: &Transaction<'_>,
    verified: &VerifiedEntry,
    lamport: i64,
    signed_bytes: &[u8],
    conflicting_entry_hash: Option<[u8; 32]>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let stream_bytes = verified.stream_id.to_bytes();
    let device_bytes = verified.device_fingerprint.to_bytes();
    let conflicting: Option<Vec<u8>> = conflicting_entry_hash.map(|h| h.to_vec());
    tx.execute(
        "INSERT INTO oplog_fork_evidence(
             stream_id, entry_hash, device_fingerprint, lamport, signed_bytes,
             conflicting_entry_hash, observed_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(stream_id, entry_hash) DO NOTHING",
        params![
            stream_bytes.as_slice(),
            verified.entry_hash.as_slice(),
            device_bytes.as_slice(),
            lamport,
            signed_bytes,
            conflicting,
            now_ms,
        ],
    )?;
    Ok(())
}

/// Rebuild the whole shadow projection — every stream present in the log — in one `IMMEDIATE`
/// txn, and stamp the projector version. The standalone entry point for the upgrade re-fold and
/// for a batch-append caller; a re-fold must sweep EVERY stream, not just the one a write touched,
/// or a quiet stream would serve a stale materialization after an upgrade. Refuses (like
/// [`append`]) if a NEWER projector already owns the projection.
pub(crate) fn rebuild_projection(conn: &Connection) -> anyhow::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    assert_projector_not_newer(&tx)?;
    reproject_all_streams(&tx)?;
    stamp_projector_version(&tx)?;
    tx.commit()?;
    Ok(())
}

/// Re-fold EVERY stream wholesale: clear BOTH tables first so a projection row whose stream no
/// longer has entries cannot linger, then fold each present stream. The only projection write
/// allowed to precede a projector-version stamp over a stale store.
fn reproject_all_streams(tx: &Transaction<'_>) -> anyhow::Result<()> {
    tx.execute("DELETE FROM oplog_projected_nodes", [])?;
    tx.execute("DELETE FROM oplog_projected_edges", [])?;
    for stream in streams_present(tx)? {
        reproject(tx, stream)?;
    }
    Ok(())
}

/// Every distinct stream the log currently holds entries for.
fn streams_present(conn: &Connection) -> anyhow::Result<Vec<StreamId>> {
    let mut stmt = conn.prepare("SELECT DISTINCT stream_id FROM oplog_entries")?;
    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut streams = Vec::new();
    for row in rows {
        streams.push(StreamId::from_bytes(hash_from_vec(row?)?));
    }
    Ok(streams)
}

/// Re-fold the shadow projection iff it was last folded by a STRICTLY OLDER (or missing) projector
/// version (a binary that learns a new op kind must re-fold WITHOUT waiting for a write). Returns
/// whether it re-folded. A projection stamped by the current or a NEWER projector is left intact —
/// never downgraded. (The mechanism; wiring this into the index open path lands when the store
/// meets the live read path.)
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
/// carry a schema bump, so the schema guard does not cover it). The stamp is store-global, not
/// per stream: one binary folds every stream it holds.
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

/// The full-replay fold for ONE stream: decode its projectable entries, `project`, and rewrite its
/// rows in BOTH shadow tables — another stream's projection is never touched, so a busy stream's
/// append cannot perturb a filtered view's materialization. Deterministic and side-effect-free
/// beyond the two tables; the O(n) cost per call is bounded later by snapshot compaction.
fn reproject(tx: &Transaction<'_>, stream: StreamId) -> anyhow::Result<()> {
    let entries = load_known_entries(tx, stream)?;
    let state = project::project(&entries);
    let stream_bytes = stream.to_bytes();
    tx.execute("DELETE FROM oplog_projected_nodes WHERE stream_id = ?1", params![
        stream_bytes.as_slice()
    ])?;
    tx.execute("DELETE FROM oplog_projected_edges WHERE stream_id = ?1", params![
        stream_bytes.as_slice()
    ])?;
    for (node_id, node) in &state.nodes {
        let content_json = serde_json::to_string(&NodeContentRow::from(&node.content))
            .context("serialize projected node content")?;
        tx.execute(
            "INSERT INTO oplog_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                stream_bytes.as_slice(),
                node_id.as_str(),
                content_json,
                node.status.as_db_str()
            ],
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
            "INSERT INTO oplog_projected_edges(stream_id, edge_key, spec_json, resolved_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![stream_bytes.as_slice(), edge_key.as_str(), spec_json, resolved_json],
        )?;
    }
    Ok(())
}

/// Load ONE stream's PROJECTABLE entries — every stored op decoded to `Known`. An `Unknown` op is
/// retained in the log but skipped here, so this is a strict subset of the stream's log (hence
/// `_known_`). A hard decode failure is unreachable given [`append`]'s accept-gate and surfaces as
/// a loud error (corruption at rest), never a silent skip.
fn load_known_entries(tx: &Transaction<'_>, stream: StreamId) -> anyhow::Result<Vec<Entry>> {
    let stream_bytes = stream.to_bytes();
    let mut stmt = tx.prepare(
        "SELECT device_fingerprint, lamport, signed_bytes FROM oplog_entries
         WHERE stream_id = ?1",
    )?;
    let rows = stmt.query_map(params![stream_bytes.as_slice()], |row| {
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
            DecodedOp::Unknown { .. } => {}, // retained in the log, not projected
        }
    }
    Ok(entries)
}

/// Reconstruct ONE stream's converged projection from the shadow tables — the read the eventual
/// live path consumes, and the round-trip for idempotency tests (compare parsed `ProjectedState`,
/// never JSON text, so serde_json key order is irrelevant).
pub(crate) fn load_projection(
    conn: &Connection,
    stream: StreamId,
) -> anyhow::Result<ProjectedState> {
    let stream_bytes = stream.to_bytes();
    let mut state = ProjectedState::default();
    {
        let mut stmt = conn.prepare(
            "SELECT node_id, content_json, status FROM oplog_projected_nodes
             WHERE stream_id = ?1",
        )?;
        let rows = stmt.query_map(params![stream_bytes.as_slice()], |row| {
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
        let mut stmt = conn.prepare(
            "SELECT edge_key, spec_json, resolved_json FROM oplog_projected_edges
             WHERE stream_id = ?1",
        )?;
        let rows = stmt.query_map(params![stream_bytes.as_slice()], |row| {
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

    /// The stream the tests append onto, plus a second one for cross-stream isolation cases.
    fn stream_a() -> StreamId {
        StreamId::from_bytes([0xAA; 32])
    }

    fn stream_b() -> StreamId {
        StreamId::from_bytes([0xBB; 32])
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

    /// Every quarantined fork head: `(entry_hash, signed_bytes, conflicting_entry_hash)`.
    #[allow(clippy::type_complexity)]
    fn fork_evidence(conn: &Connection) -> Vec<(Vec<u8>, Vec<u8>, Option<Vec<u8>>)> {
        let mut stmt = conn
            .prepare(
                "SELECT entry_hash, signed_bytes, conflicting_entry_hash FROM oplog_fork_evidence
                 ORDER BY lamport",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn append_genesis_then_chain_roundtrips() {
        let conn = db();
        let s = secret(1);
        let g = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "first"));
        let out = append(&conn, stream_a(), &g.signed_bytes, &s.public(), 1_000).unwrap();
        assert!(matches!(out, AppendOutcome::Appended { .. }));

        let e2 = entry::sign_entry(
            &s,
            stream_a(),
            Some(g.entry.entry_hash),
            4,
            &create("mem_b", "second"),
        );
        append(&conn, stream_a(), &e2.signed_bytes, &s.public(), 1_001).unwrap();
        let e3 = entry::sign_entry(
            &s,
            stream_a(),
            Some(e2.entry.entry_hash),
            9,
            &create("mem_c", "third"),
        );
        append(&conn, stream_a(), &e3.signed_bytes, &s.public(), 1_002).unwrap();

        let tail = chain_tail(&conn, stream_a(), s.public().fingerprint())
            .unwrap()
            .expect("chain has a head");
        assert_eq!(tail.lamport, 9);
        assert_eq!(tail.entry_hash, e3.entry.entry_hash);
        assert_eq!(entry_count(&conn), 3);

        let projected = load_projection(&conn, stream_a()).unwrap();
        assert_eq!(projected.nodes.len(), 3);
        assert!(projected.nodes.contains_key(&NodeId::from("mem_a")));
    }

    #[test]
    fn author_op_mints_a_genesis_chain_and_projects() {
        let conn = db();
        let device = crate::oplog::local_device(&conn, 0).unwrap();
        // The FIRST authored op is genesis (lamport 0), each next advances by one — the allocator
        // reads the tail, unlike `append` which is fed a caller-chosen lamport.
        let h0 = author_op(&conn, stream_a(), &device, &create("mem_a", "first"), 1_000).unwrap();
        let h1 = author_op(&conn, stream_a(), &device, &create("mem_b", "second"), 1_001).unwrap();
        let h2 = author_op(&conn, stream_a(), &device, &create("mem_c", "third"), 1_002).unwrap();
        assert_ne!(h0, h1, "each authored entry is distinct");
        let tail = chain_tail(&conn, stream_a(), device.fingerprint()).unwrap().unwrap();
        assert_eq!(tail.lamport, 2, "three authored ops occupy lamports 0,1,2");
        assert_eq!(tail.entry_hash, h2);
        assert_eq!(entry_count(&conn), 3);
        let projected = load_projection(&conn, stream_a()).unwrap();
        assert_eq!(projected.nodes.len(), 3);
        assert!(projected.nodes.contains_key(&NodeId::from("mem_a")));
    }

    #[test]
    fn author_in_tx_commits_atomically_with_a_caller_write() {
        let conn = db();
        let device = crate::oplog::local_device(&conn, 0).unwrap();
        // A live mutation: a table write and the op-append share ONE caller-owned txn.
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        tx.execute("CREATE TABLE probe(x INTEGER)", []).unwrap();
        tx.execute("INSERT INTO probe(x) VALUES (1)", []).unwrap();
        author_in_tx(&tx, stream_a(), &device, &create("mem_a", "first"), 1_000).unwrap();
        tx.commit().unwrap();
        // Both sides landed under the one commit.
        assert_eq!(entry_count(&conn), 1);
        assert_eq!(load_projection(&conn, stream_a()).unwrap().nodes.len(), 1);
    }

    #[test]
    fn author_in_tx_rolls_back_with_the_caller_transaction() {
        let conn = db();
        let device = crate::oplog::local_device(&conn, 0).unwrap();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_in_tx(&tx, stream_a(), &device, &create("mem_a", "first"), 1_000).unwrap();
        drop(tx); // roll back the caller's txn
        assert_eq!(entry_count(&conn), 0, "the authored entry rolls back with the caller's txn");
    }

    #[test]
    fn author_batch_writes_one_atomic_chain_and_an_empty_batch_is_a_noop() {
        let conn = db();
        let device = crate::oplog::local_device(&conn, 0).unwrap();
        let authored = author_batch(&conn, stream_a(), &device, &[], 1_000).unwrap();
        assert!(!authored, "an empty batch authors nothing");
        assert_eq!(entry_count(&conn), 0);

        let ops = [create("mem_a", "a"), create("mem_b", "b"), create("mem_c", "c")];
        assert!(
            author_batch(&conn, stream_a(), &device, &ops, 2_000).unwrap(),
            "the genesis batch authored"
        );
        let tail = chain_tail(&conn, stream_a(), device.fingerprint()).unwrap().unwrap();
        assert_eq!(tail.lamport, 2, "the batch chained lamports 0,1,2 in one txn");
        assert_eq!(entry_count(&conn), 3);
        assert_eq!(load_projection(&conn, stream_a()).unwrap().nodes.len(), 3);
    }

    #[test]
    fn author_batch_no_ops_on_a_nonempty_chain() {
        let conn = db();
        let device = crate::oplog::local_device(&conn, 0).unwrap();
        author_op(&conn, stream_a(), &device, &create("mem_a", "genesis"), 1_000).unwrap();
        // author_batch is a GENESIS gate: on an already-non-empty chain it no-ops (returns false)
        // rather than appending — that atomic gate is the backfill's idempotency guarantee.
        let authored =
            author_batch(&conn, stream_a(), &device, &[create("mem_b", "b")], 2_000).unwrap();
        assert!(!authored, "a genesis batch no-ops on a non-empty chain");
        assert_eq!(entry_count(&conn), 1, "the non-empty chain is left untouched");
    }

    #[test]
    fn reappend_is_idempotent() {
        let conn = db();
        let s = secret(2);
        let g = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "first"));
        append(&conn, stream_a(), &g.signed_bytes, &s.public(), 1_000).unwrap();
        let again = append(&conn, stream_a(), &g.signed_bytes, &s.public(), 1_005).unwrap();
        assert_eq!(again, AppendOutcome::AlreadyPresent { entry_hash: g.entry.entry_hash });
        assert_eq!(entry_count(&conn), 1);
    }

    #[test]
    fn missing_predecessor_rejections() {
        let conn = db();
        let s = secret(3);
        // A follow-on that references a predecessor we don't hold (no genesis appended yet).
        let orphan =
            entry::sign_entry(&s, stream_a(), Some([7u8; 32]), 2, &create("mem_a", "orphan"));
        assert!(matches!(
            append(&conn, stream_a(), &orphan.signed_bytes, &s.public(), 1_000).unwrap(),
            AppendOutcome::MissingPredecessor { .. }
        ));
        assert_eq!(entry_count(&conn), 0);

        // Genesis, then a follow-on that advances lamport but points prev at the wrong hash (a
        // gap).
        let g = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "first"));
        append(&conn, stream_a(), &g.signed_bytes, &s.public(), 1_001).unwrap();
        let gap = entry::sign_entry(&s, stream_a(), Some([9u8; 32]), 5, &create("mem_b", "gap"));
        assert!(matches!(
            append(&conn, stream_a(), &gap.signed_bytes, &s.public(), 1_002).unwrap(),
            AppendOutcome::MissingPredecessor { .. }
        ));
        assert_eq!(entry_count(&conn), 1);
    }

    #[test]
    fn fork_rejections_keep_evidence() {
        let conn = db();
        let s = secret(4);
        let g = entry::sign_entry(&s, stream_a(), None, 5, &create("mem_a", "first"));
        append(&conn, stream_a(), &g.signed_bytes, &s.public(), 1_000).unwrap();

        // A second genesis for the same device is an equivocation.
        let second_genesis =
            entry::sign_entry(&s, stream_a(), None, 6, &create("mem_b", "genesis-2"));
        let out =
            append(&conn, stream_a(), &second_genesis.signed_bytes, &s.public(), 1_001).unwrap();
        match out {
            AppendOutcome::Fork { conflicting, .. } => {
                assert_eq!(conflicting, g.signed_bytes, "the stored genesis is the evidence");
            },
            other => panic!("expected Fork, got {other:?}"),
        }

        // A follow-on that chains onto the head but does not advance lamport rewrites history.
        let stale = entry::sign_entry(
            &s,
            stream_a(),
            Some(g.entry.entry_hash),
            5,
            &create("mem_c", "stale"),
        );
        assert!(matches!(
            append(&conn, stream_a(), &stale.signed_bytes, &s.public(), 1_002).unwrap(),
            AppendOutcome::Fork { .. }
        ));

        // A stale entry (lamport at/below the tail) whose predecessor we don't even hold can never
        // become a valid extension either — it is a permanent equivocation, NOT a retryable gap.
        let stale_absent =
            entry::sign_entry(&s, stream_a(), Some([3u8; 32]), 4, &create("mem_d", "stale-absent"));
        assert!(matches!(
            append(&conn, stream_a(), &stale_absent.signed_bytes, &s.public(), 1_003).unwrap(),
            AppendOutcome::Fork { .. }
        ));

        assert_eq!(entry_count(&conn), 1, "no fork entered the log");

        // Every rejected head was durably quarantined — BOTH heads of each equivocation survive a
        // process exit — and each row points at the stored entry it collided with.
        let quarantined = fork_evidence(&conn);
        assert_eq!(quarantined.len(), 3, "each fork left one evidence row");
        assert!(
            quarantined.iter().any(|(hash, signed, conflicting)| {
                hash.as_slice() == second_genesis.entry.entry_hash
                    && signed == &second_genesis.signed_bytes
                    && conflicting.as_deref() == Some(g.entry.entry_hash.as_slice())
            }),
            "the second genesis is quarantined verbatim, pointing at the stored genesis"
        );

        // Redelivering a quarantined fork re-reports without duplicating the evidence row.
        assert!(matches!(
            append(&conn, stream_a(), &second_genesis.signed_bytes, &s.public(), 1_004).unwrap(),
            AppendOutcome::Fork { .. }
        ));
        assert_eq!(fork_evidence(&conn).len(), 3, "redelivery does not duplicate evidence");
    }

    #[test]
    fn branch_off_a_present_non_head_entry_is_a_fork_not_a_gap() {
        let conn = db();
        let s = secret(13);
        // Build a chain A -> B.
        let a = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "a"));
        append(&conn, stream_a(), &a.signed_bytes, &s.public(), 1).unwrap();
        let b =
            entry::sign_entry(&s, stream_a(), Some(a.entry.entry_hash), 2, &create("mem_b", "b"));
        append(&conn, stream_a(), &b.signed_bytes, &s.public(), 2).unwrap();

        // C forks off A — a PRESENT predecessor that is not the head B — at a higher lamport. The
        // predecessor exists, so backfill can never resolve it: this is an equivocation, not a gap.
        let c =
            entry::sign_entry(&s, stream_a(), Some(a.entry.entry_hash), 3, &create("mem_c", "c"));
        match append(&conn, stream_a(), &c.signed_bytes, &s.public(), 3).unwrap() {
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
        let g = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "first"));

        let mut tampered = g.signed_bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(append(&conn, stream_a(), &tampered, &s.public(), 1_000).is_err());

        // A valid entry verified under the wrong device key.
        let wrong = secret(6);
        assert!(append(&conn, stream_a(), &g.signed_bytes, &wrong.public(), 1_000).is_err());
        assert_eq!(entry_count(&conn), 0);
    }

    #[test]
    fn poison_op_bytes_are_rejected_at_accept() {
        let conn = db();
        let s = secret(7);
        // A validly-SIGNED entry whose op bytes are structurally undecodable.
        let poison = entry::sign_entry_from_op_bytes(&s, stream_a(), None, 1, vec![0xFF]);
        assert!(
            append(&conn, stream_a(), &poison.signed_bytes, &s.public(), 1_000).is_err(),
            "an undecodable op must not enter the log (it would wedge every future reproject)"
        );
        assert_eq!(entry_count(&conn), 0);
    }

    #[test]
    fn unknown_op_stores_and_chains_but_is_not_projected() {
        let conn = db();
        let s = secret(8);
        // A canonical `rag-rat/op/1` envelope with a kind tag this binary doesn't know → decodes to
        // `Unknown`, so it passes the accept-gate, stores, and chains — but never projects.
        let mut op_bytes = Vec::new();
        {
            let mut enc = Encoder::new(&mut op_bytes);
            enc.array(3).unwrap();
            enc.str(OP_DOMAIN).unwrap();
            enc.str("future_kind").unwrap();
            enc.null().unwrap();
        }
        let unknown = entry::sign_entry_from_op_bytes(&s, stream_a(), None, 1, op_bytes);
        assert!(matches!(
            append(&conn, stream_a(), &unknown.signed_bytes, &s.public(), 1_000).unwrap(),
            AppendOutcome::Appended { .. }
        ));
        // It chains: a follow-on onto it is accepted.
        let follow = entry::sign_entry(
            &s,
            stream_a(),
            Some(unknown.entry.entry_hash),
            2,
            &create("mem_a", "x"),
        );
        append(&conn, stream_a(), &follow.signed_bytes, &s.public(), 1_001).unwrap();

        assert_eq!(entry_count(&conn), 2, "both entries are in the log");
        let projected = load_projection(&conn, stream_a()).unwrap();
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
        let g = entry::sign_entry(&s, stream_a(), None, 1, &{
            let mut c = content("payload node");
            c.payload = Some(r#"{"schema_version":1,"n":42}"#.to_string());
            MemoryOp::NodeCreate { node_id: NodeId::from("mem_a"), content: c }
        });
        let g_other = entry::sign_entry(&t, stream_a(), None, 2, &create("mem_b", "other-device"));
        let status =
            entry::sign_entry(&s, stream_a(), Some(g.entry.entry_hash), 3, &MemoryOp::NodeStatus {
                node_id: NodeId::from("mem_a"),
                status: NodeStatus::Stale,
            });

        let expected;
        {
            let conn = Connection::open(&path).unwrap();
            schema::apply(&conn).unwrap();
            append(&conn, stream_a(), &g.signed_bytes, &s.public(), 1).unwrap();
            append(&conn, stream_a(), &g_other.signed_bytes, &t.public(), 2).unwrap();
            append(&conn, stream_a(), &status.signed_bytes, &s.public(), 3).unwrap();
            expected = load_projection(&conn, stream_a()).unwrap();
        }

        // Reopen a fresh connection to the same file: the projection persisted verbatim.
        let reopened = Connection::open(&path).unwrap();
        let loaded = load_projection(&reopened, stream_a()).unwrap();
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
        let g = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "first"));
        append(&conn, stream_a(), &g.signed_bytes, &s.public(), 1_000).unwrap();
        assert_eq!(load_projection(&conn, stream_a()).unwrap().nodes.len(), 1);

        // Simulate an old binary that folded under an earlier projector version and left the shadow
        // tables stale (here: emptied).
        conn.execute_batch("DELETE FROM oplog_projected_nodes;").unwrap();
        conn.execute("UPDATE oplog_meta SET value = '0' WHERE key = ?1", params![
            PROJECTOR_VERSION_KEY
        ])
        .unwrap();

        assert!(reproject_if_projector_stale(&conn).unwrap(), "a stale stamp re-folds");
        assert_eq!(load_projection(&conn, stream_a()).unwrap().nodes.len(), 1, "the node is back");
        assert!(!reproject_if_projector_stale(&conn).unwrap(), "current stamp is a no-op");
    }

    #[test]
    fn append_refuses_to_downgrade_a_newer_projection() {
        let conn = db();
        let s = secret(14);
        let g = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "first"));
        append(&conn, stream_a(), &g.signed_bytes, &s.public(), 1).unwrap();

        // Simulate a NEWER binary having folded + stamped a higher projector version.
        conn.execute("UPDATE oplog_meta SET value = ?1 WHERE key = ?2", params![
            (PROJECTOR_VERSION + 1).to_string(),
            PROJECTOR_VERSION_KEY
        ])
        .unwrap();

        // This older projector must refuse to append (it would drop newer-known ops and stamp
        // down).
        let e2 = entry::sign_entry(
            &s,
            stream_a(),
            Some(g.entry.entry_hash),
            2,
            &create("mem_b", "second"),
        );
        assert!(
            append(&conn, stream_a(), &e2.signed_bytes, &s.public(), 2).is_err(),
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
        let g = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "first"));
        append(&conn, stream_a(), &g.signed_bytes, &s.public(), 1).unwrap();
        let add =
            entry::sign_entry(&s, stream_a(), Some(g.entry.entry_hash), 2, &MemoryOp::EdgeAdd {
                edge: edge.clone(),
            });
        let add_hash = add.entry.entry_hash;
        append(&conn, stream_a(), &add.signed_bytes, &s.public(), 2).unwrap();
        assert_eq!(load_projection(&conn, stream_a()).unwrap().edges.len(), 1);

        let remove = entry::sign_entry(&s, stream_a(), Some(add_hash), 3, &MemoryOp::EdgeRemove {
            edge_key: key.clone(),
        });
        append(&conn, stream_a(), &remove.signed_bytes, &s.public(), 3).unwrap();
        assert!(
            load_projection(&conn, stream_a()).unwrap().edges.is_empty(),
            "the reproject drops the tombstoned edge's row"
        );
    }

    #[test]
    fn an_entry_offered_for_the_wrong_stream_is_refused() {
        // The cross-stream replay case the in-body stream_id exists to stop: a validly-signed
        // stream-B entry offered on stream A is a hard error — never stored, never quarantined
        // (it is not an equivocation, it is mis-delivery).
        let conn = db();
        let s = secret(15);
        let foreign = entry::sign_entry(&s, stream_b(), None, 1, &create("mem_a", "foreign"));
        assert!(
            append(&conn, stream_a(), &foreign.signed_bytes, &s.public(), 1).is_err(),
            "a stream-B entry must not append onto stream A"
        );
        assert_eq!(entry_count(&conn), 0);
        assert!(fork_evidence(&conn).is_empty(), "mis-delivery is not an equivocation");
    }

    #[test]
    fn streams_are_isolated_chains_and_projections() {
        let conn = db();
        let s = secret(16);

        // The SAME device opens a genesis at the SAME lamport on two streams: two independent
        // chains, not a fork.
        let on_a = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "on a"));
        let on_b = entry::sign_entry(&s, stream_b(), None, 1, &create("mem_b", "on b"));
        assert!(matches!(
            append(&conn, stream_a(), &on_a.signed_bytes, &s.public(), 1).unwrap(),
            AppendOutcome::Appended { .. }
        ));
        assert!(matches!(
            append(&conn, stream_b(), &on_b.signed_bytes, &s.public(), 2).unwrap(),
            AppendOutcome::Appended { .. }
        ));

        // Tails are per (stream, device).
        let device = s.public().fingerprint();
        assert_eq!(chain_tail(&conn, stream_a(), device).unwrap().unwrap().lamport, 1);
        assert_eq!(
            chain_tail(&conn, stream_a(), device).unwrap().unwrap().entry_hash,
            on_a.entry.entry_hash
        );
        assert_eq!(
            chain_tail(&conn, stream_b(), device).unwrap().unwrap().entry_hash,
            on_b.entry.entry_hash
        );

        // Projections are per stream: each holds exactly its own node.
        let projected_a = load_projection(&conn, stream_a()).unwrap();
        let projected_b = load_projection(&conn, stream_b()).unwrap();
        assert_eq!(projected_a.nodes.len(), 1);
        assert!(projected_a.nodes.contains_key(&NodeId::from("mem_a")));
        assert_eq!(projected_b.nodes.len(), 1);
        assert!(projected_b.nodes.contains_key(&NodeId::from("mem_b")));

        // A fork on stream B never perturbs stream A's chain or projection.
        let fork_b = entry::sign_entry(&s, stream_b(), None, 2, &create("mem_c", "fork"));
        assert!(matches!(
            append(&conn, stream_b(), &fork_b.signed_bytes, &s.public(), 3).unwrap(),
            AppendOutcome::Fork { .. }
        ));
        assert_eq!(load_projection(&conn, stream_a()).unwrap(), projected_a);
        assert_eq!(load_projection(&conn, stream_b()).unwrap(), projected_b);

        // A wholesale rebuild re-folds every stream and converges to the same state.
        rebuild_projection(&conn).unwrap();
        assert_eq!(load_projection(&conn, stream_a()).unwrap(), projected_a);
        assert_eq!(load_projection(&conn, stream_b()).unwrap(), projected_b);
    }

    #[test]
    fn upgrade_refold_rebuilds_every_stream() {
        // The upgrade re-fold must sweep EVERY stream, not just the busiest: a stream nothing is
        // writing to would otherwise serve a stale materialization forever after an upgrade.
        let conn = db();
        let s = secret(17);
        let on_a = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "on a"));
        let on_b = entry::sign_entry(&s, stream_b(), None, 1, &create("mem_b", "on b"));
        append(&conn, stream_a(), &on_a.signed_bytes, &s.public(), 1).unwrap();
        append(&conn, stream_b(), &on_b.signed_bytes, &s.public(), 2).unwrap();

        conn.execute_batch(
            "DELETE FROM oplog_projected_nodes;
             DELETE FROM oplog_projected_edges;",
        )
        .unwrap();
        conn.execute("UPDATE oplog_meta SET value = '0' WHERE key = ?1", params![
            PROJECTOR_VERSION_KEY
        ])
        .unwrap();

        assert!(reproject_if_projector_stale(&conn).unwrap(), "a stale stamp re-folds");
        assert_eq!(load_projection(&conn, stream_a()).unwrap().nodes.len(), 1);
        assert_eq!(load_projection(&conn, stream_b()).unwrap().nodes.len(), 1);
    }

    #[test]
    fn append_over_a_stale_stamp_refolds_every_stream_before_stamping() {
        // An append that finds an older/missing projector stamp writes the store-GLOBAL current
        // stamp on commit — so it must re-fold every stream first, not just its own. If it swept
        // only the appended stream, the now-current stamp would stop
        // `reproject_if_projector_stale` from ever fixing the others' stale rows.
        let conn = db();
        let s = secret(18);
        let on_a = entry::sign_entry(&s, stream_a(), None, 1, &create("mem_a", "on a"));
        let on_b = entry::sign_entry(&s, stream_b(), None, 1, &create("mem_b", "on b"));
        append(&conn, stream_a(), &on_a.signed_bytes, &s.public(), 1).unwrap();
        append(&conn, stream_b(), &on_b.signed_bytes, &s.public(), 2).unwrap();

        // Simulate an old binary's fold: stale shadow rows everywhere, an older stamp.
        conn.execute_batch(
            "DELETE FROM oplog_projected_nodes;
             DELETE FROM oplog_projected_edges;",
        )
        .unwrap();
        conn.execute("UPDATE oplog_meta SET value = '0' WHERE key = ?1", params![
            PROJECTOR_VERSION_KEY
        ])
        .unwrap();

        // Append lands on stream A only.
        let second = entry::sign_entry(
            &s,
            stream_a(),
            Some(on_a.entry.entry_hash),
            2,
            &create("mem_c", "second"),
        );
        append(&conn, stream_a(), &second.signed_bytes, &s.public(), 3).unwrap();

        // Stream B's projection was rebuilt too, and the stamp is genuinely current.
        assert_eq!(load_projection(&conn, stream_b()).unwrap().nodes.len(), 1);
        assert_eq!(load_projection(&conn, stream_a()).unwrap().nodes.len(), 2);
        assert!(
            !reproject_if_projector_stale(&conn).unwrap(),
            "nothing left for the upgrade re-fold to do"
        );
    }
}
