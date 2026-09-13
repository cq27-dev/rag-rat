//! The engine's orchestration surface: turn local table changes into authored entries, and fold
//! received entries back into tables.
//!
//! [`produce_and_author`] scans every registered table, authors an entry per changed row on that
//! table's scope stream, and self-applies it so the LWW clock and published-row record capture this
//! authorship (a later remote op competes at the authored lamport; the next producer pass sees no
//! delta). [`ingest`] verifies, stores, and applies one received entry, routing it to the right
//! table within its scope.
//!
//! This is the transport-independent seam the milestone's loopback test drives; the iroh milestone
//! wraps a per-scope `SyncStore` around exactly these two calls.

use rusqlite::Transaction;

use super::apply::{self, ApplyOutcome};
use super::registry::TableSpec;
use super::row_op::{self, RowOp};
use super::scope_stream::scope_stream_id;
use super::store::{self, AcceptOutcome};
use super::{produce, refold};
use crate::device::DevicePublic;
use crate::op::OpMeta;
use crate::stream::EntryHash;
use crate::{AccountId, LocalDevice};

/// The stable dependencies of a sync pass: the project being synced, the owning account (which the
/// scope stream ids derive from), this device, the syncable-table registry, and the injected clock.
pub(crate) struct SyncCtx<'a> {
    pub repo_id: &'a str,
    pub account_id: AccountId,
    pub incarnation_ref: [u8; 32],
    pub device: &'a LocalDevice,
    pub registry: &'a [TableSpec],
    pub now_ms: i64,
}

/// What ingesting one received entry did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IngestOutcome {
    Applied,
    /// Stored and relayed, but not applied — an undecodable/unknown/out-of-scope payload. The
    /// `&str` is the reason. Forward-compatible: the chain advanced, the payload is retained.
    Retained(&'static str),
    AlreadyPresent,
    /// The entry's chain predecessor has not arrived, so it is RETAINED and will be promoted when
    /// the predecessor is accepted. Not an error: out-of-order delivery is the normal condition on
    /// a transport.
    AwaitingPredecessor,
    /// Already retained awaiting its predecessor. Weaker than [`Self::AlreadyPresent`] — a retained
    /// entry can still be evicted by the per-chain cap, so a frontier must not treat it as settled.
    AlreadyAwaiting,
    /// The entry's predecessor is missing AND it is further ahead than everything the chain already
    /// holds, so the per-chain cap dropped it. NOT held — nothing will promote it. RETRYABLE once
    /// the entries between it and the tail have arrived.
    HeldChainFull,
    /// The entry conflicts with the stored chain — an equivocation. Fork EVIDENCE (proving it to a
    /// peer) is the transport milestone's.
    Forked,
    /// A held entry discarded because the entry it cites was judged a fork. It was never itself
    /// classified: nothing will ever place its predecessor on the chain, so it could not have been
    /// promoted, and it cites a hash no future acceptance produces, so nothing would look at it
    /// again.
    AbandonedBehindFork,
    /// A type mismatch — stored, unprojectable, surfaced.
    Quarantined(String),
    /// The signing device is not a roster-effective writer (off-roster, removed, or read-only), so
    /// the entry was DROPPED (#935). RETRYABLE — the local fold may lag the author's `DeviceAdd`; a
    /// caller must not treat it as peer misbehavior, and the frontier re-offers it once the account
    /// log delivers the enrollment.
    Unauthorized,
}

