//! The engine's signed entry log: `table_sync_entries`, one hash-chained chain per
//! `(stream_id, device_fingerprint)`.
//!
//! Deliberately separate from `oplog_entries` — that table's upgrade re-fold decodes every stored
//! stream as a memory-content op and would choke on a table op. The signing/verification/chain
//! primitives ([`super::super::entry`]) are op-agnostic (they treat `op_bytes` as opaque), so they
//! are reused verbatim; only the storage table and the row-op poison guard are new.
//!
//! [`author_row_entry`] mints a local entry (lamport = one past the highest on the stream);
//! [`accept_row_entry`] verifies and chain-classifies a foreign entry, stores it if the chain is
//! continuous, then decides whether its payload applies — so one bad payload never wedges the
//! chain. Full fork evidence and out-of-order backfill are the transport milestone's job — here a
//! gap or conflict is simply reported, not durably quarantined.

use anyhow::Context;
use rusqlite::{OptionalExtension, Transaction, params};

use super::row_op::{self, DecodedRowOp, RowOp};
use crate::AccountId;
use crate::account::device_is_effective_writer;
use crate::device::{DevicePublic, DeviceSecret};
use crate::entry::{self, SignedEntry, VerifiedEntry};
use crate::op::{DeviceFingerprint, OpMeta};
use crate::stream::StreamId;

/// Upper bound on an entry's lamport, far below `i64::MAX`. A Lamport clock increments by one per
/// op, so a legitimate value never approaches this; a larger one is malformed or a wedging attack.
const MAX_ENTRY_LAMPORT: u64 = 1 << 62;

/// The most a single accepted entry may advance the stream's Lamport clock. A Lamport clock ticks
/// by one per op, so a legitimate entry is at most a partition's worth of ops ahead of the highest
/// lamport already stored — never billions. This bound (far above any real causal gap, far below
/// the ceiling) refuses a griefing entry that jumps toward `MAX_ENTRY_LAMPORT`: such an entry would
/// dominate every row's whole-row LWW AND, once `next_stream_lamport` reaches the ceiling, halt all
/// local authoring on the scope. With the bound, reaching the ceiling needs ~2^30 chained entries,
/// not one. (A peer griefing WITHIN the bound is the auth/roster milestone's job — device removal.)
const MAX_LAMPORT_ADVANCE: u64 = 1 << 32;

/// Why a retained entry is NOT projected into its table — persisted per entry so a later binary
/// that understands the payload replays exactly the outstanding set (#1001). Without a durable
/// mark, redelivery short-circuits on `entry_exists` and the payload is unrecoverable.
///
/// These tokens are SCHEMA: they are written to `table_sync_entries.pending_reason` and read back
/// by a future binary, so every variant round-trips through [`PendingReason::as_db_str`] /
/// [`PendingReason::from_db_str`] and a rename needs a migration, exactly like a column rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingReason {
    /// The op carries a column this registry does not know — a newer producer. Nothing is written:
    /// applying the known subset would leave a row NO device ever authored, and publishing that row
    /// lets the producer re-author the hole at a winning lamport (see [`super::apply`]).
    UnknownColumn,
    /// The op omits a column this registry requires — an OLDER producer, whose complete row under
    /// its narrower spec is a partial after-image under ours. Whole-row LWW needs the full
    /// after-image, so nothing is written. Unlike a broken producer this is a version gap, and it
    /// is redeemed from the SENDER's side: #1002's declared column defaults rebuild the missing
    /// cells into a complete row, at which point the replay lands it.
    PartialAfterImage,
    /// The op-kind is outside this binary's row-op vocabulary.
    UnknownOpKind,
    /// The op bytes do not decode at all.
    UndecodablePayload,
    /// The op's table is not in this binary's registry for the entry's scope.
    TableNotInScope,
}

impl PendingReason {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::UnknownColumn => "unknown_column",
            Self::PartialAfterImage => "partial_after_image",
            Self::UnknownOpKind => "unknown_op_kind",
            Self::UndecodablePayload => "undecodable_payload",
            Self::TableNotInScope => "table_not_in_scope",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "unknown_column" => Some(Self::UnknownColumn),
            "partial_after_image" => Some(Self::PartialAfterImage),
            "unknown_op_kind" => Some(Self::UnknownOpKind),
            "undecodable_payload" => Some(Self::UndecodablePayload),
            "table_not_in_scope" => Some(Self::TableNotInScope),
            _ => None,
        }
    }
}

