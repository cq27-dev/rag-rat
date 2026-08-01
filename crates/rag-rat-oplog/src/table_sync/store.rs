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
//! chain.
//!
//! An entry whose predecessor has NOT arrived is held in `table_sync_gapped_entries` and promoted
//! once the predecessor is accepted, because out-of-order delivery is the normal condition on a
//! transport rather than an error. That table is deliberately separate: six queries here read
//! `table_sync_entries` as "the accepted chain" — the authoring Lamport clock, the lamport-advance
//! bound, [`chain_tail`], [`entry_exists`], the winning-entry lookup, and the refold's pending set
//! — and every one of them must keep excluding an entry that is not on a chain. A held entry is a
//! CHAIN state and carries no `pending_reason`; do not conflate it with the PROJECTION state that
//! column tracks.
//!
//! Fork EVIDENCE (proving an equivocation to a peer) remains the transport milestone's job — a
//! conflict is reported here, not durably quarantined.

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
/// by a future binary, so a rename needs a migration exactly like a column rename. The tokens are
/// strum-derived rather than hand-matched so the write and parse sides cannot drift apart as
/// variants are added, and [`PendingReason::as_db_str`] / [`PendingReason::from_db_str`] stay the
/// only paths to the stored form. The exact strings are pinned by test, so a `serialize_all` change
/// cannot silently move them.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum PendingReason {
    /// The op carries a column this registry does not know — a newer producer. Nothing is written:
    /// applying the known subset would leave a row NO device ever authored, and publishing that row
    /// lets the producer re-author the hole at a winning lamport (see [`super::apply`]).
    UnknownColumn,
    /// The op was authored against a NEWER synced column set than this binary knows. We cannot know
    /// what a later spec means — and the op may name columns we lack — so nothing is written until
    /// a binary that understands it replays the entry.
    NewerSpecVersion,
    /// The op omits a column its OWN claimed version was obliged to carry. Whole-row LWW needs the
    /// full after-image, so nothing is written.
    ///
    /// Since #1002 this no longer means "an older producer": an op that genuinely predates a column
    /// is completed from that column's declared default and applies inline, never reaching here.
    /// What remains is a BROKEN or mis-stamped producer, which under additive-only evolution no
    /// future binary can redeem on its own — the sender has to fix its bug and author again (a new
    /// entry; redelivery of this one short-circuits on `entry_exists`). It parks rather than
    /// quarantining because a forgotten `spec_version` bump is the likeliest cause and parking is
    /// what keeps that operator error recoverable, but do not expect the entry itself to clear.
    PartialAfterImage,
    /// The op carries a cell for a column introduced AFTER the version it claims — self-
    /// contradictory, so the stamp cannot be trusted to decide what the op predates. The one half
    /// of the advisory version a receiver can check against its own registry.
    MisstampedSpecVersion,
    /// The op-kind is outside this binary's row-op vocabulary.
    UnknownOpKind,
    /// The op bytes do not decode at all.
    UndecodablePayload,
    /// The op's table is not in this binary's registry for the entry's scope.
    TableNotInScope,
    /// The entry's stream has no directory row, so it cannot be placed at all — no repo to apply it
    /// to and no scope to resolve its spec (the stream id hashes both away).
    ///
    /// Deliberately NOT a deferral. Every author and every ingest records the context first, and
    /// repo purge sweeps the entries with the directory, so reaching this state at all means
    /// something outside the engine removed the row — and if it happened it is almost certainly
    /// permanent. Retrying it on every open would be the one reason that makes every store open pay
    /// forever. It is recorded so the state is diagnosable, and left alone.
    NoStreamContext,
    /// Account authority does not currently select the stream directory's incarnation. Absence,
    /// contest, or a different apparent current reference is non-monotone: later account entries
    /// can establish, disambiguate, or restore this reference, so replay must retain debt and
    /// retry.
    DeferredIncarnationAuthority,
    /// A live row whose content differs from what was last published: an edit no peer has seen,
    /// which replaying this entry would silently overwrite.
    DeferredUnsentEdit,
    /// The row is gone but its published identity survives — a local delete not yet authored, which
    /// replaying an upsert would undo by recreating the row.
    DeferredUnsentDelete,
    /// A `Remove` over a row holding a cell this binary cannot read (#1017). Applying it would
    /// delete the row outright, local-only columns included, and repair nothing.
    DeferredUnreadableRow,
    /// The row was published under a different column set AND its winning op could not be resolved,
    /// so whether it holds an unsent edit cannot be established either way. Conservative by design:
    /// refuse to replay rather than risk overwriting.
    DeferredUnresolvedWinner,
}

impl PendingReason {
    pub(crate) fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Whether this entry is waiting on mutable row/account state rather than on a later binary.
    ///
    /// The two families are redeemed by different events and so have different retry triggers. A
    /// version gap clears when this binary understands more, which the projector version records —
    /// so replaying one before the next bump is pure cost. A deferral clears when mutable state
    /// changes (a row edit gets authored, an unreadable cell is repaired, or incarnation authority
    /// resolves), which nothing stamps in this projection; the only way to find out is to look
    /// again, so a deferral is owed a replay at every open (#1005).
    ///
    /// Every non-deferral variant is a version gap, INCLUDING `PartialAfterImage` (a broken
    /// producer its own doc says will not clear here) and `NoStreamContext`: as deferrals those
    /// would retry forever with nothing able to redeem them.
    /// Whether this deferral rests on PROOF that local work would be destroyed, as opposed to an
    /// inability to establish the row's state either way.
    ///
    /// The difference decides what the LIVE ingest path does. `DeferredUnresolvedWinner` is the one
    /// reason that is merely unprovable — the row was published under a different column set and
    /// its winning op cannot be projected — and acting on it at ingest would delay a deletion
    /// across a version skew, which is the convergence wedge [`super::apply::apply_row_op`]
    /// deliberately keeps `Remove` out of. The refold pays that cost because it runs at store
    /// open with no driver to have ordered it; ingest does not, because it is the path a driver
    /// orders.
    pub(crate) fn is_proven_unsent_work(self) -> bool {
        match self {
            Self::DeferredUnsentEdit | Self::DeferredUnsentDelete | Self::DeferredUnreadableRow =>
                true,
            Self::DeferredUnresolvedWinner | Self::DeferredIncarnationAuthority => false,
            other => {
                debug_assert!(!other.is_deferral(), "every deferral must state its confidence");
                false
            },
        }
    }