/// Author the row ops that bring peers up to this device's state, returning each as signed wire
/// bytes. Empty when everything is already published.
pub(crate) fn produce_and_author(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
) -> anyhow::Result<Vec<Vec<u8>>> {
    store::assert_current_incarnation(tx, ctx.account_id, ctx.repo_id, ctx.incarnation_ref)?;
    // Never author into a store a NEWER projector folded: our narrower column set would record
    // anti-echo hashes and park decisions the newer binary has to distrust.
    refold::assert_projector_not_newer(tx)?;
    let mut authored = Vec::new();
    for spec in ctx.registry {
        let stream =
            scope_stream_id(ctx.repo_id, ctx.account_id, ctx.incarnation_ref, spec.scope_id);
        // Record the apply context for every stream we author on: the stream id hashes
        // (repo_id, account_id, incarnation_ref, scope_id) one-way, so without the directory a
        // retained entry could never be replayed by a later binary (see [`super::refold`]).
        store::record_stream_context(
            tx,
            stream,
            ctx.repo_id,
            ctx.account_id,
            ctx.incarnation_ref,
            spec.scope_id,
        )?;
        for op in produce::produce_row_ops(tx, spec, ctx.repo_id, stream)? {
            let signed = store::author_row_entry(tx, stream, ctx.device.secret(), &op, ctx.now_ms)?;
            let meta =
                OpMeta { lamport: signed.entry.lamport, device: signed.entry.device_fingerprint };
            // Self-apply so this authorship enters the LWW clock and published-row record: a later
            // remote op competes at this lamport, and the producer never re-emits it (no
            // self-echo). A locally-produced op MUST self-apply cleanly — the registry lint rejects
            // every shape that would quarantine (nullable pk, cross-row constraint, and the
            // producer reads well-typed values). If it quarantines anyway, the
            // published hash is NOT recorded, so the next pass would re-author the same
            // row forever (unbounded signed-log growth) and peers would quarantine each
            // copy: surface it and do NOT transmit the junk op.
            match apply::apply_row_op_on_stream(tx, spec, ctx.repo_id, stream, &op, meta)? {
                apply::ApplyOutcome::Applied => authored.push(signed.signed_bytes),
                // A locally-authored op CANNOT lose its own self-apply while the row's bookkeeping
                // belongs to this stream: the entry took `MAX(lamport) + 1` over the whole stream,
                // so it outranks every clock any op on it could have set. Losing therefore proves
                // the row's clock came from a DIFFERENT stream — the shape a changed scope or
                // account leaves behind, where `sync_row_clocks` (keyed only by
                // `(repo_id, table_name, row_pk)`) survives a move its lamports have no meaning
                // after.
                //
                // FAIL rather than accept it. A superseded self-apply writes no published record,
                // so the next pass re-derives the identical delta and signs it again — growing the
                // log without bound and broadcasting entries every peer will also discard, until
                // the new stream's lamport happens to climb past the stale clock. Bailing rolls
                // back the entry `author_row_entry` just inserted, exactly as the quarantine arm
                // below does, and leaves an attributable error instead of silent churn.
                // No `debug_assert` here, unlike the arm below: that one is provably unreachable
                // (the lint rejects every shape that quarantines), so asserting is a dev-time
                // tripwire for a lint gap. This one is REACHABLE whenever a row's bookkeeping and
                // its stream come apart, which is precisely the condition worth reporting — a
                // panic would replace a diagnosable error with a crash.
                apply::ApplyOutcome::Superseded => {
                    anyhow::bail!(
                        "table-sync: a locally-produced op lost its own self-apply on `{}` — the \
                         row's write clock carries a lamport from another stream, so authoring \
                         cannot settle it",
                        spec.name
                    );
                },
                // A locally-produced op failing to self-apply is UNREACHABLE for a registered
                // table: the lint rejects every shape that would quarantine (nullable pk, cross-row
                // constraint, ValueType/physical-type mismatch), the producer reads well-typed
                // values, and it emits only columns from THIS registry so it can never carry one we
                // do not know. If it somehow happens, FAIL the pass — `author_row_entry` has
                // already inserted this entry in the caller's txn, so bailing rolls
                // it back rather than leaving it stored-but-unpublished and
                // re-authored (re-signed) every pass (unbounded log growth). Assert
                // first, so a test catches the lint/producer gap.
                outcome @ (apply::ApplyOutcome::Quarantined(_)
                | apply::ApplyOutcome::Unprojectable(_)) => {
                    debug_assert!(
                        false,
                        "table-sync: a locally-produced op did not self-apply on `{}`: {outcome:?}",
                        spec.name
                    );
                    anyhow::bail!(
                        "table-sync: a locally-produced op did not self-apply on `{}`: {outcome:?}",
                        spec.name
                    );
                },
            }
        }
    }
    // Local work is now published, so anything ingest deferred behind it is unblocked — replay it
    // here rather than leaving it for the next store open. This is where author-before-apply
    // actually holds: a remote op deferred to protect a local edit gets its rematch immediately
    // after that edit is authored, and loses on the merits instead of by default.
    refold::replay_deferred_entries(tx, ctx.registry)?;
    Ok(authored)
}

