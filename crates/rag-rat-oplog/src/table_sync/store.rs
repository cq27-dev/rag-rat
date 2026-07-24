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

/// The result of accepting a foreign entry. A chain-continuous entry is ALWAYS stored (so one bad
/// payload cannot wedge the device's chain); whether its payload is APPLIED is a separate decision.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AcceptOutcome {
    /// Stored, and its known in-scope row op is ready to apply.
    Stored {
        op: RowOp,
        meta: OpMeta,
    },
    /// Stored and retained, but NOT applied — an undecodable payload, a future op-kind, or a table
    /// not in this scope. The chain still advanced; the `&str` is the reason, for reporting.
    StoredInert(&'static str),
    AlreadyPresent,
    /// The lamport advances past the tail but the entry does not link to it (a gap): the
    /// predecessor has not arrived. Routine under out-of-order delivery; the transport retries
    /// after backfill.
    MissingPredecessor,
    /// The entry conflicts with the stored chain (a second genesis, or a lamport at/behind the
    /// tail) — an equivocation the transport milestone will quarantine with evidence.
    Fork,
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
    insert_entry(tx, &signed.entry, &signed.signed_bytes, now_ms)?;
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
    match classify(tx, expected_stream, &verified)? {
        ChainFit::Ok => {},
        ChainFit::Gap => return Ok(AcceptOutcome::MissingPredecessor),
        ChainFit::Conflict => return Ok(AcceptOutcome::Fork),
    }
    // Chain-continuous: store now so a bad payload can never wedge the device's chain.
    insert_entry(tx, &verified, signed_bytes, now_ms)?;

    // Classify the payload for application — the entry is already durably stored either way.
    Ok(match row_op::decode(&verified.op_bytes) {
        Err(_) => AcceptOutcome::StoredInert("undecodable op payload"),
        Ok(DecodedRowOp::Unknown { .. }) => AcceptOutcome::StoredInert("unknown op-kind"),
        Ok(DecodedRowOp::Known(op)) =>
            if expected_tables.contains(&op.table()) {
                AcceptOutcome::Stored {
                    op,
                    meta: OpMeta { lamport: verified.lamport, device: verified.device_fingerprint },
                }
            } else {
                AcceptOutcome::StoredInert("table not in scope")
            },
    })
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

fn insert_entry(
    tx: &Transaction<'_>,
    verified: &VerifiedEntry,
    signed_bytes: &[u8],
    now_ms: i64,
) -> anyhow::Result<()> {
    let stream_bytes = verified.stream_id.to_bytes();
    let device_bytes = verified.device_fingerprint.to_bytes();
    let prev_hash: Option<Vec<u8>> = verified.prev_hash.map(|h| h.to_vec());
    tx.execute(
        "INSERT INTO table_sync_entries(
             entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
             received_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            verified.entry_hash.as_slice(),
            stream_bytes.as_slice(),
            device_bytes.as_slice(),
            i64::try_from(verified.lamport)?,
            prev_hash,
            signed_bytes,
            now_ms,
        ],
    )?;
    Ok(())
}

fn fixed32(bytes: Vec<u8>) -> anyhow::Result<[u8; 32]> {
    <[u8; 32]>::try_from(bytes)
        .map_err(|got| anyhow::anyhow!("stored entry_hash must be 32 bytes, got {}", got.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table_sync::row_op::TypedValue;

    fn conn() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
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
        let outcome =
            accept_row_entry(&tx, stream(), &["t"], &signed.signed_bytes, &secret.public(), 0)
                .unwrap();
        assert_eq!(outcome, AcceptOutcome::Stored {
            op: op("r1"),
            meta: OpMeta { lamport: 0, device: secret.public().fingerprint() }
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
            accept_row_entry(&tx, stream(), &["t"], &signed.signed_bytes, &secret.public(), 0)
                .unwrap(),
            AcceptOutcome::Stored { .. }
        ));
        assert_eq!(
            accept_row_entry(&tx, stream(), &["t"], &signed.signed_bytes, &secret.public(), 0)
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
            accept_row_entry(&tx, other, &["t"], &signed.signed_bytes, &secret.public(), 0)
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
            accept_row_entry(&tx, stream(), &["other"], &first.signed_bytes, &secret.public(), 0)
                .unwrap(),
            AcceptOutcome::StoredInert("table not in scope"),
        );
        // The chain is not wedged: the next entry (which links to the first) still stores +
        // applies.
        assert!(matches!(
            accept_row_entry(&tx, stream(), &["t"], &second.signed_bytes, &secret.public(), 0)
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
            insert_entry(&tx, &garbage.entry, &garbage.signed_bytes, 0).unwrap();
            let valid = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
            tx.commit().unwrap();
            (garbage, valid)
        };
        let mut b = conn();
        let tx = b.transaction().unwrap();
        assert_eq!(
            accept_row_entry(&tx, stream(), &["t"], &garbage.signed_bytes, &secret.public(), 0)
                .unwrap(),
            AcceptOutcome::StoredInert("undecodable op payload"),
        );
        // One bad payload does not wedge the chain: the next valid entry still applies.
        assert!(matches!(
            accept_row_entry(&tx, stream(), &["t"], &valid.signed_bytes, &secret.public(), 0)
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
            accept_row_entry(&tx, stream(), &["t"], &poison.signed_bytes, &secret.public(), 0)
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
            accept_row_entry(&tx, stream(), &["t"], &beyond.signed_bytes, &secret.public(), 0)
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
            accept_row_entry(&tx, stream(), &["t"], &second.signed_bytes, &secret.public(), 0)
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
        accept_row_entry(&tx, stream(), &["t"], &e1.signed_bytes, &secret.public(), 0).unwrap();
        accept_row_entry(&tx, stream(), &["t"], &e2.signed_bytes, &secret.public(), 0).unwrap();
        // Links past the tail to the STORED ancestor e1 (which already has a successor) → an
        // equivocation, not a missing predecessor.
        assert_eq!(
            accept_row_entry(&tx, stream(), &["t"], &fork.signed_bytes, &secret.public(), 0)
                .unwrap(),
            AcceptOutcome::Fork,
            "a fork linking to a stored ancestor is a Fork, not a MissingPredecessor",
        );
    }
}