    pub(crate) fn is_deferral(self) -> bool {
        match self {
            Self::DeferredUnsentEdit
            | Self::DeferredUnsentDelete
            | Self::DeferredUnreadableRow
            | Self::DeferredUnresolvedWinner
            | Self::DeferredIncarnationAuthority => true,
            Self::UnknownColumn
            | Self::NewerSpecVersion
            | Self::PartialAfterImage
            | Self::MisstampedSpecVersion
            | Self::UnknownOpKind
            | Self::UndecodablePayload
            | Self::TableNotInScope
            | Self::NoStreamContext => false,
        }
    }

    /// Exact-token parse: an unrecognized value is `None`, never coerced to a default — a stored
    /// token this binary does not know means a NEWER binary wrote it, and guessing would silently
    /// reclassify why an entry is outstanding.
    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        value.parse().ok()
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
        /// Where this entry sits on its chain — what a gapped successor cites. `None` for a
        /// genesis.
        prev_hash: Option<[u8; 32]>,
    },
    /// Stored and retained, but NOT applied — an undecodable payload, a future op-kind, or a table
    /// not in this scope. The chain still advanced, and the entry is marked pending so a later
    /// binary replays it.
    StoredInert {
        reason: PendingReason,
        entry_hash: [u8; 32],
        prev_hash: Option<[u8; 32]>,
    },
    AlreadyPresent,
    /// The lamport advances past the tail but the entry does not link to it (a gap): the
    /// predecessor has not arrived. Routine under out-of-order delivery, which is the NORMAL
    /// condition on a transport — so the entry is RETAINED in `table_sync_gapped_entries` and
    /// promoted once its predecessor is accepted, rather than dropped and left to redelivery in
    /// exact causal order.
    GapRetained,
    /// A gapped entry already held. Deliberately distinct from [`Self::AlreadyPresent`]: that one
    /// is a durable promise, whereas a gapped entry can still be evicted by the per-chain cap, so
    /// a caller must not build a sync frontier on this as if the entry were settled.
    AlreadyGapped,
    /// The entry gapped AND is further ahead than every entry the chain is already holding, so the
    /// per-chain cap dropped it rather than a nearer one. Reported rather than folded into
    /// [`Self::GapRetained`]: the entry is NOT held, and a caller that treats it as held would wait
    /// forever for a promotion that cannot come. RETRYABLE — once the entries between it and the
    /// tail arrive and promote, there is room and it is no longer the furthest ahead.
    GapChainFull,
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
    let stored_tail = chain_tail(tx, stream, device)?;
    if let Some(witness) = chain_witness(tx, stream, device)?
        && stored_tail != Some(witness)
    {
        anyhow::bail!(
            "table-sync chain continuity is not restored through the retained local tip; refusing \
             to author a second genesis"
        );
    }
    let lamport = next_stream_lamport(tx, stream)?;
    let prev_hash = stored_tail.map(|(_, entry_hash)| entry_hash);
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
    let next = match stream_max_lamport(tx, stream)? {
        Some(lamport) => lamport.checked_add(1).context("stream lamport overflow")?,
        None => 0,
    };
    // Cap at the same ceiling `accept_row_entry` enforces, so a locally-authored entry can never
    // exceed what peers accept. Only reachable if a near-ceiling entry was ingested (impossible at
    // legitimate op volume); refusing to author is a bounded halt, never a divergent split.
    anyhow::ensure!(next < MAX_ENTRY_LAMPORT, "stream lamport ceiling reached");
    Ok(next)
}

/// The accepted or retained high-water for one stream. Both authoring and foreign-entry bounded
/// advance must use this exact clock basis: a purged accepted log leaves only its retained witness.
fn stream_max_lamport(tx: &Transaction<'_>, stream: StreamId) -> anyhow::Result<Option<u64>> {
    let highest: Option<i64> = tx.query_row(
        "SELECT MAX(lamport) FROM (
             SELECT lamport FROM table_sync_entries WHERE stream_id = ?1
             UNION ALL
             SELECT lamport FROM table_sync_chain_tips WHERE stream_id = ?1
         )",
        params![stream.to_bytes().as_slice()],
        |row| row.get::<_, Option<i64>>(0),
    )?;
    highest.map(u64::try_from).transpose().map_err(Into::into)
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
    let stream_max = stream_max_lamport(tx, expected_stream)?.unwrap_or(0);
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
    // Dedupe against the gapped table HERE, beside the accepted-entry check and BEFORE the
    // authority gate, so a redelivered gapped entry reports the same way whether or not its
    // device is still roster-effective — the dedup-precedence-over-authority ordering the
    // accepted path already has. In the `Gap` arm below instead, a removed device's redelivery
    // would report `Unauthorized`.
    if gapped_entry_exists(tx, &verified.entry_hash)? {
        return Ok(AcceptOutcome::AlreadyGapped);
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
        ChainFit::Ok | ChainFit::Restore => {},
        ChainFit::Gap =>
            return Ok(if retain_gapped_entry(tx, &verified, signed_bytes, now_ms)? {
                AcceptOutcome::GapRetained
            } else {
                AcceptOutcome::GapChainFull
            }),
        ChainFit::Conflict => return Ok(AcceptOutcome::Fork),
    }
    // Classify the payload BEFORE storing, so the entry lands with its projection state recorded in
    // the same INSERT — no store-then-mark window, and no second write.
    let inert = |reason| AcceptOutcome::StoredInert {
        reason,
        entry_hash: verified.entry_hash,
        prev_hash: verified.prev_hash,
    };
    let outcome = match row_op::decode(&verified.op_bytes) {
        Err(_) => inert(PendingReason::UndecodablePayload),
        Ok(DecodedRowOp::Unknown { .. }) => inert(PendingReason::UnknownOpKind),
        Ok(DecodedRowOp::Known(op)) =>
            if expected_tables.contains(&op.table()) {
                AcceptOutcome::Stored {
                    op,
                    meta: OpMeta { lamport: verified.lamport, device: verified.device_fingerprint },
                    entry_hash: verified.entry_hash,
                    prev_hash: verified.prev_hash,
                }
            } else {
                inert(PendingReason::TableNotInScope)
            },
    };
    // Chain-continuous: store now so a bad payload can never wedge the device's chain.
    let pending = match &outcome {
        AcceptOutcome::StoredInert { reason, .. } => Some(*reason),
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
    Restore,
    Gap,
    Conflict,
}