/// Re-author the removed writer's surviving state on this stream under the local device key, then
/// mark the removal handled (#997). Runs inside the producer transaction: it sees the just-authored
/// local rows and is protected by the same current-incarnation and projector gates as authoring.
///
/// Returns `None` when the removal CANNOT be fully drained here — the local device is not
/// currently a roster-effective writer, the stream has no recorded apply context, or a row exists
/// that cannot be carried in an op today (an unreadable synced column: retried after the cell is
/// repaired, never written off). `Some(n)` means the work item completed (n re-authored rows,
/// possibly 0): the caller must not treat "could not drain" and "nothing to re-author" as the
/// same outcome.
pub(crate) fn process_readoption_work_for_stream(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
    stream: crate::stream::StreamId,
) -> anyhow::Result<Option<usize>> {
    let Some(work) = store::readoption_work_for_stream(tx, ctx.account_id, stream)? else {
        return Ok(Some(0));
    };
    if !crate::account::device_is_effective_writer(tx, ctx.account_id, ctx.device.fingerprint())? {
        return Ok(None);
    }
    // Re-verify the removal still stands: a device re-invited before this pass is effective again,
    // so its entries ingest directly on every replica and re-authoring them here would only steal
    // clock ownership from an active writer. Completing (not deleting) keeps the row idempotent.
    if crate::account::device_is_effective_writer(tx, ctx.account_id, work.device_fingerprint)? {
        store::complete_readoption_work(
            tx,
            ctx.account_id,
            work.device_fingerprint,
            stream,
            ctx.now_ms,
        )?;
        return Ok(Some(0));
    }
    let Some(context) = store::stream_context(tx, stream)? else {
        return Ok(None);
    };
    let removed = work.device_fingerprint;
    let removed_hex = removed.to_string();
    let mut authored = 0;
    let mut unrepairable = 0;
    for candidate in store::readoption_candidates(tx, stream, work.device_fingerprint)? {
        let Some(spec) = ctx
            .registry
            .iter()
            .find(|spec| spec.scope_id == context.scope_id && spec.name == candidate.table_name)
        else {
            continue;
        };
        let op =
            match row_repair_op(tx, ctx.repo_id, spec, stream, &candidate.row_pk, &removed_hex)? {
                RowRepair::Skip => continue,
                RowRepair::Unrepairable => {
                    unrepairable += 1;
                    continue;
                },
                RowRepair::Repair(op) => op,
            };
        let Some(adopted_entry_hash) = author_repair(tx, ctx, spec, stream, &op, "re-adoption")?
        else {
            unrepairable += 1;
            continue;
        };
        store::record_readoption_audit(tx, store::ReadoptionAudit {
            account_id: ctx.account_id,
            removed,
            adopter: ctx.device.fingerprint(),
            stream,
            repo_id: context.repo_id.clone(),
            scope_id: context.scope_id.clone(),
            table_name: spec.name.to_string(),
            row_pk: candidate.row_pk.clone(),
            original_lamport: candidate.original_lamport,
            original_entry_hash: candidate.entry_hash,
            adopted_entry_hash,
            adopted_at_ms: ctx.now_ms,
        })?;
        authored += 1;
    }
    if unrepairable > 0 {
        // A row the pass cannot carry today is NOT written off: completing here would abandon it
        // to anti-echo silence (a later content-identical repair authors nothing, so a fresh
        // replica never receives the row — the divergence this pass exists to fix). Report
        // "could not drain" and leave the item pending; a pass after the cell is repaired
        // completes it. Already re-authored rows above stay committed.
        return Ok(None);
    }
    store::complete_readoption_work(
        tx,
        ctx.account_id,
        work.device_fingerprint,
        stream,
        ctx.now_ms,
    )?;
    Ok(Some(authored))
}