/// The result of accepting a foreign entry. A chain-continuous entry is ALWAYS stored (so one bad
/// payload cannot wedge the device's chain); whether its payload is APPLIED is a separate decision.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AcceptOutcome {
    /// Stored, and its known in-scope row op is ready to apply. `entry_hash` lets the caller mark
    /// the entry pending if the APPLY turns out to be incomplete (an unknown column), which only
    /// the registry-aware applier can detect.
    Stored {
        op: RowOp,
        meta: OpMeta,
        entry_hash: [u8; 32],
    },
    /// Stored and retained, but NOT applied — an undecodable payload, a future op-kind, or a table
    /// not in this scope. The chain still advanced, and the entry is marked pending so a later
    /// binary replays it.
    StoredInert(PendingReason),
    AlreadyPresent,
    /// The lamport advances past the tail but the entry does not link to it (a gap): the
    /// predecessor has not arrived. Routine under out-of-order delivery; the transport retries
    /// after backfill.
    MissingPredecessor,
    /// The entry conflicts with the stored chain (a second genesis, or a lamport at/behind the
    /// tail) — an equivocation the transport milestone will quarantine with evidence.
    Fork,
    /// The signing device is not a roster-effective writer of the account (off-roster, removed, or
    /// read-only), so the entry is DROPPED — not stored, not relayed, chain not advanced (#935).
    /// RETRYABLE, not peer misbehavior: the local fold may not have applied the author's
    /// `DeviceAdd` yet, so a caller must never penalize the peer, and the sync frontier
    /// re-offers the entry once the account log delivers the enrollment.
    Unauthorized,
}

/// Mint one local row op as a signed entry on `stream` and store it.
///
/// The lamport is a Lamport clock over the WHOLE stream — one past the highest lamport seen from
/// any device — NOT this device's own chain position. That is what makes a local edit supersede a
/// row this device just received: a per-device counter would restart at 0 and could tie (and lose)
/// to an ingested op at the same position. The `prev_hash` still links this device's own chain, so
/// a device's lamports are strictly increasing but need not be contiguous. Read inside the caller's
/// transaction so the reads and the insert are one write.
pub(crate) fn author_row_entry(
    tx: &Transaction<'_>,
    stream: StreamId,
    secret: &DeviceSecret,
    op: &RowOp,
    now_ms: i64,
) -> anyhow::Result<SignedEntry> {
    let device = secret.public().fingerprint();
    let lamport = next_stream_lamport(tx, stream)?;
    let prev_hash = chain_tail(tx, stream, device)?.map(|(_, entry_hash)| entry_hash);
    let signed =
        entry::sign_entry_from_op_bytes(secret, stream, prev_hash, lamport, row_op::encode(op));
    // A locally-authored op is projected by construction: the producer builds it from THIS
    // registry, so it can never carry a column or op-kind this binary does not understand.
    insert_entry(tx, &signed.entry, &signed.signed_bytes, now_ms, None)?;
    Ok(signed)
}

/// One past the highest lamport on `stream` across all devices — the next Lamport-clock tick. `0`
/// for an empty stream.
fn next_stream_lamport(tx: &Transaction<'_>, stream: StreamId) -> anyhow::Result<u64> {
    let stream_bytes = stream.to_bytes();
    let highest: Option<i64> = tx.query_row(
        "SELECT MAX(lamport) FROM table_sync_entries WHERE stream_id = ?1",
        params![stream_bytes.as_slice()],
        |row| row.get::<_, Option<i64>>(0),
    )?;
    let next = match highest {
        Some(lamport) =>
            u64::try_from(lamport)?.checked_add(1).context("stream lamport overflow")?,
        None => 0,
    };
    // Cap at the same ceiling `accept_row_entry` enforces, so a locally-authored entry can never
    // exceed what peers accept. Only reachable if a near-ceiling entry was ingested (impossible at
    // legitimate op volume); refusing to author is a bounded halt, never a divergent split.
    anyhow::ensure!(next < MAX_ENTRY_LAMPORT, "stream lamport ceiling reached");
    Ok(next)
}