fn classify(
    tx: &Transaction<'_>,
    stream: StreamId,
    verified: &VerifiedEntry,
) -> anyhow::Result<ChainFit> {
    let tail = chain_tail(tx, stream, verified.device_fingerprint)?;
    if tail.is_none()
        && let Some((witness_lamport, witness_hash)) =
            chain_witness(tx, stream, verified.device_fingerprint)?
    {
        if verified.entry_hash == witness_hash && verified.lamport == witness_lamport {
            return Ok(ChainFit::Restore);
        }
        return Ok(match verified.prev_hash {
            Some(prev) if prev == witness_hash && verified.lamport > witness_lamport =>
                ChainFit::Ok,
            None => ChainFit::Conflict,
            Some(_) if verified.lamport <= witness_lamport => ChainFit::Conflict,
            Some(_) => ChainFit::Gap,
        });
    }
    Ok(match (verified.prev_hash, tail) {
        // A genesis (no predecessor) is the valid first entry of this device's chain; a genesis
        // when a chain already exists is a second head — an equivocation.
        (None, None) => ChainFit::Ok,
        (None, Some(_)) => ChainFit::Conflict,
        // A non-genesis whose device has no chain yet. Its predecessor has not been delivered —
        // UNLESS we already hold it, in which case it belongs to a DIFFERENT device's chain and
        // this link is structurally impossible: `author_row_entry` always sets `prev_hash` from the
        // signer's OWN chain tail, so a cross-chain citation can never be honest. Reporting `Gap`
        // there would retain an entry that no arrival can ever redeem, because this arm keys the
        // tail on the signer's chain and would keep returning `Gap` forever.
        (Some(prev), None) =>
            if entry_exists(tx, &prev)? {
                ChainFit::Conflict
            } else {
                ChainFit::Gap
            },
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

fn chain_witness(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: DeviceFingerprint,
) -> anyhow::Result<Option<(u64, [u8; 32])>> {
    let row = tx
        .query_row(
            "SELECT lamport, entry_hash FROM table_sync_chain_tips
              WHERE stream_id = ?1 AND device_fingerprint = ?2",
            params![stream.to_bytes().as_slice(), device.to_bytes().as_slice()],
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

/// A verified entry held in `table_sync_gapped_entries`, awaiting its chain predecessor.
pub(crate) struct GappedEntry {
    pub entry_hash: [u8; 32],
    pub prev_hash: [u8; 32],
    pub signed_bytes: Vec<u8>,
}

/// The most gapped entries held for one `(stream, device)` chain.
///
/// Only a roster-effective WRITER reaches the retention below — the authority gate in
/// [`accept_row_entry`] runs first — so this bounds a misbehaving ROSTER MEMBER, whose remedy is
/// device removal. That is the same posture [`MAX_LAMPORT_ADVANCE`] documents for in-window lamport
/// griefing, not a defense against arbitrary peers.
const MAX_GAPPED_PER_CHAIN: usize = 4096;

fn gapped_entry_exists(tx: &Transaction<'_>, entry_hash: &[u8; 32]) -> anyhow::Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM table_sync_gapped_entries WHERE entry_hash = ?1",
            params![entry_hash.as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Hold a verified entry whose predecessor has not arrived, keeping the chain's NEAREST-TAIL
/// `MAX_GAPPED_PER_CHAIN` entries when the cap is reached.
///
/// The policy is "retain the entries closest to the tail", because those promote soonest — an entry
/// far ahead of the clock needs everything between it and the tail to arrive first. That is why the
/// cap evicts rather than simply refusing: a peer that filled the table early must not be able to
/// block the near-tail entry the chain is actually waiting on.
///
/// The newcomer competes on the same footing as the rows already held, so an arrival that is ITSELF
/// the furthest-ahead is the one refused. Evicting the stored maximum unconditionally would invert
/// the policy exactly when it matters — a table full of near-tail entries would be hollowed out one
/// row at a time by a stream of ever-higher-lamport arrivals.
///
/// Returns whether the entry is now held.
fn retain_gapped_entry(
    tx: &Transaction<'_>,
    verified: &VerifiedEntry,
    signed_bytes: &[u8],
    now_ms: i64,
) -> anyhow::Result<bool> {
    let stream_bytes = verified.stream_id.to_bytes();
    let device_bytes = verified.device_fingerprint.to_bytes();
    // `classify` returns `Gap` only for an entry that HAS a predecessor — a genesis never gaps — so
    // the NOT NULL column always has a value here.
    let prev_hash = verified.prev_hash.context("a gapped entry always has a predecessor")?;
    let held: i64 = tx.query_row(
        "SELECT COUNT(*) FROM table_sync_gapped_entries
          WHERE stream_id = ?1 AND device_fingerprint = ?2",
        params![stream_bytes.as_slice(), device_bytes.as_slice()],
        |row| row.get(0),
    )?;
    if usize::try_from(held)? >= MAX_GAPPED_PER_CHAIN {
        let (furthest_hash, furthest_lamport) = tx.query_row(
            "SELECT entry_hash, lamport FROM table_sync_gapped_entries
              WHERE stream_id = ?1 AND device_fingerprint = ?2
              ORDER BY lamport DESC, entry_hash DESC LIMIT 1",
            params![stream_bytes.as_slice(), device_bytes.as_slice()],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
        )?;
        // Rank on `(lamport, entry_hash)`, the SAME total order the victim was chosen by. On
        // lamport alone a tie at the cap boundary would resolve as "whoever is already held wins",
        // which is insertion order again — the property the hash tie-break exists to remove.
        if (i64::try_from(verified.lamport)?, verified.entry_hash.as_slice())
            >= (furthest_lamport, furthest_hash.as_slice())
        {
            // The arrival is the furthest-ahead of the whole set: it is the one the policy drops.
            return Ok(false);
        }
        tx.execute("DELETE FROM table_sync_gapped_entries WHERE entry_hash = ?1", params![
            furthest_hash
        ])?;
    }
    tx.execute(
        "INSERT INTO table_sync_gapped_entries(
             entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
             gapped_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            verified.entry_hash.as_slice(),
            stream_bytes.as_slice(),
            device_bytes.as_slice(),
            i64::try_from(verified.lamport)?,
            prev_hash.as_slice(),
            signed_bytes,
            now_ms,
        ],
    )?;
    Ok(true)
}

/// Discard every held entry on `stream` that descends from `root` — the subtree behind an entry
/// that has just been judged a fork, or abandoned behind one.
///
/// A fork is never stored, so nothing will ever put `root`'s hash on the chain, so an entry citing
/// it can never be promoted; and because it is keyed to a hash no future acceptance will produce,
/// no later probe would look at it again either. Without this sweep such an entry sits in the table
/// until the per-chain cap or a repo purge removes it, occupying a slot a promotable entry could
/// have used.
///
/// The walk is scoped to the STREAM, not to the rejected entry's device. A citation from another
/// device's chain is structurally impossible, but it is retained anyway (until the cited hash is
/// held, nothing distinguishes it from an ordinary missing predecessor) — and for a REJECTED hash
/// there is no later event that could retire it, because [`discard_foreign_chain_citations`] fires
/// only on an ACCEPTED one. A device-filtered walk here would leave exactly those rows stranded.
///
/// Stream scope is also the safety boundary: a stream id is derived from `(repo_id, account_id,
/// scope_id)`, so staying within one stream stays within the repo whose write lock the caller
/// holds. Sweeping by `prev_hash` alone would reach other repos' rows in a shared database.
///
/// Iterative over a worklist, for the same reason promotion is: the abandoned subtree can be as
/// deep as the chain is long.
///
/// Returns how many entries were discarded.
pub(crate) fn discard_gapped_descendants(
    tx: &Transaction<'_>,
    stream: StreamId,
    root: &[u8; 32],
) -> anyhow::Result<usize> {
    let stream_bytes = stream.to_bytes();
    let mut discarded = 0;
    let mut worklist = vec![*root];
    while let Some(parent) = worklist.pop() {
        let children: Vec<Vec<u8>> = tx
            .prepare(
                "SELECT entry_hash FROM table_sync_gapped_entries
                  WHERE stream_id = ?1 AND prev_hash = ?2",
            )?
            .query_map(params![stream_bytes.as_slice(), parent.as_slice()], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for child in children {
            tx.execute("DELETE FROM table_sync_gapped_entries WHERE entry_hash = ?1", params![
                child
            ])?;
            worklist.push(fixed32(child)?);
            discarded += 1;
        }
    }
    Ok(discarded)
}

/// Discard held entries on OTHER devices' chains that cite `hash` as their predecessor, together
/// with everything behind them.
///
/// A chain is per `(stream, device)` and [`author_row_entry`] always links to the signer's OWN
/// tail, so an entry citing a predecessor from a different device's chain can never be honest. Such
/// an entry is nonetheless retained on arrival, because until the cited hash is held there is no
/// way to tell it from an ordinary missing predecessor.
///
/// Accepting the cited entry is the moment that becomes decidable, and it is also the ONLY moment:
/// [`classify`] keys the tail on the citing device's own chain, so re-examining the entry later
/// would just report `Gap` again forever. Without this sweep it occupies that chain's hold capacity
/// until eviction or a repo purge.
///
/// A citation from a different STREAM is deliberately NOT swept, and this is the one residual the
/// sweep leaves. Reaching it would mean matching on `prev_hash` without the `stream_id` equality,
/// and a stream id is derived from `(repo_id, account_id, incarnation_ref, scope_id)` — so that
/// query would delete rows belonging to OTHER REPOS in a shared database, under a write lock scoped
/// to this one.
/// Trading a bounded capacity leak for a cross-repo write outside its lock is the wrong direction.
/// The leak is charged to the malformed signer's own chain, is capped by `MAX_GAPPED_PER_CHAIN`,
/// and is reclaimed by the deferred retention/GC horizon over this table. (The reverse arrival
/// order is already handled: [`classify`] consults `entry_exists`, which is not stream-scoped, so a
/// citation of an already-held entry is judged on arrival rather than retained.)
///
/// Returns how many entries were discarded.
pub(crate) fn discard_foreign_chain_citations(
    tx: &Transaction<'_>,
    stream: StreamId,
    own_device: DeviceFingerprint,
    hash: &[u8; 32],
) -> anyhow::Result<usize> {
    let stream_bytes = stream.to_bytes();
    let own_bytes = own_device.to_bytes();
    let citations: Vec<Vec<u8>> = tx
        .prepare(
            "SELECT entry_hash FROM table_sync_gapped_entries
              WHERE stream_id = ?1 AND prev_hash = ?2 AND device_fingerprint != ?3",
        )?
        .query_map(
            params![stream_bytes.as_slice(), hash.as_slice(), own_bytes.as_slice()],
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let mut discarded = 0;
    for entry_hash in citations {
        tx.execute("DELETE FROM table_sync_gapped_entries WHERE entry_hash = ?1", params![
            entry_hash
        ])?;
        discarded += 1;
        // Everything queued behind it is orphaned by the same argument.
        discarded += discard_gapped_descendants(tx, stream, &fixed32(entry_hash)?)?;
    }
    Ok(discarded)
}

/// Remove and return one gapped entry of `(stream, device)` whose predecessor is `prev_hash`.
///
/// TAKE, not read: the caller feeds the entry straight back through [`accept_row_entry`], which
/// re-verifies it and re-classifies it against the now-advanced chain. Leaving it here would either
/// duplicate it on acceptance or strand it when it turns out to be a fork.
///
/// Ordered by `(lamport, entry_hash)`, so which of two equivocating siblings takes the successor
/// slot is a property of the ENTRIES rather than of insertion order. The hash tie-break is
/// load-bearing, not decoration: the schema deliberately admits two siblings at one lamport
/// (§`table_sync_gapped_entries`), and on lamport alone SQLite's choice between them falls back to
/// physical row order — so two replicas holding the same pair could accept different successors and
/// project different rows, with each then classifying the other's winner as a fork.
pub(crate) fn take_gapped_child(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: DeviceFingerprint,
    prev_hash: &[u8; 32],
) -> anyhow::Result<Option<GappedEntry>> {
    let stream_bytes = stream.to_bytes();
    let device_bytes = device.to_bytes();
    let row = tx
        .query_row(
            "SELECT entry_hash, prev_hash, signed_bytes FROM table_sync_gapped_entries
              WHERE stream_id = ?1 AND device_fingerprint = ?2 AND prev_hash = ?3
              ORDER BY lamport, entry_hash LIMIT 1",
            params![stream_bytes.as_slice(), device_bytes.as_slice(), prev_hash.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((entry_hash, prev, signed_bytes)) = row else {
        return Ok(None);
    };
    tx.execute("DELETE FROM table_sync_gapped_entries WHERE entry_hash = ?1", params![
        entry_hash.as_slice()
    ])?;
    Ok(Some(GappedEntry {
        entry_hash: fixed32(entry_hash)?,
        prev_hash: fixed32(prev)?,
        signed_bytes,
    }))
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
    tx.execute(
        "INSERT INTO table_sync_chain_tips(
             stream_id, device_fingerprint, lamport, entry_hash
         ) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(stream_id, device_fingerprint) DO UPDATE SET
             lamport = excluded.lamport, entry_hash = excluded.entry_hash
         WHERE excluded.lamport > table_sync_chain_tips.lamport
            OR (excluded.lamport = table_sync_chain_tips.lamport
                AND excluded.entry_hash = table_sync_chain_tips.entry_hash)",
        params![
            stream_bytes.as_slice(),
            device_bytes.as_slice(),
            i64::try_from(verified.lamport)?,
            verified.entry_hash.as_slice(),
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

/// Record that an entry was rejected on its own merits — a type mismatch, a constraint violation —
/// and drop it from the replay worklist.
///
/// TERMINAL, unlike a pending mark: those are version gaps a later binary redeems, this is data
/// that does not fit the table and never will. Both facts have to be durable. Leaving it merely
/// unmarked would make a rejected payload indistinguishable from a fully projected one, so nothing
/// downstream could ever report it; leaving it PENDING would retry it on every future projector
/// bump forever. The entry itself stays stored — it still relays, and it is the evidence.
pub(crate) fn record_entry_quarantine(
    tx: &Transaction<'_>,
    entry_hash: &[u8; 32],
    reason: &str,
) -> anyhow::Result<()> {
    tx.execute(
        "UPDATE table_sync_entries
            SET pending_reason = NULL, pending_projector_version = NULL, quarantine_reason = ?2
          WHERE entry_hash = ?1",
        params![entry_hash.as_slice(), reason],
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

/// The op of the entry that currently WINS a row — located by the clock's `(device, lamport)`,
/// which `table_sync_entries` is UNIQUE on within a stream, so at most one entry can match.
///
/// The clock stores the fingerprint as lowercase hex while the entry log stores raw bytes; the
/// fingerprint type round-trips between them, and an unparseable value simply resolves to nothing.
/// Returns `None` when the entry is absent or its payload no longer decodes to a known op.
pub(crate) fn winning_entry_op(
    tx: &Transaction<'_>,
    stream: StreamId,
    device_hex: &str,
    lamport: u64,
) -> anyhow::Result<Option<RowOp>> {
    let Ok(device) = device_hex.parse::<DeviceFingerprint>() else {
        return Ok(None);
    };
    let signed_bytes: Option<Vec<u8>> = tx
        .query_row(
            "SELECT signed_bytes FROM table_sync_entries
              WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport = ?3",
            params![
                stream.to_bytes().as_slice(),
                device.to_bytes().as_slice(),
                i64::try_from(lamport)?
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(signed_bytes) = signed_bytes else {
        return Ok(None);
    };
    let Ok(signed) = entry::decode_signed(&signed_bytes) else {
        return Ok(None);
    };
    Ok(match row_op::decode(&signed.entry.op_bytes) {
        Ok(DecodedRowOp::Known(op)) => Some(op),
        _ => None,
    })
}

/// One retained-but-unprojected entry, with everything replay needs except its apply context (which
/// comes from [`stream_context`], since the stream id hashes that away).
pub(crate) struct PendingEntry {
    pub(crate) entry_hash: [u8; 32],
    pub(crate) stream_id: StreamId,
    pub(crate) signed_bytes: Vec<u8>,
    /// The mark this entry currently carries. `None` when the stored token is not in this binary's
    /// vocabulary — a NEWER binary parked it for a reason this one has no name for. Exact-token
    /// parsing keeps that legible instead of coercing it to a wrong reason; the replay itself does
    /// not consult it, so an unknown token costs nothing beyond one re-mark.
    pub(crate) reason: Option<PendingReason>,
    pub(crate) projector_version: Option<i64>,
}

/// Which retained entries a refold pass replays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Worklist {
    /// Everything outstanding — this binary may understand more than whatever last evaluated them.
    All,
    /// Only the entries blocked behind local row state ([`PendingReason::is_deferral`]).
    Deferrals,
}

/// The entries this binary has not fully projected, in stream/lamport order. Ordered for
/// determinism and reproducible diagnostics, NOT for correctness: each replay goes through the
/// unchanged LWW gates, which are arrival-order independent.
pub(crate) fn pending_entries(
    tx: &Transaction<'_>,
    worklist: Worklist,
) -> anyhow::Result<Vec<PendingEntry>> {
    let filter = match worklist {
        Worklist::All => "pending_reason IS NOT NULL".to_string(),
        Worklist::Deferrals => format!("pending_reason IN ({})", deferral_tokens_sql()),
    };
    let mut stmt = tx.prepare(&format!(
        "SELECT entry_hash, stream_id, signed_bytes, pending_reason, pending_projector_version
           FROM table_sync_entries
          WHERE {filter}
          ORDER BY stream_id, lamport, device_fingerprint"
    ))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(hash, stream, signed_bytes, reason, projector_version)| {
            Ok(PendingEntry {
                entry_hash: fixed32(hash)?,
                stream_id: StreamId::from_bytes(fixed32(stream)?),
                signed_bytes,
                reason: reason.as_deref().and_then(PendingReason::from_db_str),
                projector_version,
            })
        })
        .collect()
}

/// The deferral family's tokens as a SQL literal list, DERIVED from the enum rather than written
/// out: a new variant answers [`PendingReason::is_deferral`] and the query follows it, so the two
/// cannot drift apart. The tokens are `serialize_all = "snake_case"` renderings of a closed enum,
/// so there is no untrusted text here to quote against.
pub(crate) fn deferral_tokens_sql() -> String {
    <PendingReason as strum::IntoEnumIterator>::iter()
        .filter(|reason| reason.is_deferral())
        .map(|reason| format!("'{}'", reason.as_db_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The apply context a stored entry needs to be replayed. `scope_stream_id` is a ONE-WAY sha256 of
/// `(repo_id, account_id, incarnation_ref, scope_id)` and entries store only the stream id, so
/// without this directory a retained entry can never be re-applied: `repo_id` scopes every
/// projected write, `incarnation_ref` validates its authority generation, and `scope_id` resolves
/// the op's table spec.
pub(crate) struct StreamContext {
    pub(crate) repo_id: String,
    pub(crate) incarnation_ref: [u8; 32],
    pub(crate) scope_id: String,
}

pub(crate) fn assert_current_incarnation(
    tx: &Transaction<'_>,
    account_id: AccountId,
    repo_id: &str,
    incarnation_ref: [u8; 32],
) -> anyhow::Result<()> {
    match crate::account::repo_incarnation_state(tx, account_id, repo_id)? {
        crate::account::RepoIncarnationState::Current(current) if current == incarnation_ref =>
            Ok(()),
        crate::account::RepoIncarnationState::Current(_) => {
            anyhow::bail!("table-sync context names a stale repository incarnation")
        },
        crate::account::RepoIncarnationState::Absent => {
            anyhow::bail!("repository incarnation authority is absent")
        },
        crate::account::RepoIncarnationState::Contested => {
            anyhow::bail!("repository incarnation authority is contested")
        },
    }
}

/// Record a stream's apply context, idempotently. Called on every authored and ingested entry, so
/// the directory covers exactly the streams whose entries could ever need replaying.
pub(crate) fn record_stream_context(
    tx: &Transaction<'_>,
    stream: StreamId,
    repo_id: &str,
    account_id: AccountId,
    incarnation_ref: [u8; 32],
    scope_id: &str,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO table_sync_streams(
             stream_id, repo_id, account_id, incarnation_ref, scope_id
         ) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(stream_id) DO NOTHING",
        params![
            stream.to_bytes().as_slice(),
            repo_id,
            account_id.to_bytes().as_slice(),
            incarnation_ref.as_slice(),
            scope_id,
        ],
    )?;
    let recorded: (String, Vec<u8>, Vec<u8>, String) = tx.query_row(
        "SELECT repo_id, account_id, incarnation_ref, scope_id
           FROM table_sync_streams WHERE stream_id = ?1",
        [stream.to_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    anyhow::ensure!(
        recorded.0 == repo_id
            && recorded.1 == account_id.to_bytes()
            && recorded.2 == incarnation_ref
            && recorded.3 == scope_id,
        "table-sync stream context conflicts with its existing directory row",
    );
    Ok(())
}

pub(crate) fn stream_context(
    tx: &Transaction<'_>,
    stream: StreamId,
) -> anyhow::Result<Option<StreamContext>> {
    Ok(tx
        .query_row(
            "SELECT repo_id, incarnation_ref, scope_id FROM table_sync_streams WHERE stream_id = \
             ?1",
            params![stream.to_bytes().as_slice()],
            |row| {
                let incarnation: Vec<u8> = row.get(1)?;
                Ok(StreamContext {
                    repo_id: row.get(0)?,
                    incarnation_ref: incarnation.try_into().map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            1,
                            "incarnation_ref".into(),
                            rusqlite::types::Type::Blob,
                        )
                    })?,
                    scope_id: row.get(2)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn stream_account_id(
    tx: &Transaction<'_>,
    stream: StreamId,
) -> anyhow::Result<AccountId> {
    let bytes: Vec<u8> = tx.query_row(
        "SELECT account_id FROM table_sync_streams WHERE stream_id = ?1",
        [stream.to_bytes().as_slice()],
        |row| row.get(0),
    )?;
    Ok(AccountId::from_bytes(fixed32(bytes)?))
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
        RowOp::Remove {
            spec_version: 1,
            table: "t".to_string(),
            pk: vec![TypedValue::Text(id.to_string())],
        }
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
            prev_hash: None,
        });
    }

    #[test]
    fn retained_local_tip_blocks_second_genesis_until_the_tip_is_restored() {
        let mut c = conn();
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let tx = c.transaction().unwrap();
        let first = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
        tx.commit().unwrap();

        // Repository purge removes the accepted log but deliberately leaves the witness.
        c.execute("DELETE FROM table_sync_entries WHERE stream_id = ?1", [stream()
            .to_bytes()
            .as_slice()])
            .unwrap();
        let tx = c.transaction().unwrap();
        let error = author_row_entry(&tx, stream(), &secret, &op("r2"), 1).unwrap_err();
        assert!(error.to_string().contains("continuity is not restored"));
        tx.rollback().unwrap();

        // Re-delivering the exact retained tip restores continuity. The next local entry extends it
        // rather than emitting another genesis.
        let tx = c.transaction().unwrap();
        assert!(matches!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &first.signed_bytes,
                &secret.public(),
                2,
            )
            .unwrap(),
            AcceptOutcome::Stored { .. }
        ));
        let second = author_row_entry(&tx, stream(), &secret, &op("r2"), 3).unwrap();
        assert_eq!(second.entry.prev_hash, Some(first.entry.entry_hash));
        assert_eq!(second.entry.lamport, 1);
        tx.commit().unwrap();
    }

    #[test]
    fn retained_high_lamport_tip_allows_exact_and_direct_successor_restoration() {
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let high_lamport = MAX_LAMPORT_ADVANCE * 2;
        let tip = entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            None,
            high_lamport,
            row_op::encode(&op("tip")),
        );
        let successor = entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            Some(tip.entry.entry_hash),
            high_lamport + 1,
            row_op::encode(&op("successor")),
        );

        for candidate in [&tip, &successor] {
            let mut c = conn();
            c.execute(
                "INSERT INTO table_sync_chain_tips(
                     stream_id, device_fingerprint, lamport, entry_hash
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    stream().to_bytes().as_slice(),
                    secret.public().fingerprint().to_bytes().as_slice(),
                    i64::try_from(high_lamport).unwrap(),
                    tip.entry.entry_hash.as_slice(),
                ],
            )
            .unwrap();
            let tx = c.transaction().unwrap();
            assert!(matches!(
                accept_row_entry(
                    &tx,
                    account(),
                    stream(),
                    &["t"],
                    &candidate.signed_bytes,
                    &secret.public(),
                    0,
                )
                .unwrap(),
                AcceptOutcome::Stored { .. }
            ));
        }
    }

    #[test]
    fn stream_context_conflicts_fail_closed() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        record_stream_context(&tx, stream(), "repo", account(), [1; 32], "demo/1").unwrap();
        let error =
            record_stream_context(&tx, stream(), "repo", account(), [2; 32], "demo/1").unwrap_err();
        assert!(error.to_string().contains("conflicts"));
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
            AcceptOutcome::StoredInert {
                reason: PendingReason::TableNotInScope,
                entry_hash: first.entry.entry_hash,
                prev_hash: None,
            },
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
            AcceptOutcome::StoredInert {
                reason: PendingReason::UndecodablePayload,
                entry_hash: garbage.entry.entry_hash,
                prev_hash: None,
            },
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
    fn a_gap_is_retained_awaiting_its_predecessor() {
        // A second-position entry (lamport 1) arriving before the genesis has a missing
        // predecessor: it is held rather than dropped, so reverse delivery can still converge.
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
            AcceptOutcome::GapRetained,
        );
        let held: i64 = tx
            .query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            held, 1,
            "and it is HELD, not merely reported — the verdict alone is not the fix"
        );
        let accepted: i64 =
            tx.query_row("SELECT COUNT(*) FROM table_sync_entries", [], |r| r.get(0)).unwrap();
        assert_eq!(accepted, 0, "but it is not on the accepted chain");
    }

    /// Two entries citing the SAME predecessor, both arriving before it. Both are held — neither
    /// can be classified yet, because the predecessor that would make one of them a second
    /// successor is not here. The equivocation only becomes visible on promotion.
    #[test]
    fn two_siblings_awaiting_one_predecessor_are_both_held() {
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let (genesis, first) = {
            let mut a = conn();
            let tx = a.transaction().unwrap();
            let genesis = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
            let first = author_row_entry(&tx, stream(), &secret, &op("r2"), 0).unwrap();
            tx.commit().unwrap();
            (genesis, first)
        };
        let sibling = entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            Some(genesis.entry.entry_hash),
            first.entry.lamport + 1,
            row_op::encode(&op("r_sibling")),
        );

        let mut b = conn();
        let tx = b.transaction().unwrap();
        for bytes in [&first.signed_bytes, &sibling.signed_bytes] {
            assert_eq!(
                accept_row_entry(&tx, account(), stream(), &["t"], bytes, &secret.public(), 0)
                    .unwrap(),
                AcceptOutcome::GapRetained,
            );
        }
        let held: i64 = tx
            .query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(held, 2, "both siblings are held; neither can be judged without the parent");
    }

    /// The cap evicts the FURTHEST-AHEAD held entry, not the newcomer. Refusing the newcomer would
    /// let whoever filled the table first block the near-tail entry the chain actually needs.
    #[test]
    fn the_cap_evicts_the_furthest_ahead_held_entry() {
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let mut b = conn();
        let tx = b.transaction().unwrap();
        let stream_bytes = stream().to_bytes();
        let device_bytes = secret.public().fingerprint().to_bytes();
        // Fill the chain to the cap with synthetic held rows at high lamports.
        for i in 0..MAX_GAPPED_PER_CHAIN {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&(i as u64).to_be_bytes());
            tx.execute(
                "INSERT INTO table_sync_gapped_entries(entry_hash, stream_id, device_fingerprint, \
                 lamport, prev_hash, signed_bytes, gapped_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    hash.as_slice(),
                    stream_bytes.as_slice(),
                    device_bytes.as_slice(),
                    i64::try_from(1_000_000 + i).unwrap(),
                    [9u8; 32].as_slice(),
                    [0u8; 4].as_slice(),
                ],
            )
            .unwrap();
        }
        let highest = i64::try_from(1_000_000 + MAX_GAPPED_PER_CHAIN - 1).unwrap();

        // A near-tail entry arrives with the table full.
        let newcomer = entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            Some([7u8; 32]),
            5,
            row_op::encode(&op("r_new")),
        );
        assert_eq!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &newcomer.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::GapRetained,
            "the newcomer is held, not refused",
        );
        let still_capped: i64 = tx
            .query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(usize::try_from(still_capped).unwrap(), MAX_GAPPED_PER_CHAIN, "the cap holds");
        let evicted: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM table_sync_gapped_entries WHERE lamport = ?1",
                params![highest],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(evicted, 0, "the furthest-ahead entry made room, not the arriving one");
        let kept: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM table_sync_gapped_entries WHERE entry_hash = ?1",
                params![newcomer.entry.entry_hash.as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "and the near-tail newcomer is the one held");

        // The OTHER direction: an arrival further ahead than everything held is itself the entry
        // the policy drops. Evicting the stored maximum for it would invert the policy — a table of
        // near-tail entries would be hollowed out by a stream of ever-higher-lamport arrivals.
        let far = entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            Some([7u8; 32]),
            9_000_000,
            row_op::encode(&op("r_far")),
        );
        assert_eq!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &far.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::GapChainFull,
            "the furthest-ahead arrival is refused, and says so rather than reporting itself held",
        );
        let far_held: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM table_sync_gapped_entries WHERE entry_hash = ?1",
                params![far.entry.entry_hash.as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(far_held, 0, "it is not held");
        let survivors: i64 = tx
            .query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            usize::try_from(survivors).unwrap(),
            MAX_GAPPED_PER_CHAIN,
            "and it displaced nothing",
        );
    }

    /// At the cap boundary the arrival's lamport can TIE the furthest-ahead held entry. Ranking on
    /// lamport alone resolves that as "whoever is already held wins" — insertion order again, the
    /// property the hash tie-break exists to remove. The comparison therefore uses the same
    /// `(lamport, entry_hash)` order the eviction victim is chosen by, so two ties on opposite
    /// sides of the held entry get opposite answers.
    #[test]
    fn a_lamport_tie_at_the_cap_boundary_is_broken_by_hash() {
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let mut b = conn();
        let tx = b.transaction().unwrap();
        let stream_bytes = stream().to_bytes();
        let device_bytes = secret.public().fingerprint().to_bytes();
        // Fill to the cap. Every row sits at the SAME lamport with a mid-range hash, so a real
        // signed entry at that lamport can sort on either side of the furthest-ahead one.
        const BOUNDARY: i64 = 500;
        for i in 0..MAX_GAPPED_PER_CHAIN {
            let mut hash = [0x80u8; 32];
            hash[8..16].copy_from_slice(&(i as u64).to_be_bytes());
            tx.execute(
                "INSERT INTO table_sync_gapped_entries(entry_hash, stream_id, device_fingerprint, \
                 lamport, prev_hash, signed_bytes, gapped_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    hash.as_slice(),
                    stream_bytes.as_slice(),
                    device_bytes.as_slice(),
                    BOUNDARY,
                    [9u8; 32].as_slice(),
                    [0u8; 4].as_slice(),
                ],
            )
            .unwrap();
        }
        let furthest: Vec<u8> = tx
            .query_row(
                "SELECT entry_hash FROM table_sync_gapped_entries
                  WHERE stream_id = ?1 AND lamport = ?2 ORDER BY entry_hash DESC LIMIT 1",
                params![stream_bytes.as_slice(), BOUNDARY],
                |r| r.get(0),
            )
            .unwrap();

        let (mut lower, mut higher) = (None, None);
        for nonce in 0u32..256 {
            let tie = entry::sign_entry_from_op_bytes(
                &secret,
                stream(),
                Some([7u8; 32]),
                u64::try_from(BOUNDARY).unwrap(),
                row_op::encode(&op(&format!("tie{nonce}"))),
            );
            if tie.entry.entry_hash.as_slice() < furthest.as_slice() {
                lower.get_or_insert(tie);
            } else {
                higher.get_or_insert(tie);
            }
            if lower.is_some() && higher.is_some() {
                break;
            }
        }
        let (lower, higher) =
            (lower.expect("a lower-hash tie"), higher.expect("a higher-hash tie"));

        assert_eq!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &higher.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::GapChainFull,
            "a tie sorting ABOVE the furthest held entry is the one dropped",
        );
        assert_eq!(
            accept_row_entry(
                &tx,
                account(),
                stream(),
                &["t"],
                &lower.signed_bytes,
                &secret.public(),
                0
            )
            .unwrap(),
            AcceptOutcome::GapRetained,
            "a tie sorting BELOW it displaces it instead",
        );
    }

    /// Which of two same-lamport siblings takes the successor slot must be a property of the
    /// ENTRIES, not of the order they happened to be inserted in. On lamport alone, SQLite falls
    /// back to physical row order, so two replicas holding the same pair could accept different
    /// successors and project different rows.
    #[test]
    fn the_sibling_taken_first_does_not_depend_on_insertion_order() {
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let genesis = {
            let mut a = conn();
            let tx = a.transaction().unwrap();
            let g = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
            tx.commit().unwrap();
            g
        };
        let sib = |id: &str| {
            entry::sign_entry_from_op_bytes(
                &secret,
                stream(),
                Some(genesis.entry.entry_hash),
                7,
                row_op::encode(&op(id)),
            )
        };
        let (one, two) = (sib("r_one"), sib("r_two"));

        // Two stores, same pair of siblings, opposite insertion orders.
        let taken_by = |order: [&crate::entry::SignedEntry; 2]| {
            let mut b = conn();
            let tx = b.transaction().unwrap();
            for e in order {
                accept_row_entry(
                    &tx,
                    account(),
                    stream(),
                    &["t"],
                    &e.signed_bytes,
                    &secret.public(),
                    0,
                )
                .unwrap();
            }
            take_gapped_child(
                &tx,
                stream(),
                secret.public().fingerprint(),
                &genesis.entry.entry_hash,
            )
            .unwrap()
            .expect("a sibling is available")
            .entry_hash
        };

        assert_eq!(
            taken_by([&one, &two]),
            taken_by([&two, &one]),
            "the same pair yields the same winner whichever arrived first",
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