/// Re-author the oldest `pins` of this device's own chain at its tail, in order, so compaction can
/// drop the entries that carried them (#1277). Stops at the first pin that cannot be carried today
/// and returns how many leading pins moved; the rest still pin. A pin cannot be carried while its
/// physical row disagrees with its merge state (the producer owes that row), a synced column is
/// unreadable, an accepted entry this binary cannot apply yet sits above it, or its re-signed
/// entry would not fit the transport limit.
///
/// A re-authored pin is a NEW write of the row's current cells. It competes under LWW with any
/// concurrent edit to the same row this device has not received yet, and can win it — the same
/// cost re-adoption accepts when it re-authors a removed writer's rows.
pub(crate) fn reauthor_chain_pins(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
    scope_id: &str,
    stream: crate::stream::StreamId,
    pins: &[super::retention::Pin],
) -> anyhow::Result<usize> {
    if pins.is_empty() {
        return Ok(0);
    }
    // Never author into a store a NEWER projector folded (the producer's gate).
    refold::assert_projector_not_newer(tx)?;
    let device_hex = ctx.device.fingerprint().to_string();
    for (moved, pin) in pins.iter().enumerate() {
        let Some(spec) = ctx
            .registry
            .iter()
            .find(|spec| spec.scope_id == scope_id && spec.name == pin.table_name)
        else {
            return Ok(moved);
        };
        let RowRepair::Repair(op) =
            row_repair_op(tx, ctx.repo_id, spec, stream, &pin.row_pk, &device_hex)?
        else {
            return Ok(moved);
        };
        if author_repair(tx, ctx, spec, stream, &op, "compaction")?.is_none() {
            return Ok(moved);
        }
    }
    Ok(pins.len())
}

/// Sign `op` under the local key and self-apply it, returning the new entry's hash — `None`, with
/// nothing stored, when the re-signed entry would not fit the transport limit (the row stays
/// where it is). A repair that does not self-apply cleanly is a failure, never a settlement: the
/// caller rolls back and the just-inserted entry goes with it.
fn author_repair(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
    spec: &TableSpec,
    stream: crate::stream::StreamId,
    op: &RowOp,
    what: &str,
) -> anyhow::Result<Option<EntryHash>> {
    let Some(signed) =
        store::author_row_entry_if_it_fits(tx, stream, ctx.device.secret(), op, ctx.now_ms)?
    else {
        return Ok(None);
    };
    let meta = OpMeta { lamport: signed.entry.lamport, device: signed.entry.device_fingerprint };
    match apply::apply_row_op_on_stream(tx, spec, ctx.repo_id, stream, op, meta)? {
        ApplyOutcome::Applied => Ok(Some(signed.entry.entry_hash)),
        // Same diagnosis as the produce path: a locally-authored op takes the stream's
        // MAX(lamport)+1, so losing its own self-apply means the row's clock carries a lamport
        // from ANOTHER stream — the shape a scope or account move leaves behind. Swallowing it
        // would record the repair done with the row unrepaired and no retry left.
        ApplyOutcome::Superseded => anyhow::bail!(
            "table-sync: a {what} op lost its own self-apply on `{}` — the row's write clock \
             carries a lamport from another stream",
            spec.name
        ),
        outcome @ (ApplyOutcome::Quarantined(_) | ApplyOutcome::Unprojectable(_)) => {
            anyhow::bail!(
                "table-sync: a {what} op did not self-apply on `{}`: {outcome:?}",
                spec.name
            )
        },
    }
}