/// Verify + chain-classify + store one foreign signed entry, expected on `expected_stream` under
/// `pubkey`. A tampered/wrong-keyed entry or one naming a different stream is an `Err`; a chain gap
/// or conflict is a (non-storing) [`AcceptOutcome`]. A chain-continuous entry is stored REGARDLESS
/// of whether its payload is applicable, so one undecodable / unknown / out-of-scope payload cannot
/// wedge every later entry from that device — storage is gated on the CHAIN, application on the
/// PAYLOAD.
pub(crate) fn accept_row_entry(
    tx: &Transaction<'_>,
    account_id: AccountId,
    expected_stream: StreamId,
    expected_tables: &[&str],
    signed_bytes: &[u8],
    pubkey: &DevicePublic,
    now_ms: i64,
) -> anyhow::Result<AcceptOutcome> {
    let verified = entry::verify_signed(signed_bytes, pubkey)?;
    if verified.stream_id != expected_stream {
        anyhow::bail!("entry names a different stream than the one being synced");
    }
    // Reserve the lamport ceiling (reject `>=`, not `>`). Without this, an entry AT the boundary
    // would be accepted, then the next local `MAX(lamport)+1` would exceed it and be rejected by
    // every peer — splitting this device's later edits from the scope. Authoring is capped at the
    // same ceiling (`next_stream_lamport`), so no locally-authored entry can ever cross it. The
    // ceiling sits far below i64::MAX, so it is unreachable by legitimate op volume.
    if verified.lamport >= MAX_ENTRY_LAMPORT {
        anyhow::bail!("entry lamport {} exceeds the protocol ceiling", verified.lamport);
    }
    // Bounded advance: refuse an entry that jumps the stream's Lamport clock implausibly far ahead
    // of what is already stored (see `MAX_LAMPORT_ADVANCE`). Without it, a single griefing
    // entry near the ceiling would dominate every row's LWW and halt local authoring once
    // `next_stream_lamport` hits the ceiling. Read the current max BEFORE storing this entry.
    let stream_max: u64 = {
        let stored: i64 = tx.query_row(
            "SELECT COALESCE(MAX(lamport), 0) FROM table_sync_entries WHERE stream_id = ?1",
            params![expected_stream.to_bytes().as_slice()],
            |row| row.get(0),
        )?;
        u64::try_from(stored).unwrap_or(0)
    };
    if verified.lamport > stream_max.saturating_add(MAX_LAMPORT_ADVANCE) {
        anyhow::bail!(
            "entry lamport {} jumps more than {MAX_LAMPORT_ADVANCE} past the stream clock \
             {stream_max} — refusing (a near-ceiling jump would dominate LWW and halt authoring)",
            verified.lamport
        );
    }
    if entry_exists(tx, &verified.entry_hash)? {
        return Ok(AcceptOutcome::AlreadyPresent);
    }
    // Authority gate (#935): the signing device must be a roster-effective WRITER of the account.
    // Placed AFTER `entry_exists` so an entry stored while the device WAS a writer still reports
    // `AlreadyPresent` after its removal (dedup precedence over authority), and BEFORE
    // `insert_entry` and the `StoredInert` path so an off-roster / removed / read-only
    // principal can never store or retain a row. Dropping (not storing) is correct here: chains
    // are per `(stream, device)`, so an unauthorized device has no legitimate chain that
    // dropping could wedge.
    //
    // Known liveness gap: this is current-roster authority, not as-of-authoring authority. A
    // fresh replica that ingests a since-removed writer's entries drops even the rows that device
    // legitimately authored before removal, so those rows never reach it — the entry lacks the
    // roster epoch needed to distinguish pre- from post-removal authorship. The epoch-aware fix
    // (roster reference in the wire format) is #892; an interim re-adoption pass, where an active
    // writer re-authors orphaned rows under its own chain, is #997.
    if !device_is_effective_writer(tx, account_id, verified.device_fingerprint)? {
        return Ok(AcceptOutcome::Unauthorized);
    }
    match classify(tx, expected_stream, &verified)? {
        ChainFit::Ok => {},
        ChainFit::Gap => return Ok(AcceptOutcome::MissingPredecessor),
        ChainFit::Conflict => return Ok(AcceptOutcome::Fork),
    }
    // Classify the payload BEFORE storing, so the entry lands with its projection state recorded in
    // the same INSERT — no store-then-mark window, and no second write.
    let outcome = match row_op::decode(&verified.op_bytes) {
        Err(_) => AcceptOutcome::StoredInert(PendingReason::UndecodablePayload),
        Ok(DecodedRowOp::Unknown { .. }) =>
            AcceptOutcome::StoredInert(PendingReason::UnknownOpKind),
        Ok(DecodedRowOp::Known(op)) =>
            if expected_tables.contains(&op.table()) {
                AcceptOutcome::Stored {
                    op,
                    meta: OpMeta { lamport: verified.lamport, device: verified.device_fingerprint },
                    entry_hash: verified.entry_hash,
                }
            } else {
                AcceptOutcome::StoredInert(PendingReason::TableNotInScope)
            },
    };
    // Chain-continuous: store now so a bad payload can never wedge the device's chain.
    let pending = match &outcome {
        AcceptOutcome::StoredInert(reason) => Some(*reason),
        // A `Stored` op may still fail to project on an unknown COLUMN, which only the
        // registry-aware applier sees; the caller marks it pending via `entry_hash` in that case.
        _ => None,
    };
    insert_entry(tx, &verified, signed_bytes, now_ms, pending)?;
    Ok(outcome)
}

/// How a verified entry fits its `(stream, device)` chain tail.
enum ChainFit {
    Ok,
    Gap,
    Conflict,
}