/// What a repair pass can do with one row it wants to carry under the local key.
enum RowRepair {
    /// Re-author this op under the local key.
    Repair(RowOp),
    /// Nothing to carry: the row is settled under another writer, or physically inconsistent with
    /// its merge state (an absent row under a live clock is the producer's `Remove` to author).
    Skip,
    /// The row cannot be carried today: a synced column cannot be read as its declared type, or an
    /// accepted entry this binary cannot apply yet sits above the winner. Retried later, never
    /// written off.
    Unrepairable,
}

/// The physical-table repair for one row while `winner_hex` still owns its merge state.
fn row_repair_op(
    tx: &Transaction<'_>,
    repo_id: &str,
    spec: &TableSpec,
    stream: crate::stream::StreamId,
    row_pk: &str,
    winner_hex: &str,
) -> anyhow::Result<RowRepair> {
    let key = apply::RowKey { stream, repo_id, table: spec.name, row_pk };
    let clock = apply::row_clock_winner_on_stream(tx, &key)?;
    let tombstone = apply::tombstone_winner_on_stream(tx, &key)?;
    // A live clock and a tombstone can only coexist with the clock newer: a remove raises the
    // tombstone at its own lamport, and a remove that BEATS the clock clears the clock. So a live
    // clock always owns the row, and a tombstone owns the deletion only without one.
    let (winner_lamport, live) = match (clock, tombstone) {
        (Some((lamport, winner)), _) if winner == winner_hex => (lamport, true),
        (None, Some((lamport, winner))) if winner == winner_hex => (lamport, false),
        _ => return Ok(RowRepair::Skip),
    };
    // What the PHYSICAL row allows decides the repair. A live winner is carried while its row is
    // (an absent one is the producer's Remove to author); a deletion is carried only while no row
    // exists, or a live local row would be destroyed by a stale repair.
    let pk = row_op::row_pk_values(row_pk)?;
    let repair = match (live, apply::read_synced_cells(tx, spec, &pk)?) {
        (true, apply::SyncedRow::Cells(cells)) => RowRepair::Repair(RowOp::Upsert {
            table: spec.name.to_string(),
            spec_version: spec.spec_version,
            pk,
            cells,
        }),
        (false, apply::SyncedRow::Absent) =>
            RowRepair::Repair(apply::readopt_remove(spec, row_pk)?),
        (_, apply::SyncedRow::Unreadable(_)) => RowRepair::Unrepairable,
        _ => RowRepair::Skip,
    };
    // A repair is signed at the stream tail, above every accepted entry. One retained for replay
    // above the winner (a newer spec version during a rolling upgrade) may be a newer write to
    // this very row: peers that understand it have applied it, and the repair would beat it there
    // with the stale cells. Hold the repair until that entry replays.
    if matches!(repair, RowRepair::Repair(_))
        && store::pending_entry_at_or_above(tx, stream, winner_lamport)?
    {
        return Ok(RowRepair::Unrepairable);
    }
    Ok(repair)
}

/// **The caller MUST roll back on `Err`.** Everything here runs in the caller's transaction and
/// nothing is safe to commit after a failure: the entry may be stored with its payload neither
/// applied nor marked, and a promotion in flight takes an entry out of the held table before
/// re-ingesting it, so committing past an error can lose it. That contract predates promotion — an
/// `accept` followed by a failing apply had it already — and promotion widens what is lost rather
/// than changing the rule. No `SAVEPOINT` is taken: it would make this one function's atomicity
/// self-contained while every sibling in the module still depends on caller rollback, which is a
/// worse thing to reason about than one uniform rule.
///
/// Verify, store, and apply one received entry for `scope_id`'s stream, signed by `pubkey`. A scope
/// may carry several tables (overlay, distill), so the target spec is resolved from the decoded
/// op's table against every registry entry in the scope — not fixed by the caller. The op's table
/// is validated against the scope's table set BEFORE the entry is stored, so a misrouted op never
/// advances the chain and orphans itself.
pub(crate) fn ingest(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
    scope_id: &str,
    signed_bytes: &[u8],
    pubkey: &DevicePublic,
    advertised_floor: Option<store::AdvertisedFloor>,
) -> anyhow::Result<IngestReport> {
    store::assert_current_incarnation(tx, ctx.account_id, ctx.repo_id, ctx.incarnation_ref)?;
    let device = pubkey.fingerprint();
    let stream = scope_stream_id(ctx.repo_id, ctx.account_id, ctx.incarnation_ref, scope_id);
    let scope = IngestScope { ctx, scope_id, pubkey };
    let (outcome, mut tail) = ingest_one(tx, &scope, signed_bytes, advertised_floor)?;
    let mut promoted = Vec::new();
    // Each accepted entry settles the held CHILDREN of two hashes, and both sets must be drained
    // or rows sit in the table forever, keyed to a hash no probe will ever revisit:
    //
    //  1. the children of its PREDECESSOR — its own siblings. The acceptance just filled that
    //     successor slot, so each is now provably an equivocation.
    //  2. the children of the accepted entry ITSELF — the chain advance.
    //
    // Both are the same operation, so both go through `drain_children`: take every held child of a
    // hash, re-ingest each, and report which one (if any) took the slot. Draining must CONTINUE
    // past a child that fails to store — a rejected child leaves the slot open, and stopping there
    // would strand a valid successor queued behind it and halt the chain.
    //
    // Iterative, not recursive: a long chain delivered in reverse must heal without growing the
    // stack in proportion to its length.
    while let Some(accepted) = tail {
        if let Some(prev) = accepted.prev_hash {
            // Cannot advance the chain: the slot is already held by `accepted`.
            drain_children(tx, &scope, stream, device, &prev, &mut promoted)?;
        }
        // A held entry on ANOTHER device's chain citing this hash is structurally impossible (a
        // chain links only within its own device), and accepting this entry is the only moment that
        // is decidable — `classify` keys the tail on the citing device's chain, so it would report
        // `Gap` forever. Sweep those now or they hold that chain's capacity until eviction.
        let foreign =
            store::discard_foreign_chain_citations(tx, stream, device, &accepted.entry_hash)?;
        promoted.extend(std::iter::repeat_n(IngestOutcome::AbandonedBehindFork, foreign));
        tail = drain_children(tx, &scope, stream, device, &accepted.entry_hash, &mut promoted)?;
    }
    Ok(IngestReport { outcome, promoted })
}

/// What one entry's arrival did, plus everything its acceptance unblocked.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IngestReport {
    pub outcome: IngestOutcome,
    /// Outcomes of the entries promoted out of the gapped table by this arrival, in promotion
    /// order. Reported individually rather than counted: promotion settles the CHAIN question, so
    /// a promoted entry can still be retained, quarantined, or deferred on its PAYLOAD.
    pub promoted: Vec<IngestOutcome>,
}

/// What every entry of one [`ingest`] call — the received entry and each held child it promotes —
/// is ingested under: the sync context, the scope whose stream it rides, and the signer's key.
#[derive(Clone, Copy)]
struct IngestScope<'a> {
    ctx: &'a SyncCtx<'a>,
    scope_id: &'a str,
    pubkey: &'a DevicePublic,
}

/// The entry an acceptance put at the chain tail — what a promotion probe keys on.
#[derive(Debug, Clone, Copy)]
struct AcceptedEntry {
    entry_hash: EntryHash,
    prev_hash: Option<EntryHash>,
}