fn classify(
    tx: &Transaction<'_>,
    stream: StreamId,
    verified: &VerifiedEntry,
) -> anyhow::Result<ChainFit> {
    Ok(match (verified.prev_hash, chain_tail(tx, stream, verified.device_fingerprint)?) {
        // A genesis (no predecessor) is the valid first entry of this device's chain; a genesis
        // when a chain already exists is a second head — an equivocation.
        (None, None) => ChainFit::Ok,
        (None, Some(_)) => ChainFit::Conflict,
        // A non-genesis whose device has no chain yet: its predecessor has not been delivered.
        (Some(_), None) => ChainFit::Gap,
        (Some(prev), Some((tail_lamport, tail_hash))) =>
            if prev == tail_hash && verified.lamport > tail_lamport {
                ChainFit::Ok
            } else if verified.lamport <= tail_lamport {
                ChainFit::Conflict // at/behind the tail — an equivocation.
            } else if entry_exists(tx, &prev)? {
                // Links PAST the tail to an ALREADY-STORED ancestor: that ancestor already has a
                // successor (the one leading to the tail), so this is a SECOND successor — an
                // equivocation, not a missing intermediate. Reporting Gap would make the transport
                // backfill, get `AlreadyPresent`, and loop without ever recognizing the fork.
                ChainFit::Conflict
            } else {
                ChainFit::Gap // links to an UNKNOWN predecessor — a genuine missing intermediate.
            },
    })
}