/// Re-ingest every held child of `parent_hash`, returning the one that took the successor slot.
///
/// At most one can: once a child is stored, the rest classify as equivocations. But the loop must
/// run to exhaustion either way. Stopping at the first child that fails to store would leave a
/// VALID successor queued behind an invalid one — the invalid child sorts first (a lamport at or
/// below the tail is a `Conflict`, and it is a lower lamport), takes the slot's only probe, and the
/// legitimate entry behind it is never examined again. The chain would stop advancing there.
///
/// A child that does not store is a fork, and its own held descendants are abandoned with it:
/// nothing will ever put its hash on the chain, so they can never promote, and they cite a hash no
/// future acceptance produces, so no probe would reach them either.
fn drain_children(
    tx: &Transaction<'_>,
    scope: &IngestScope<'_>,
    stream: crate::stream::StreamId,
    device: crate::op::DeviceFingerprint,
    parent_hash: &EntryHash,
    promoted: &mut Vec<IngestOutcome>,
) -> anyhow::Result<Option<AcceptedEntry>> {
    let mut took_the_slot = None;
    while let Some(child) = store::take_gapped_child(tx, stream, device, parent_hash)? {
        // The advertised floor is deliberately NOT passed here: a promoted child is chain
        // continuation, not a candidate for adoption, and passing the floor would let an exact
        // match re-classify as RootAdopt where a promotion belongs.
        let (outcome, stored) = ingest_one(tx, scope, &child.signed_bytes, None)?;
        promoted.push(outcome);
        match stored {
            Some(entry) => took_the_slot = Some(entry),
            None => {
                let abandoned = store::discard_gapped_descendants(tx, stream, &child.entry_hash)?;
                promoted.extend(std::iter::repeat_n(IngestOutcome::AbandonedBehindFork, abandoned));
            },
        }
    }
    Ok(took_the_slot)
}