/// The `(stream, device)` chain's highest-lamport `(lamport, entry_hash)`, or `None` for an empty
/// chain — the `max(seen)+1` restore point, read fresh inside the caller's transaction.
fn chain_tail(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: DeviceFingerprint,
) -> anyhow::Result<Option<(u64, [u8; 32])>> {
    let stream_bytes = stream.to_bytes();
    let device_bytes = device.to_bytes();
    let row = tx
        .query_row(
            "SELECT lamport, entry_hash FROM table_sync_entries
             WHERE stream_id = ?1 AND device_fingerprint = ?2 ORDER BY lamport DESC LIMIT 1",
            params![stream_bytes.as_slice(), device_bytes.as_slice()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    row.map(|(lamport, hash)| Ok((u64::try_from(lamport)?, fixed32(hash)?))).transpose()
}

fn entry_exists(tx: &Transaction<'_>, entry_hash: &[u8; 32]) -> anyhow::Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM table_sync_entries WHERE entry_hash = ?1",
            params![entry_hash.as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Store a chain-continuous entry, carrying its projection state (`pending`: `None` = fully
/// projected) in the same INSERT.
fn insert_entry(
    tx: &Transaction<'_>,
    verified: &VerifiedEntry,
    signed_bytes: &[u8],
    now_ms: i64,
    pending: Option<PendingReason>,
) -> anyhow::Result<()> {
    let stream_bytes = verified.stream_id.to_bytes();
    let device_bytes = verified.device_fingerprint.to_bytes();
    let prev_hash: Option<Vec<u8>> = verified.prev_hash.map(|h| h.to_vec());
    tx.execute(
        "INSERT INTO table_sync_entries(
             entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
             received_at_ms, pending_reason, pending_projector_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            verified.entry_hash.as_slice(),
            stream_bytes.as_slice(),
            device_bytes.as_slice(),
            i64::try_from(verified.lamport)?,
            prev_hash,
            signed_bytes,
            now_ms,
            pending.map(PendingReason::as_db_str),
            pending.map(|_| super::refold::TABLE_SYNC_PROJECTOR_VERSION),
        ],
    )?;
    Ok(())
}

/// Mark a stored entry as not-yet-projected under `projector_version` — the caller-side counterpart
/// of [`insert_entry`]'s `pending`, for the unknown-COLUMN case that only the registry-aware
/// applier can detect (the entry is already stored by then).
pub(crate) fn mark_entry_pending(
    tx: &Transaction<'_>,
    entry_hash: &[u8; 32],
    reason: PendingReason,
    projector_version: i64,
) -> anyhow::Result<()> {
    tx.execute(
        "UPDATE table_sync_entries
            SET pending_reason = ?2, pending_projector_version = ?3
          WHERE entry_hash = ?1",
        params![entry_hash.as_slice(), reason.as_db_str(), projector_version],
    )?;
    Ok(())
}

/// Clear an entry's pending mark — it now projects completely.
pub(crate) fn clear_entry_pending(
    tx: &Transaction<'_>,
    entry_hash: &[u8; 32],
) -> anyhow::Result<()> {
    tx.execute(
        "UPDATE table_sync_entries
            SET pending_reason = NULL, pending_projector_version = NULL
          WHERE entry_hash = ?1",
        params![entry_hash.as_slice()],
    )?;
    Ok(())
}

/// One retained-but-unprojected entry, with everything replay needs except its apply context (which
/// comes from [`stream_context`], since the stream id hashes that away).
pub(crate) struct PendingEntry {
    pub(crate) entry_hash: [u8; 32],
    pub(crate) stream_id: StreamId,
    pub(crate) signed_bytes: Vec<u8>,
}

/// Every entry this binary has not fully projected, in stream/lamport order. Ordered for
/// determinism and reproducible diagnostics, NOT for correctness: each replay goes through the
/// unchanged LWW gates, which are arrival-order independent.
pub(crate) fn pending_entries(tx: &Transaction<'_>) -> anyhow::Result<Vec<PendingEntry>> {
    let mut stmt = tx.prepare(
        "SELECT entry_hash, stream_id, signed_bytes FROM table_sync_entries
          WHERE pending_reason IS NOT NULL
          ORDER BY stream_id, lamport, device_fingerprint",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(hash, stream, signed_bytes)| {
            Ok(PendingEntry {
                entry_hash: fixed32(hash)?,
                stream_id: StreamId::from_bytes(fixed32(stream)?),
                signed_bytes,
            })
        })
        .collect()
}

/// The apply context a stored entry needs to be replayed. `scope_stream_id` is a ONE-WAY sha256 of
/// `(repo_id, account_id, scope_id)` and entries store only the stream id, so without this
/// directory a retained entry can never be re-applied: `repo_id` scopes every projected write and
/// `scope_id` resolves the op's table spec.
pub(crate) struct StreamContext {
    pub(crate) repo_id: String,
    pub(crate) scope_id: String,
}

/// Record a stream's apply context, idempotently. Called on every authored and ingested entry, so
/// the directory covers exactly the streams whose entries could ever need replaying.
pub(crate) fn record_stream_context(
    tx: &Transaction<'_>,
    stream: StreamId,
    repo_id: &str,
    account_id: AccountId,
    scope_id: &str,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO table_sync_streams(stream_id, repo_id, account_id, scope_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(stream_id) DO NOTHING",
        params![stream.to_bytes().as_slice(), repo_id, account_id.to_bytes().as_slice(), scope_id],
    )?;
    Ok(())
}

pub(crate) fn stream_context(
    tx: &Transaction<'_>,
    stream: StreamId,
) -> anyhow::Result<Option<StreamContext>> {
    Ok(tx
        .query_row(
            "SELECT repo_id, scope_id FROM table_sync_streams WHERE stream_id = ?1",
            params![stream.to_bytes().as_slice()],
            |row| Ok(StreamContext { repo_id: row.get(0)?, scope_id: row.get(1)? }),
        )
        .optional()?)
}

fn fixed32(bytes: Vec<u8>) -> anyhow::Result<[u8; 32]> {
    <[u8; 32]>::try_from(bytes)
        .map_err(|got| anyhow::anyhow!("stored entry_hash must be 32 bytes, got {}", got.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table_sync::row_op::TypedValue;

    /// The account every table-sync test scopes to. `accept_row_entry` gates on the signing device
    /// being a roster-effective writer of THIS account.
    fn account() -> AccountId {
        AccountId::from_bytes([9; 32])
    }

    /// Insert a roster-effective row so `device_is_effective_writer(account(), fp)` sees `fp` at
    /// `role`. `roster_ref` is the PRIMARY KEY, so it must be unique per row — the fingerprint is a
    /// fine per-device key for a test, and `INSERT OR IGNORE` keeps re-enrollment idempotent.
    fn enroll(c: &rusqlite::Connection, account: AccountId, fp: DeviceFingerprint, role: &str) {
        c.execute(
            "INSERT OR IGNORE INTO account_roster_history
                 (roster_ref, account_id, device_fingerprint, role, effective_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, 0, NULL)",
            params![
                fp.to_bytes().as_slice(),
                account.to_bytes().as_slice(),
                fp.to_bytes().as_slice(),
                role
            ],
        )
        .unwrap();
    }

    /// Mark `fp`'s roster row removed (`closed_at` set) — an off-roster device after removal.
    fn remove_from_roster(c: &rusqlite::Connection, account: AccountId, fp: DeviceFingerprint) {
        c.execute(
            "UPDATE account_roster_history SET closed_at = 1
             WHERE account_id = ?1 AND device_fingerprint = ?2",
            params![account.to_bytes().as_slice(), fp.to_bytes().as_slice()],
        )
        .unwrap();
    }

    fn conn() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        // The behavior tests (chain / lamport / fork / payload) use the `[1; 32]` device; enroll it
        // as an effective writer of account() so the #935 authority gate admits it and they reach
        // the logic under test. The authority tests below use OTHER devices and set their
        // own state.
        enroll(&c, account(), DeviceSecret::from_seed(&[1; 32]).public().fingerprint(), "owner");
        c
    }

    fn stream() -> StreamId {
        StreamId::from_bytes([5; 32])
    }

    fn op(id: &str) -> RowOp {
        RowOp::Remove { table: "t".to_string(), pk: vec![TypedValue::Text(id.to_string())] }
    }

    #[test]
    fn author_then_accept_round_trips_a_row_op() {
        let mut a = conn();
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let tx = a.transaction().unwrap();
        let signed = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
        tx.commit().unwrap();

        // A fresh store accepts the wire and decodes the same op.
        let mut b = conn();
        let tx = b.transaction().unwrap();
        let outcome = accept_row_entry(
            &tx,
            account(),
            stream(),
            &["t"],
            &signed.signed_bytes,
            &secret.public(),
            0,
        )
        .unwrap();
        assert_eq!(outcome, AcceptOutcome::Stored {
            op: op("r1"),
            meta: OpMeta { lamport: 0, device: secret.public().fingerprint() },
            entry_hash: signed.entry.entry_hash,
        });
    }

    #[test]
    fn lamport_advances_and_restores_from_the_stored_tail() {
        let mut a = conn();
        let secret = DeviceSecret::from_seed(&[1; 32]);
        {
            let tx = a.transaction().unwrap();
            assert_eq!(
                author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap().entry.lamport,
                0
            );
            assert_eq!(
                author_row_entry(&tx, stream(), &secret, &op("r2"), 0).unwrap().entry.lamport,
                1
            );
            tx.commit().unwrap();
        }
        // Re-opening the transaction continues from the stored tail (max seen + 1), not from 0.
        let tx = a.transaction().unwrap();
        assert_eq!(
            author_row_entry(&tx, stream(), &secret, &op("r3"), 0).unwrap().entry.lamport,
            2
        );
    }

    #[test]
    fn a_redelivered_entry_is_idempotent() {
        let mut b = conn();
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let signed = {
            let mut a = conn();
            let tx = a.transaction().unwrap();
            let s = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
            tx.commit().unwrap();
            s
        };
        let tx = b.transaction().unwrap();
        assert!(matches!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &signed.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::Stored { .. }
        ));
        assert_eq!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &signed.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::AlreadyPresent,
        );
    }

    #[test]
    fn an_entry_for_a_foreign_stream_is_rejected() {
        let mut b = conn();
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let signed = {
            let mut a = conn();
            let tx = a.transaction().unwrap();
            let s = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
            tx.commit().unwrap();
            s
        };
        let tx = b.transaction().unwrap();
        let other = StreamId::from_bytes([9; 32]);
        assert!(
            accept_row_entry(
                &tx,
                account(),
                other,
                &["t"],
                &signed.signed_bytes,
                &secret.public(),
                0
            )
            .is_err(),
            "an entry cannot be re-homed onto a stream it was not signed for",
        );
    }

    #[test]
    fn a_foreign_table_op_is_stored_inert_and_does_not_wedge_the_chain() {
        let secret = DeviceSecret::from_seed(&[1; 32]);
        // Sender authors two chained ops for table "t".
        let (first, second) = {
            let mut a = conn();
            let tx = a.transaction().unwrap();
            let first = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
            let second = author_row_entry(&tx, stream(), &secret, &op("r2"), 0).unwrap();
            tx.commit().unwrap();
            (first, second)
        };
        let mut b = conn();
        let tx = b.transaction().unwrap();
        // The genesis routed to a scope that does NOT include "t": stored INERT (the chain still
        // advances), not applied.
        assert_eq!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["other"],
                &first.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::StoredInert(PendingReason::TableNotInScope),
        );
        // The chain is not wedged: the next entry (which links to the first) still stores +
        // applies.
        assert!(matches!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &second.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::Stored { .. },
        ));
    }

    #[test]
    fn a_malformed_payload_is_stored_inert_and_does_not_wedge_the_chain() {
        let secret = DeviceSecret::from_seed(&[1; 32]);
        // Sender: a genesis entry with GARBAGE (undecodable) op-bytes, then a valid entry chained
        // onto it.
        let (garbage, valid) = {
            let mut a = conn();
            let tx = a.transaction().unwrap();
            let garbage = entry::sign_entry_from_op_bytes(&secret, stream(), None, 0, vec![0x00]);
            insert_entry(&tx, &garbage.entry, &garbage.signed_bytes, 0, None).unwrap();
            let valid = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
            tx.commit().unwrap();
            (garbage, valid)
        };
        let mut b = conn();
        let tx = b.transaction().unwrap();
        assert_eq!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &garbage.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::StoredInert(PendingReason::UndecodablePayload),
        );
        // One bad payload does not wedge the chain: the next valid entry still applies.
        assert!(matches!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &valid.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::Stored { .. },
        ));
    }

    #[test]
    fn an_out_of_bound_lamport_is_rejected() {
        let secret = DeviceSecret::from_seed(&[1; 32]);
        // A signed genesis claiming a near-maximal lamport would make every peer's next
        // MAX(lamport)+1 overflow i64 at insert — it must be refused before it is stored.
        let poison = entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            None,
            u64::MAX,
            row_op::encode(&op("r1")),
        );
        let mut b = conn();
        let tx = b.transaction().unwrap();
        assert!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &poison.signed_bytes,
                &secret.public(),
                0
            )
            .is_err(),
            "an out-of-bound lamport is rejected before it can poison the stream counter",
        );
    }

    #[test]
    fn a_lamport_jump_beyond_the_advance_bound_is_rejected() {
        // On an empty stream the clock is 0, so the largest acceptable lamport is exactly the
        // advance bound; one past it is a griefing jump (it would dominate every row's LWW
        // and, near the ceiling, halt local authoring). Two fresh streams so the accepted
        // entry does not raise the clock the rejected one is measured against.
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let at_bound = entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            None,
            MAX_LAMPORT_ADVANCE,
            row_op::encode(&op("r1")),
        );
        let beyond = entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            None,
            MAX_LAMPORT_ADVANCE + 1,
            row_op::encode(&op("r2")),
        );

        let mut ok = conn();
        let tx = ok.transaction().unwrap();
        assert!(
            matches!(
                accept_row_entry(
                    &tx,
                    account(),
                    stream(),
                    &["t"],
                    &at_bound.signed_bytes,
                    &secret.public(),
                    0
                )
                .unwrap(),
                AcceptOutcome::Stored { .. },
            ),
            "a lamport exactly at the advance bound is accepted",
        );

        let mut bad = conn();
        let tx = bad.transaction().unwrap();
        assert!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &beyond.signed_bytes,
                &secret.public(),
                0
            )
            .is_err(),
            "a lamport one past the advance bound is refused",
        );
    }

    #[test]
    fn a_gap_is_reported_not_stored() {
        // A second-position entry (lamport 1) arriving before the genesis is a missing predecessor.
        let mut b = conn();
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let second = {
            let mut a = conn();
            let tx = a.transaction().unwrap();
            author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
            let s = author_row_entry(&tx, stream(), &secret, &op("r2"), 0).unwrap();
            tx.commit().unwrap();
            s
        };
        let tx = b.transaction().unwrap();
        assert_eq!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &second.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::MissingPredecessor,
        );
    }

    #[test]
    fn a_fork_linking_past_the_tail_to_a_stored_ancestor_is_a_conflict() {
        let secret = DeviceSecret::from_seed(&[1; 32]);
        // Device A's real chain: e1 (genesis) -> e2.
        let (e1, e2) = {
            let mut a = conn();
            let tx = a.transaction().unwrap();
            let e1 = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
            let e2 = author_row_entry(&tx, stream(), &secret, &op("r2"), 0).unwrap();
            tx.commit().unwrap();
            (e1, e2)
        };
        // A FORK: a SECOND successor of e1 (prev = e1's hash) with a lamport PAST the tail e2.
        let fork = entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            Some(e1.entry.entry_hash),
            e2.entry.lamport + 1,
            row_op::encode(&op("r_fork")),
        );

        let mut b = conn();
        let tx = b.transaction().unwrap();
        accept_row_entry(&tx, account(), stream(), &["t"], &e1.signed_bytes, &secret.public(), 0)
            .unwrap();
        accept_row_entry(&tx, account(), stream(), &["t"], &e2.signed_bytes, &secret.public(), 0)
            .unwrap();
        // Links past the tail to the STORED ancestor e1 (which already has a successor) → an
        // equivocation, not a missing predecessor.
        assert_eq!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &fork.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::Fork,
            "a fork linking to a stored ancestor is a Fork, not a MissingPredecessor",
        );
    }

    // ─── #935: roster/role authority gate ───

    /// A signed, decodable row entry from `secret` on `stream()` — built without storing (like the
    /// garbage/fork helpers), enough to drive the authority gate.
    fn signed_row(secret: &DeviceSecret, id: &str) -> SignedEntry {
        entry::sign_entry_from_op_bytes(secret, stream(), None, 0, row_op::encode(&op(id)))
    }

    fn accept(
        tx: &Transaction<'_>,
        acct: AccountId,
        signed: &SignedEntry,
        pubkey: &DevicePublic,
    ) -> AcceptOutcome {
        accept_row_entry(tx, acct, stream(), &["t"], &signed.signed_bytes, pubkey, 0).unwrap()
    }

    fn stream_entry_count(tx: &Transaction<'_>) -> i64 {
        tx.query_row(
            "SELECT COUNT(*) FROM table_sync_entries WHERE stream_id = ?1",
            params![stream().to_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn an_off_roster_device_is_unauthorized_and_stores_nothing() {
        let secret = DeviceSecret::from_seed(&[2; 32]); // never enrolled
        let signed = signed_row(&secret, "r1");
        let mut b = conn();
        let tx = b.transaction().unwrap();
        assert_eq!(accept(&tx, account(), &signed, &secret.public()), AcceptOutcome::Unauthorized);
        assert_eq!(stream_entry_count(&tx), 0, "an unauthorized entry advances no chain");
    }

    #[test]
    fn a_read_only_device_is_unauthorized() {
        let secret = DeviceSecret::from_seed(&[3; 32]);
        let signed = signed_row(&secret, "r1");
        let mut b = conn();
        enroll(&b, account(), secret.public().fingerprint(), "read_only");
        let tx = b.transaction().unwrap();
        assert_eq!(accept(&tx, account(), &signed, &secret.public()), AcceptOutcome::Unauthorized);
    }

    #[test]
    fn a_member_and_an_owner_may_author() {
        for (seed, role) in [([4u8; 32], "member"), ([1u8; 32], "owner")] {
            let secret = DeviceSecret::from_seed(&seed);
            let signed = signed_row(&secret, "r1");
            let mut b = conn();
            enroll(&b, account(), secret.public().fingerprint(), role);
            let tx = b.transaction().unwrap();
            assert!(
                matches!(
                    accept(&tx, account(), &signed, &secret.public()),
                    AcceptOutcome::Stored { .. }
                ),
                "{role} may author table rows",
            );
        }
    }

    #[test]
    fn a_removed_writer_is_unauthorized() {
        let secret = DeviceSecret::from_seed(&[1; 32]); // conn() enrolls it as owner
        let signed = signed_row(&secret, "r1");
        let mut b = conn();
        remove_from_roster(&b, account(), secret.public().fingerprint());
        let tx = b.transaction().unwrap();
        assert_eq!(accept(&tx, account(), &signed, &secret.public()), AcceptOutcome::Unauthorized);
    }

    #[test]
    fn a_writer_in_another_account_is_unauthorized_here() {
        let secret = DeviceSecret::from_seed(&[1; 32]); // owner in account()
        let signed = signed_row(&secret, "r1");
        let mut b = conn();
        let tx = b.transaction().unwrap();
        let other = AccountId::from_bytes([0xAA; 32]);
        assert_eq!(accept(&tx, other, &signed, &secret.public()), AcceptOutcome::Unauthorized);
    }

    #[test]
    fn the_authority_gate_precedes_forward_compat_retention() {
        // An off-roster device authoring an UNDECODABLE payload is dropped Unauthorized, never
        // retained StoredInert: the gate runs before the forward-compat path, so an unauthorized
        // principal can never populate a retained stream.
        let secret = DeviceSecret::from_seed(&[2; 32]);
        let garbage = entry::sign_entry_from_op_bytes(&secret, stream(), None, 0, vec![0x00]);
        let mut b = conn();
        let tx = b.transaction().unwrap();
        assert_eq!(accept(&tx, account(), &garbage, &secret.public()), AcceptOutcome::Unauthorized);
        assert_eq!(stream_entry_count(&tx), 0);
    }

    #[test]
    fn an_unauthorized_drop_heals_after_the_device_is_enrolled() {
        // Roster lag: dropped before the author's DeviceAdd folds locally, accepted on the re-offer
        // after it does — so `Unauthorized` is retryable, not terminal.
        let secret = DeviceSecret::from_seed(&[5; 32]);
        let signed = signed_row(&secret, "r1");
        let mut b = conn();
        {
            let tx = b.transaction().unwrap();
            assert_eq!(
                accept(&tx, account(), &signed, &secret.public()),
                AcceptOutcome::Unauthorized
            );
            tx.commit().unwrap();
        }
        enroll(&b, account(), secret.public().fingerprint(), "member"); // the DeviceAdd folded
        let tx = b.transaction().unwrap();
        assert!(
            matches!(
                accept(&tx, account(), &signed, &secret.public()),
                AcceptOutcome::Stored { .. }
            ),
            "the re-offer is accepted once the author is roster-effective",
        );
    }

    #[test]
    fn already_present_takes_precedence_over_a_later_removal() {
        // An entry stored while the device WAS a writer still reports AlreadyPresent after removal
        // (the gate sits after `entry_exists`), preserving dedup/frontier semantics.
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let signed = signed_row(&secret, "r1");
        let mut b = conn();
        {
            let tx = b.transaction().unwrap();
            assert!(matches!(
                accept(&tx, account(), &signed, &secret.public()),
                AcceptOutcome::Stored { .. }
            ));
            tx.commit().unwrap();
        }
        remove_from_roster(&b, account(), secret.public().fingerprint());
        let tx = b.transaction().unwrap();
        assert_eq!(
            accept(&tx, account(), &signed, &secret.public()),
            AcceptOutcome::AlreadyPresent
        );
    }
}