/// Ingest exactly one entry, with no promotion. Returns the accepted entry when this call STORED
/// one, which is what drives [`ingest`]'s loop.
///
/// The tail is returned explicitly rather than inferred from the outcome variant. Several outcomes
/// imply storage (`Applied`, `Retained`, `Quarantined`), and a future one that also stores must not
/// silently fail to drive promotion.
fn ingest_one(
    tx: &Transaction<'_>,
    scope: &IngestScope<'_>,
    signed_bytes: &[u8],
    advertised_floor: Option<store::AdvertisedFloor>,
) -> anyhow::Result<(IngestOutcome, Option<AcceptedEntry>)> {
    let IngestScope { ctx, scope_id, pubkey } = *scope;
    // Same refusal as the producer: an older binary must not re-park, under its own version, an
    // entry a newer projector already understood and folded.
    refold::assert_projector_not_newer(tx)?;
    let stream = scope_stream_id(ctx.repo_id, ctx.account_id, ctx.incarnation_ref, scope_id);
    // The apply context for anything this stream retains — recorded before the entry is stored, so
    // a pending entry is never left without the mapping its replay needs.
    store::record_stream_context(
        tx,
        stream,
        ctx.repo_id,
        ctx.account_id,
        ctx.incarnation_ref,
        scope_id,
    )?;
    let scope_tables: Vec<&str> =
        ctx.registry.iter().filter(|s| s.scope_id == scope_id).map(|s| s.name).collect();
    Ok(
        match store::accept_row_entry(
            tx,
            &store::AcceptCtx {
                account_id: ctx.account_id,
                expected_stream: stream,
                expected_tables: &scope_tables,
                pubkey,
                now_ms: ctx.now_ms,
            },
            signed_bytes,
            advertised_floor,
        )? {
            AcceptOutcome::Stored { op, meta, entry_hash, prev_hash } => {
                let accepted = Some(AcceptedEntry { entry_hash, prev_hash });
                // `accept_row_entry` already validated the op's table is in `scope_tables`, so
                // exactly one spec matches; the fallback is defensive, never
                // reached.
                let Some(spec) =
                    ctx.registry.iter().find(|s| s.scope_id == scope_id && s.name == op.table())
                else {
                    return Ok((IngestOutcome::Retained("table not in scope"), accepted));
                };
                // NEVER apply over unsent local work. A raw local write does not advance the row
                // clock, so the LWW comparison below cannot see it: this op would simply win and
                // record its OWN hash as published, after which the producer sees no delta and the
                // local change is gone with nothing left to author it from.
                //
                // Deferring is safe to do HERE, which it was not before #1005: a deferral now
                // carries its own refold trigger, so the entry is retried once the local work is
                // authored rather than being parked and forgotten. The chain still advances — the
                // entry is stored either way — and convergence stays on the merits, because the
                // local edit is authored at `MAX(lamport) + 1` counting this parked entry and so
                // wins the comparison this op would otherwise have won by default.
                //
                // `DeferExceptUnprovableRemoval`, unlike the refold's blanket caution: a deletion
                // must not be held back on a verdict that may never resolve, or a row deleted after
                // a column change becomes undeletable across the skew. That exemption is for
                // `Remove` only — an unprovable verdict against an `Upsert` still defers, because
                // applying it would destroy the very edit this guard exists to protect.
                if let apply::PreApply::Park(deferral) = apply::pre_apply(
                    tx,
                    spec,
                    ctx.repo_id,
                    stream,
                    &op,
                    apply::RowDoubt::DeferExceptUnprovableRemoval,
                )? {
                    store::mark_entry_pending(
                        tx,
                        &entry_hash,
                        deferral,
                        refold::TABLE_SYNC_PROJECTOR_VERSION,
                    )?;
                    return Ok((IngestOutcome::Retained(deferral.as_db_str()), accepted));
                }
                let outcome = match apply::apply_row_op_on_stream(
                    tx,
                    spec,
                    ctx.repo_id,
                    stream,
                    &op,
                    meta,
                )? {
                    // A received op that lost on the merits still landed: the entry is
                    // stored, nothing is outstanding, and redelivery stays idempotent.
                    ApplyOutcome::Applied | ApplyOutcome::Superseded => IngestOutcome::Applied,
                    // Durably recorded as well as returned: the caller sees this one, but nothing
                    // later could tell a rejected payload from a projected one without the mark.
                    ApplyOutcome::Quarantined(why) => {
                        store::record_entry_quarantine(tx, &entry_hash, &why)?;
                        IngestOutcome::Quarantined(why)
                    },
                    // A newer producer's column: nothing was written. Mark the stored entry so the
                    // refold replays it once this binary learns the column — only the applier can
                    // see this, which is why `accept_row_entry` handed back the entry hash.
                    ApplyOutcome::Unprojectable(reason) => {
                        store::mark_entry_pending(
                            tx,
                            &entry_hash,
                            reason,
                            refold::TABLE_SYNC_PROJECTOR_VERSION,
                        )?;
                        IngestOutcome::Retained(reason.as_db_str())
                    },
                };
                (outcome, accepted)
            },
            AcceptOutcome::StoredInert { reason, entry_hash, prev_hash } => (
                IngestOutcome::Retained(reason.as_db_str()),
                Some(AcceptedEntry { entry_hash, prev_hash }),
            ),
            // Nothing was stored by these, so none of them advances a chain and none can unblock a
            // retained successor.
            AcceptOutcome::AlreadyPresent => (IngestOutcome::AlreadyPresent, None),
            AcceptOutcome::GapRetained => (IngestOutcome::AwaitingPredecessor, None),
            AcceptOutcome::GapChainFull => (IngestOutcome::HeldChainFull, None),
            AcceptOutcome::AlreadyGapped => (IngestOutcome::AlreadyAwaiting, None),
            AcceptOutcome::Fork => (IngestOutcome::Forked, None),
            AcceptOutcome::Unauthorized => (IngestOutcome::Unauthorized, None),
        },
    )
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
