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
        let op = match readoption_repair_op(tx, ctx, spec, stream, &candidate.row_pk, &removed_hex)?
        {
            ReadoptionRepair::Skip => continue,
            ReadoptionRepair::Unrepairable => {
                unrepairable += 1;
                continue;
            },
            ReadoptionRepair::Repair(op) => op,
        };
        let signed = store::author_row_entry(tx, stream, ctx.device.secret(), &op, ctx.now_ms)?;
        let meta =
            OpMeta { lamport: signed.entry.lamport, device: signed.entry.device_fingerprint };
        match apply::apply_row_op_on_stream(tx, spec, ctx.repo_id, stream, &op, meta)? {
            ApplyOutcome::Applied => {
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
                    adopted_entry_hash: signed.entry.entry_hash,
                    adopted_at_ms: ctx.now_ms,
                })?;
                authored += 1;
            },
            ApplyOutcome::Superseded => {
                // Same diagnosis as the produce path: a locally-authored op takes the stream's
                // MAX(lamport)+1, so losing its own self-apply means the row's clock carries a
                // lamport from ANOTHER stream — the shape a scope or account move leaves behind.
                // Swallowing it would mark the removal complete with the orphan unrepaired and no
                // retry left. Fail the pass instead: the work item stays pending and the rollback
                // drops the just-inserted entry.
                anyhow::bail!(
                    "table-sync: a re-adoption op lost its own self-apply on `{}` — the row's \
                     write clock carries a lamport from another stream",
                    spec.name
                );
            },
            outcome @ (ApplyOutcome::Quarantined(_) | ApplyOutcome::Unprojectable(_)) => {
                anyhow::bail!(
                    "table-sync: a re-adoption op did not self-apply on `{}`: {outcome:?}",
                    spec.name
                );
            },
        }
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

/// What the drain can do with one candidate row.
enum ReadoptionRepair {
    /// Re-author this op under the local key.
    Repair(RowOp),
    /// Nothing owed: the row is settled under another writer, or physically consistent with its
    /// merge state (an absent row under a live clock is the producer's `Remove` to author, not
    /// this pass's).
    Skip,
    /// The physical row exists but a synced column cannot be read as its declared type. NOT
    /// abandoned: the work item stays pending so a pass after the cell is repaired still
    /// converges the fresh replica.
    Unrepairable,
}

/// The physical-table repair for one still-orphaned candidate row.
fn readoption_repair_op(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
    spec: &TableSpec,
    stream: crate::stream::StreamId,
    row_pk: &str,
    removed_hex: &str,
) -> anyhow::Result<ReadoptionRepair> {
    let clock = apply::row_clock_winner_on_stream(tx, stream, ctx.repo_id, spec.name, row_pk)?;
    let tombstone = apply::tombstone_winner_on_stream(tx, stream, ctx.repo_id, spec.name, row_pk)?;
    Ok(match (clock, tombstone) {
        // A live clock and a tombstone can only coexist with the clock newer: a remove raises the
        // tombstone at its own lamport, and a remove that BEATS the clock clears the clock. So a
        // live clock always owns the row; re-adopt it while the removed writer is that winner.
        // What the PHYSICAL row allows decides the repair: carried (re-author), absent (the
        // producer's Remove owns it), or unreadable (stay pending until repaired).
        (Some((_, winner)), _) if winner == removed_hex => {
            let pk = row_op::row_pk_values(row_pk)?;
            match apply::read_synced_cells(tx, spec, &pk)? {
                apply::SyncedRow::Cells(cells) => ReadoptionRepair::Repair(RowOp::Upsert {
                    table: spec.name.to_string(),
                    spec_version: spec.spec_version,
                    pk,
                    cells,
                }),
                apply::SyncedRow::Absent => ReadoptionRepair::Skip,
                apply::SyncedRow::Unreadable(_) => ReadoptionRepair::Unrepairable,
            }
        },
        // A tombstone with no live clock owns the deletion. Re-adopt it only while no physical
        // row exists; otherwise a live local row would be destroyed by a stale repair.
        (None, Some((_, winner))) if winner == removed_hex => {
            let pk = row_op::row_pk_values(row_pk)?;
            match apply::read_synced_cells(tx, spec, &pk)? {
                apply::SyncedRow::Absent =>
                    ReadoptionRepair::Repair(apply::readopt_remove(spec, row_pk)?),
                apply::SyncedRow::Cells(_) => ReadoptionRepair::Skip,
                apply::SyncedRow::Unreadable(_) => ReadoptionRepair::Unrepairable,
            }
        },
        _ => ReadoptionRepair::Skip,
    })
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
    let (outcome, mut tail) =
        ingest_one(tx, ctx, scope_id, signed_bytes, pubkey, advertised_floor)?;
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
            drain_children(tx, ctx, scope_id, pubkey, stream, device, &prev, &mut promoted)?;
        }
        // A held entry on ANOTHER device's chain citing this hash is structurally impossible (a
        // chain links only within its own device), and accepting this entry is the only moment that
        // is decidable — `classify` keys the tail on the citing device's chain, so it would report
        // `Gap` forever. Sweep those now or they hold that chain's capacity until eviction.
        let foreign =
            store::discard_foreign_chain_citations(tx, stream, device, &accepted.entry_hash)?;
        promoted.extend(std::iter::repeat_n(IngestOutcome::AbandonedBehindFork, foreign));
        tail = drain_children(
            tx,
            ctx,
            scope_id,
            pubkey,
            stream,
            device,
            &accepted.entry_hash,
            &mut promoted,
        )?;
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

/// The entry an acceptance put at the chain tail — what a promotion probe keys on.
#[derive(Debug, Clone, Copy)]
struct AcceptedEntry {
    entry_hash: [u8; 32],
    prev_hash: Option<[u8; 32]>,
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
#[allow(clippy::too_many_arguments)]
fn drain_children(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
    scope_id: &str,
    pubkey: &DevicePublic,
    stream: crate::stream::StreamId,
    device: crate::op::DeviceFingerprint,
    parent_hash: &[u8; 32],
    promoted: &mut Vec<IngestOutcome>,
) -> anyhow::Result<Option<AcceptedEntry>> {
    let mut took_the_slot = None;
    while let Some(child) = store::take_gapped_child(tx, stream, device, parent_hash)? {
        // A promoted child can never be the advertised floor root: this drain only runs after an
        // entry was ACCEPTED, so the local chain is non-empty and the floor branch cannot fire.
        let (outcome, stored) = ingest_one(tx, ctx, scope_id, &child.signed_bytes, pubkey, None)?;
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
    ctx: &SyncCtx<'_>,
    scope_id: &str,
    signed_bytes: &[u8],
    pubkey: &DevicePublic,
    advertised_floor: Option<store::AdvertisedFloor>,
) -> anyhow::Result<(IngestOutcome, Option<AcceptedEntry>)> {
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
            ctx.account_id,
            stream,
            &scope_tables,
            signed_bytes,
            pubkey,
            ctx.now_ms,
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
mod tests {
    use super::*;
    use crate::table_sync::registry::{ColumnSpec, ValueType};

    const SPEC: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("title", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    const REGISTRY: &[TableSpec] = &[SPEC];

    /// One account's device: a fully-migrated store with the synthetic table and a minted identity.
    struct Device {
        conn: rusqlite::Connection,
        local: LocalDevice,
    }

    impl Device {
        fn new() -> Self {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
            seed_incarnation(&conn);
            conn.execute_batch("CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT) STRICT;")
                .unwrap();
            let local = crate::local_device(&conn, 0).unwrap();
            Self { conn, local }
        }

        fn pubkey(&self) -> DevicePublic {
            self.local.secret().public()
        }

        fn set_title(&self, title: &str) {
            self.conn.execute("UPDATE t_demo SET title = ?1 WHERE id = 'r1'", [title]).unwrap();
        }

        fn title(&self) -> Option<String> {
            self.conn
                .query_row("SELECT title FROM t_demo WHERE id = 'r1'", [], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
        }

        fn produce(&mut self) -> Vec<Vec<u8>> {
            let tx = self.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: AccountId::from_bytes([42; 32]),
                incarnation_ref: [0x44; 32],
                device: &self.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            let out = produce_and_author(&tx, &ctx).unwrap();
            tx.commit().unwrap();
            out
        }

        fn delete_row(&self) {
            self.conn.execute("DELETE FROM t_demo WHERE id = 'r1'", []).unwrap();
        }

        /// Deliver `entries` in the given order, returning the FULL report per entry — including
        /// what each arrival promoted out of the gapped table.
        fn ingest_reports(
            &mut self,
            entries: &[Vec<u8>],
            from: &DevicePublic,
        ) -> Vec<IngestReport> {
            enroll_writer(&self.conn, AccountId::from_bytes([42; 32]), from.fingerprint());
            let tx = self.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: AccountId::from_bytes([42; 32]),
                incarnation_ref: [0x44; 32],
                device: &self.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            let out =
                entries.iter().map(|bytes| ingest(&tx, &ctx, "demo/1", bytes, from, None).unwrap());
            let out = out.collect();
            tx.commit().unwrap();
            out
        }

        /// Entries held awaiting a predecessor, across every stream.
        fn gapped_count(&self) -> i64 {
            self.conn
                .query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |r| r.get(0))
                .unwrap()
        }

        fn ingest_all(&mut self, entries: &[Vec<u8>], from: &DevicePublic) -> Vec<IngestOutcome> {
            // The receiver has folded the author's DeviceAdd, so it is an effective writer here —
            // otherwise the #935 authority gate would drop every entry as Unauthorized.
            enroll_writer(&self.conn, AccountId::from_bytes([42; 32]), from.fingerprint());
            let tx = self.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: AccountId::from_bytes([42; 32]),
                incarnation_ref: [0x44; 32],
                device: &self.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            let out = entries
                .iter()
                .map(|bytes| ingest(&tx, &ctx, "demo/1", bytes, from, None).unwrap().outcome);
            let out = out.collect();
            tx.commit().unwrap();
            out
        }
    }

    /// Enroll `fp` as a roster-effective writer (Owner) of `account`, so the #935 ingest gate
    /// admits its entries — the receiver-side view after it has folded the author's
    /// `DeviceAdd`.
    fn enroll_writer(
        conn: &rusqlite::Connection,
        account: AccountId,
        fp: crate::op::DeviceFingerprint,
    ) {
        conn.execute(
            "INSERT OR IGNORE INTO account_roster_history
                 (roster_ref, account_id, device_fingerprint, role, effective_at, closed_at)
             VALUES (?1, ?2, ?3, 'owner', 0, NULL)",
            rusqlite::params![
                fp.to_bytes().as_slice(),
                account.to_bytes().as_slice(),
                fp.to_bytes().as_slice()
            ],
        )
        .unwrap();
    }

    /// Close `fp`'s roster row — the device is removed from the account, so its entries fail the
    /// #935 gate and the drain-time effectiveness re-check sees the removal.
    fn remove_writer(
        conn: &rusqlite::Connection,
        account: AccountId,
        fp: crate::op::DeviceFingerprint,
    ) {
        let closed = conn
            .execute(
                "UPDATE account_roster_history SET closed_at = 1
                 WHERE account_id = ?1 AND device_fingerprint = ?2 AND closed_at IS NULL",
                rusqlite::params![account.to_bytes().as_slice(), fp.to_bytes().as_slice()],
            )
            .unwrap();
        assert_eq!(closed, 1, "the device was on the roster to remove");
    }

    /// Re-open `fp`'s closed roster row — the device is re-invited and effective again.
    fn reinvite_writer(
        conn: &rusqlite::Connection,
        account: AccountId,
        fp: crate::op::DeviceFingerprint,
    ) {
        let reopened = conn
            .execute(
                "UPDATE account_roster_history SET closed_at = NULL
                 WHERE account_id = ?1 AND device_fingerprint = ?2 AND closed_at IS NOT NULL",
                rusqlite::params![account.to_bytes().as_slice(), fp.to_bytes().as_slice()],
            )
            .unwrap();
        assert_eq!(reopened, 1, "the device was removed to re-invite");
    }

    fn seed_incarnation(conn: &rusqlite::Connection) {
        conn.execute(
            "INSERT INTO account_repo_incarnation_current(
                 account_id, repository_id, incarnation_ref
             ) VALUES (?1, 'repo', ?2)",
            rusqlite::params![
                AccountId::from_bytes([42; 32]).to_bytes().as_slice(),
                [0x44u8; 32].as_slice()
            ],
        )
        .unwrap();
    }

    #[test]
    fn readoption_re_authors_a_removed_writers_row_and_uses_it_to_converge_a_fresh_replica() {
        let mut a = Device::new(); // the original author, later removed from the roster
        let mut c = Device::new(); // a current writer that already holds A's row
        let mut d = Device::new(); // a replica enrolled only AFTER A had left

        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'distilled')", []).unwrap();
        let entry = a.produce();
        assert_eq!(entry.len(), 1);

        enroll_writer(&c.conn, AccountId::from_bytes([42; 32]), a.pubkey().fingerprint());
        enroll_writer(&c.conn, AccountId::from_bytes([42; 32]), c.local.fingerprint());
        assert_eq!(c.ingest_all(&entry, &a.pubkey()), vec![IngestOutcome::Applied]);
        assert_eq!(c.title().as_deref(), Some("distilled"));
        let account = AccountId::from_bytes([42; 32]);
        remove_writer(&c.conn, account, a.pubkey().fingerprint());
        let stream = scope_stream_id("repo", account, [0x44; 32], "demo/1");
        let removal_ref = [7; 32];
        {
            let tx = c.conn.transaction().unwrap();
            store::enqueue_readoption_work(
                &tx,
                account,
                a.pubkey().fingerprint(),
                stream,
                removal_ref,
                9,
                10,
            )
            .unwrap();
            tx.commit().unwrap();
        }

        let reauthored = c.produce();
        assert!(reauthored.is_empty(), "ordinary production does not re-adopt without the driver");
        {
            let tx = c.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &c.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            let processed = process_readoption_work_for_stream(&tx, &ctx, stream).unwrap();
            tx.commit().unwrap();
            assert_eq!(processed, Some(1), "the orphaned row is re-authored once");
        }
        let audit_count: i64 = c
            .conn
            .query_row("SELECT COUNT(*) FROM table_sync_readoption_audit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(audit_count, 1, "the re-authorship carries its provenance");

        let reauthored = {
            let tail: Vec<u8> = c
                .conn
                .query_row(
                    "SELECT signed_bytes FROM table_sync_entries
                     WHERE stream_id = ?1 AND device_fingerprint = ?2
                     ORDER BY lamport DESC LIMIT 1",
                    rusqlite::params![
                        stream.to_bytes().as_slice(),
                        c.local.fingerprint().to_bytes().as_slice()
                    ],
                    |row| row.get(0),
                )
                .unwrap();
            vec![tail]
        };

        enroll_writer(&d.conn, AccountId::from_bytes([42; 32]), c.local.fingerprint());
        assert_eq!(d.ingest_all(&reauthored, &c.pubkey()), vec![IngestOutcome::Applied]);
        assert_eq!(d.title().as_deref(), Some("distilled"));
    }

    #[test]
    fn readoption_never_authors_a_remove_while_the_physical_row_is_live() {
        let mut a = Device::new(); // creates AND deletes r1, then leaves the roster
        let mut c = Device::new();
        let account = AccountId::from_bytes([42; 32]);
        let stream = scope_stream_id("repo", account, [0x44; 32], "demo/1");

        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'created')", []).unwrap();
        let create = a.produce();
        enroll_writer(&c.conn, account, a.pubkey().fingerprint());
        enroll_writer(&c.conn, account, c.local.fingerprint());
        c.ingest_all(&create, &a.pubkey());
        a.delete_row();
        let delete = a.produce();
        assert_eq!(delete.len(), 1);
        c.ingest_all(&delete, &a.pubkey());
        assert_eq!(c.title(), None, "A's delete landed: r1 is gone, tombstoned under A");
        remove_writer(&c.conn, account, a.pubkey().fingerprint());

        // Recreate the pk locally without publishing it. This is exactly the state the scan-based
        // design destroyed: tombstone state says removed-author delete wins, while the physical
        // table says the row is live again.
        c.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'recreated')", []).unwrap();

        {
            let tx = c.conn.transaction().unwrap();
            store::enqueue_readoption_work(
                &tx,
                account,
                a.pubkey().fingerprint(),
                stream,
                [8; 32],
                11,
                12,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = c.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &c.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            assert_eq!(
                process_readoption_work_for_stream(&tx, &ctx, stream).unwrap(),
                Some(0),
                "the physical-liveness guard declines the stale tombstone repair",
            );
            tx.commit().unwrap();
        }
        let out = c.produce();
        assert_eq!(out.len(), 1, "only the legitimate local upsert is authored");
        assert_eq!(c.title().as_deref(), Some("recreated"));
        let removes: i64 = c
            .conn
            .query_row("SELECT COUNT(*) FROM table_sync_readoption_audit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(removes, 0, "no stale tombstone re-adoption can delete a live row");
    }

    /// A device removed, drained, re-invited, and removed again must have its SECOND removal
    /// re-arm the worklist — the roster_ref distinguishes the two removals (#997 review).
    #[test]
    fn a_second_removal_after_drain_re_adopts_the_devices_new_rows() {
        let mut a = Device::new(); // invited, removed, re-invited, removed again
        let mut c = Device::new(); // the current writer that holds every copy
        let account = AccountId::from_bytes([42; 32]);
        let stream = scope_stream_id("repo", account, [0x44; 32], "demo/1");

        // Round one: A authors r1, C ingests it, A is removed, and C drains the work.
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'first')", []).unwrap();
        let first = a.produce();
        enroll_writer(&c.conn, account, a.pubkey().fingerprint());
        enroll_writer(&c.conn, account, c.local.fingerprint());
        c.ingest_all(&first, &a.pubkey());
        remove_writer(&c.conn, account, a.pubkey().fingerprint());
        {
            let tx = c.conn.transaction().unwrap();
            store::enqueue_readoption_work(
                &tx,
                account,
                a.pubkey().fingerprint(),
                stream,
                [7; 32],
                9,
                10,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = c.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &c.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            assert_eq!(process_readoption_work_for_stream(&tx, &ctx, stream).unwrap(), Some(1));
            tx.commit().unwrap();
        }

        // Re-invite: A is roster-effective again, authors r2, and C ingests it. A's SECOND
        // removal carries a new roster_ref.
        reinvite_writer(&c.conn, account, a.pubkey().fingerprint());
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'second')", []).unwrap();
        let second = a.produce();
        c.ingest_all(&second, &a.pubkey());
        remove_writer(&c.conn, account, a.pubkey().fingerprint());
        {
            let tx = c.conn.transaction().unwrap();
            store::enqueue_readoption_work(
                &tx,
                account,
                a.pubkey().fingerprint(),
                stream,
                [9; 32],
                21,
                22,
            )
            .unwrap();
            assert!(
                store::readoption_work_for_stream(&tx, account, stream).unwrap().is_some(),
                "a different roster_ref resets processed_at_ms",
            );
            tx.commit().unwrap();
        }
        {
            let tx = c.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &c.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            // r1's clock winner is C after round one; only r2 is still orphaned under A.
            assert_eq!(
                process_readoption_work_for_stream(&tx, &ctx, stream).unwrap(),
                Some(1),
                "the second removal re-adopts only the newly orphaned rows",
            );
            tx.commit().unwrap();
        }
        let audit_count: i64 = c
            .conn
            .query_row("SELECT COUNT(*) FROM table_sync_readoption_audit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(audit_count, 2, "both rounds left their provenance");

        // An idempotent re-fold of the SAME removal does not re-arm the drained row.
        {
            let tx = c.conn.transaction().unwrap();
            store::enqueue_readoption_work(
                &tx,
                account,
                a.pubkey().fingerprint(),
                stream,
                [9; 32],
                21,
                23,
            )
            .unwrap();
            assert!(
                store::readoption_work_for_stream(&tx, account, stream).unwrap().is_none(),
                "the same roster_ref is a re-fold, not a new removal",
            );
            tx.commit().unwrap();
        }
    }

    /// A row written twice by the removed device must have its audit name the entry the row's
    /// LWW clock actually points at — the latest one, not the first.
    #[test]
    fn the_audit_names_the_winning_entry_for_a_row_written_twice() {
        let mut a = Device::new();
        let mut c = Device::new();
        let account = AccountId::from_bytes([42; 32]);
        let stream = scope_stream_id("repo", account, [0x44; 32], "demo/1");

        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'v1')", []).unwrap();
        let first = a.produce();
        a.conn.execute("UPDATE t_demo SET title = 'v2' WHERE id = 'r1'", []).unwrap();
        let second = a.produce();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1, "the edit authors a second entry for the same row");

        enroll_writer(&c.conn, account, a.pubkey().fingerprint());
        enroll_writer(&c.conn, account, c.local.fingerprint());
        c.ingest_all(&first, &a.pubkey());
        c.ingest_all(&second, &a.pubkey());
        remove_writer(&c.conn, account, a.pubkey().fingerprint());
        {
            let tx = c.conn.transaction().unwrap();
            store::enqueue_readoption_work(
                &tx,
                account,
                a.pubkey().fingerprint(),
                stream,
                [7; 32],
                9,
                10,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = c.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &c.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            assert_eq!(process_readoption_work_for_stream(&tx, &ctx, stream).unwrap(), Some(1));
            tx.commit().unwrap();
        }
        let (original_lamport, original_hash): (i64, Vec<u8>) = c
            .conn
            .query_row(
                "SELECT original_lamport, original_entry_hash FROM table_sync_readoption_audit",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let winner_lamport: i64 = c
            .conn
            .query_row(
                "SELECT MAX(lamport) FROM table_sync_entries
                 WHERE stream_id = ?1 AND device_fingerprint = ?2",
                rusqlite::params![
                    stream.to_bytes().as_slice(),
                    a.pubkey().fingerprint().to_bytes().as_slice()
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            original_lamport, winner_lamport,
            "original_* names the winning entry, not the first write"
        );
        let winner_hash: Vec<u8> = c
            .conn
            .query_row(
                "SELECT entry_hash FROM table_sync_entries
                 WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport = ?3",
                rusqlite::params![
                    stream.to_bytes().as_slice(),
                    a.pubkey().fingerprint().to_bytes().as_slice(),
                    original_lamport
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(original_hash, winner_hash, "the audit joins back by (lamport, hash)");
    }

    /// A later write that quarantined never owned the row's clock, so the audit must not name it:
    /// dedup follows the clock, not the device's latest entry.
    #[test]
    fn the_audit_skips_a_quarantined_later_write_that_never_owned_the_clock() {
        let mut a = Device::new();
        let mut c = Device::new();
        let account = AccountId::from_bytes([42; 32]);
        let stream = scope_stream_id("repo", account, [0x44; 32], "demo/1");

        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'v1')", []).unwrap();
        let first = a.produce();
        enroll_writer(&c.conn, account, a.pubkey().fingerprint());
        enroll_writer(&c.conn, account, c.local.fingerprint());
        c.ingest_all(&first, &a.pubkey());
        let first_hash = crate::entry::decode_signed(&first[0]).unwrap().entry.entry_hash;

        // A's second write carries a wrongly-typed cell: stored and quarantined, never applied,
        // so the row's clock still points at the FIRST entry.
        let bad_op = RowOp::Upsert {
            spec_version: 1,
            table: "t_demo".to_string(),
            pk: vec![row_op::TypedValue::Text("r1".to_string())],
            cells: vec![row_op::Cell {
                column: "title".to_string(),
                value: row_op::TypedValue::I64(1),
            }],
        };
        let bad = crate::entry::sign_entry_from_op_bytes(
            a.local.secret(),
            stream,
            Some(first_hash),
            1,
            row_op::encode(&bad_op),
        );
        let outcomes = c.ingest_all(std::slice::from_ref(&bad.signed_bytes), &a.pubkey());
        assert!(
            matches!(outcomes.as_slice(), [IngestOutcome::Quarantined(_)]),
            "the malformed write quarantines instead of taking the clock: {outcomes:?}",
        );
        remove_writer(&c.conn, account, a.pubkey().fingerprint());

        {
            let tx = c.conn.transaction().unwrap();
            store::enqueue_readoption_work(
                &tx,
                account,
                a.pubkey().fingerprint(),
                stream,
                [7; 32],
                9,
                10,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = c.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &c.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            assert_eq!(process_readoption_work_for_stream(&tx, &ctx, stream).unwrap(), Some(1));
            tx.commit().unwrap();
        }
        let original_lamport: i64 = c
            .conn
            .query_row("SELECT original_lamport FROM table_sync_readoption_audit", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(original_lamport, 0, "the audit names the entry the clock points at");
    }

    /// A device re-invited before the drain is effective again: its entries ingest directly, so
    /// the pending removal completes WITHOUT re-authoring its rows.
    #[test]
    fn a_reinvited_devices_pending_removal_completes_without_reauthoring() {
        let mut a = Device::new();
        let mut c = Device::new();
        let account = AccountId::from_bytes([42; 32]);
        let stream = scope_stream_id("repo", account, [0x44; 32], "demo/1");

        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'v')", []).unwrap();
        let entry = a.produce();
        enroll_writer(&c.conn, account, a.pubkey().fingerprint());
        enroll_writer(&c.conn, account, c.local.fingerprint());
        c.ingest_all(&entry, &a.pubkey());
        {
            let tx = c.conn.transaction().unwrap();
            store::enqueue_readoption_work(
                &tx,
                account,
                a.pubkey().fingerprint(),
                stream,
                [7; 32],
                9,
                10,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        // A is re-invited (roster-effective again) before any sync pass drains the work.
        {
            let tx = c.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &c.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            assert_eq!(process_readoption_work_for_stream(&tx, &ctx, stream).unwrap(), Some(0));
            tx.commit().unwrap();
        }
        let audit_count: i64 = c
            .conn
            .query_row("SELECT COUNT(*) FROM table_sync_readoption_audit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(audit_count, 0, "an effective writer keeps its own clock ownership");
        {
            let tx = c.conn.transaction().unwrap();
            assert!(
                !store::has_pending_readoption_work(&tx, account, stream).unwrap(),
                "the stale work item is completed, not left to re-author later",
            );
            tx.commit().unwrap();
        }
    }

    /// An unreadable synced column must not be written off: the work item stays pending, and a
    /// pass after the cell is repaired still re-adopts the row.
    #[test]
    fn an_unreadable_orphan_stays_pending_until_the_cell_is_repaired() {
        const BOOL_SPEC: TableSpec = TableSpec {
            name: "t_typed",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("flag", ValueType::Bool)],
            local_columns: &[],
            repo_column: None,
        };
        const BOOL_REGISTRY: &[TableSpec] = &[BOOL_SPEC];

        let mut a = Device::new();
        let mut c = Device::new();
        let account = AccountId::from_bytes([42; 32]);
        let stream = scope_stream_id("repo", account, [0x44; 32], "demo/1");
        for device in [&a, &c] {
            device
                .conn
                .execute_batch("CREATE TABLE t_typed(id TEXT PRIMARY KEY, flag INTEGER) STRICT;")
                .unwrap();
        }

        a.conn.execute("INSERT INTO t_typed(id, flag) VALUES ('r1', 1)", []).unwrap();
        let entry = {
            let tx = a.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &a.local,
                registry: BOOL_REGISTRY,
                now_ms: 0,
            };
            let out = produce_and_author(&tx, &ctx).unwrap();
            tx.commit().unwrap();
            out
        };
        enroll_writer(&c.conn, account, a.pubkey().fingerprint());
        enroll_writer(&c.conn, account, c.local.fingerprint());
        {
            let tx = c.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &c.local,
                registry: BOOL_REGISTRY,
                now_ms: 0,
            };
            for bytes in &entry {
                ingest(&tx, &ctx, "demo/1", bytes, &a.pubkey(), None).unwrap();
            }
            tx.commit().unwrap();
        }
        // A raw write leaves the cell unreadable as its declared type (STRICT stores the integer;
        // reading it as Bool rejects anything but 0/1).
        c.conn.execute("UPDATE t_typed SET flag = 2 WHERE id = 'r1'", []).unwrap();
        remove_writer(&c.conn, account, a.pubkey().fingerprint());
        {
            let tx = c.conn.transaction().unwrap();
            store::enqueue_readoption_work(
                &tx,
                account,
                a.pubkey().fingerprint(),
                stream,
                [7; 32],
                9,
                10,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = c.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &c.local,
                registry: BOOL_REGISTRY,
                now_ms: 0,
            };
            assert_eq!(
                process_readoption_work_for_stream(&tx, &ctx, stream).unwrap(),
                None,
                "an unreadable row cannot be drained today",
            );
            assert!(
                store::has_pending_readoption_work(&tx, account, stream).unwrap(),
                "and the removal is NOT written off",
            );
            tx.commit().unwrap();
        }

        // Repair the cell content-identically: anti-echo means the producer authors nothing, so
        // only the re-adoption pass can carry the row to a fresh replica.
        c.conn.execute("UPDATE t_typed SET flag = 1 WHERE id = 'r1'", []).unwrap();
        {
            let tx = c.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &c.local,
                registry: BOOL_REGISTRY,
                now_ms: 0,
            };
            assert_eq!(
                process_readoption_work_for_stream(&tx, &ctx, stream).unwrap(),
                Some(1),
                "the repaired row is re-adopted by the retry",
            );
            tx.commit().unwrap();
        }
        let audit_count: i64 = c
            .conn
            .query_row("SELECT COUNT(*) FROM table_sync_readoption_audit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(audit_count, 1);
    }

    /// Two devices removed on one stream drain in ONE pass — the second repair does not wait a
    /// whole sync session.
    #[test]
    fn one_pass_drains_every_pending_removal_on_a_stream() {
        let mut a = Device::new();
        let mut b = Device::new();
        let mut c = Device::new();
        let account = AccountId::from_bytes([42; 32]);
        let stream = scope_stream_id("repo", account, [0x44; 32], "demo/1");

        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'from-a')", []).unwrap();
        b.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'from-b')", []).unwrap();
        let from_a = a.produce();
        let from_b = b.produce();
        enroll_writer(&c.conn, account, a.pubkey().fingerprint());
        enroll_writer(&c.conn, account, b.pubkey().fingerprint());
        enroll_writer(&c.conn, account, c.local.fingerprint());
        c.ingest_all(&from_a, &a.pubkey());
        c.ingest_all(&from_b, &b.pubkey());
        remove_writer(&c.conn, account, a.pubkey().fingerprint());
        remove_writer(&c.conn, account, b.pubkey().fingerprint());

        for (fingerprint, roster_ref) in
            [(a.pubkey().fingerprint(), [7; 32]), (b.pubkey().fingerprint(), [8; 32])]
        {
            let tx = c.conn.transaction().unwrap();
            store::enqueue_readoption_work(&tx, account, fingerprint, stream, roster_ref, 9, 10)
                .unwrap();
            tx.commit().unwrap();
        }

        let tx = c.conn.transaction().unwrap();
        let ctx = SyncCtx {
            repo_id: "repo",
            account_id: account,
            incarnation_ref: [0x44; 32],
            device: &c.local,
            registry: REGISTRY,
            now_ms: 0,
        };
        while store::has_pending_readoption_work(&tx, account, stream).unwrap() {
            process_readoption_work_for_stream(&tx, &ctx, stream).unwrap();
        }
        tx.commit().unwrap();

        let audit_count: i64 = c
            .conn
            .query_row("SELECT COUNT(*) FROM table_sync_readoption_audit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(audit_count, 2, "both removals were repaired in the same pass");
        assert_eq!(c.title().as_deref(), Some("from-a"));
        let r2: String = c
            .conn
            .query_row("SELECT title FROM t_demo WHERE id = 'r2'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(r2, "from-b");
    }

    /// Three rows authored in order on A, then delivered to B in REVERSE. Before entries awaiting
    /// a predecessor were retained, everything after the gap was dropped and only redelivery in
    /// exact causal order could recover it.
    #[test]
    fn a_chain_delivered_in_reverse_converges() {
        let mut a = Device::new();
        let mut b = Device::new();
        let mut entries = Vec::new();
        for (id, title) in [("r1", "one"), ("r2", "two"), ("r3", "three")] {
            a.conn.execute("INSERT INTO t_demo(id, title) VALUES (?1, ?2)", [id, title]).unwrap();
            entries.extend(a.produce());
        }
        assert_eq!(entries.len(), 3, "one entry per authored row");

        entries.reverse();
        let reports = b.ingest_reports(&entries, &a.pubkey());

        // The first two arrive with no predecessor and are held; the third completes the chain and
        // drags both forward behind it.
        assert_eq!(reports[0].outcome, IngestOutcome::AwaitingPredecessor);
        assert_eq!(reports[1].outcome, IngestOutcome::AwaitingPredecessor);
        assert_eq!(reports[2].outcome, IngestOutcome::Applied);
        assert_eq!(
            reports[2].promoted,
            vec![IngestOutcome::Applied, IngestOutcome::Applied],
            "the genesis promotes both retained successors, in chain order",
        );
        let titles: Vec<String> = b
            .conn
            .prepare("SELECT title FROM t_demo ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(titles, ["one", "two", "three"], "every row lands");
        assert_eq!(b.gapped_count(), 0, "and nothing is left held");
    }

    /// A long chain delivered in reverse converges in ONE delivery pass, at a write cost linear in
    /// its length.
    ///
    /// Scoped to what the instrument can actually see. `total_changes` counts rows written, so it
    /// catches a promote path that writes per-promotion work proportional to the chain (the
    /// quadratic shape), but it cannot see read cost — a purely-reading rescan would be invisible
    /// here, and nothing in this test would catch it. The depth is likewise chosen to exercise the
    /// iterative walk, not to prove recursion would overflow: a few hundred frames would not.
    #[test]
    fn a_long_chain_delivered_in_reverse_converges_at_linear_write_cost() {
        const ROWS: usize = 200;
        let mut a = Device::new();
        let mut b = Device::new();
        let mut entries = Vec::new();
        for i in 0..ROWS {
            a.conn
                .execute("INSERT INTO t_demo(id, title) VALUES (?1, 't')", [format!("r{i:04}")])
                .unwrap();
            entries.extend(a.produce());
        }
        assert_eq!(entries.len(), ROWS);
        entries.reverse();

        let before = b.conn.total_changes();
        let reports = b.ingest_reports(&entries, &a.pubkey());
        let writes = b.conn.total_changes() - before;

        assert_eq!(reports[ROWS - 1].promoted.len(), ROWS - 1, "one promotion per held entry");
        let rows: i64 = b.conn.query_row("SELECT COUNT(*) FROM t_demo", [], |r| r.get(0)).unwrap();
        assert_eq!(rows as usize, ROWS, "every row lands");
        assert_eq!(b.gapped_count(), 0, "and nothing is left held");
        // Each entry costs a bounded number of writes: retain, take (delete), insert, apply, plus
        // the row-clock and published-row bookkeeping. A per-promotion write-amplifying rescan
        // blows past this by orders of magnitude; the bound is loose enough not to be a churn
        // magnet.
        assert!(
            writes < (ROWS as u64) * 40,
            "delivery cost {writes} writes for {ROWS} entries — superlinear in the chain length",
        );
    }

    /// Redelivery of a held entry must not duplicate it, and must not be reported as settled: the
    /// per-chain cap can still evict it, unlike an accepted entry.
    #[test]
    fn a_redelivered_held_entry_is_recognized_and_not_duplicated() {
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'one')", []).unwrap();
        let genesis = a.produce();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'two')", []).unwrap();
        let second = a.produce();

        let first = b.ingest_reports(&second, &a.pubkey());
        assert_eq!(first[0].outcome, IngestOutcome::AwaitingPredecessor);
        assert_eq!(b.gapped_count(), 1);

        let again = b.ingest_reports(&second, &a.pubkey());
        assert_eq!(
            again[0].outcome,
            IngestOutcome::AlreadyAwaiting,
            "a redelivered held entry is recognized, and reported distinctly from AlreadyPresent",
        );
        assert_eq!(b.gapped_count(), 1, "and is not held twice");

        // It still promotes once the predecessor lands.
        let done = b.ingest_reports(&genesis, &a.pubkey());
        assert_eq!(done[0].promoted, vec![IngestOutcome::Applied]);
        assert_eq!(b.gapped_count(), 0);
    }

    /// Entries awaiting a predecessor are NOT on the accepted chain, so they must not move the
    /// stream's Lamport clock. If they did, one far-ahead held entry would drag local authoring
    /// with it — exactly what the lamport-advance bound exists to stop a single entry doing.
    #[test]
    fn a_held_entry_does_not_advance_the_stream_lamport_clock() {
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'one')", []).unwrap();
        let _genesis = a.produce();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'two')", []).unwrap();
        let second = a.produce();

        // B holds A's second entry (lamport 1) without its predecessor, then authors its own row.
        b.ingest_reports(&second, &a.pubkey());
        assert_eq!(b.gapped_count(), 1, "the entry is held, not accepted");
        b.conn.execute("INSERT INTO t_demo(id, title) VALUES ('own', 'mine')", []).unwrap();
        let mine = b.produce();
        assert_eq!(mine.len(), 1);

        let lamport: i64 = b
            .conn
            .query_row(
                "SELECT lamport FROM table_sync_entries WHERE device_fingerprint = ?1",
                rusqlite::params![b.pubkey().fingerprint().to_bytes().as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            lamport, 0,
            "B's own genesis takes lamport 0 — the held entry's lamport 1 is invisible to the \
             clock",
        );
    }

    /// The promote loop must drain the SIBLINGS of the entry it just accepted, not only that
    /// entry's child.
    ///
    /// Two held entries cite the same predecessor. When it arrives, one takes the successor slot;
    /// the other is now provably an equivocation — but it is still keyed to a predecessor whose
    /// slot is filled, so a loop that probes only the ADVANCING tail would never look at it again
    /// and it would sit in the table forever, re-examined on every future promotion.
    #[test]
    fn a_promotion_drains_the_sibling_it_just_proved_to_be_a_fork() {
        use crate::entry;
        use crate::table_sync::row_op;

        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'one')", []).unwrap();
        let genesis = a.produce();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'two')", []).unwrap();
        let second = a.produce();

        // A second successor of the genesis, signed by the same device — an equivocation.
        let stream = scope_stream_id("repo", AccountId::from_bytes([42; 32]), [0x44; 32], "demo/1");
        let genesis_hash: [u8; 32] = a
            .conn
            .query_row(
                "SELECT entry_hash FROM table_sync_entries ORDER BY lamport LIMIT 1",
                [],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .unwrap()
            .try_into()
            .unwrap();
        let sibling = entry::sign_entry_from_op_bytes(
            a.local.secret(),
            stream,
            Some(genesis_hash),
            9,
            row_op::encode(&row_op::RowOp::Remove {
                table: "t_demo".into(),
                pk: vec![row_op::TypedValue::Text("r9".into())],
                spec_version: 1,
            }),
        );

        // Both successors arrive before the genesis and are held.
        let held =
            b.ingest_reports(&[second[0].clone(), sibling.signed_bytes.clone()], &a.pubkey());
        assert_eq!(held[0].outcome, IngestOutcome::AwaitingPredecessor);
        assert_eq!(held[1].outcome, IngestOutcome::AwaitingPredecessor);
        assert_eq!(b.gapped_count(), 2);

        let report = b.ingest_reports(&genesis, &a.pubkey());
        assert_eq!(report[0].outcome, IngestOutcome::Applied);
        assert!(
            report[0].promoted.contains(&IngestOutcome::Forked),
            "the losing sibling is judged, not left held: {:?}",
            report[0].promoted,
        );
        assert_eq!(
            b.gapped_count(),
            0,
            "and the table is empty — nothing is stranded behind a filled successor slot",
        );
    }

    /// A promoted entry goes through the SAME gates as a freshly delivered one — it is fed back
    /// through the whole accept-and-apply path, not written straight into its table.
    ///
    /// The observable is the unsent-work guard: B holds an entry that would overwrite a local edit
    /// no peer has seen. When the predecessor arrives and promotes it, it must DEFER, exactly as it
    /// would have on direct delivery. A promote path that wrote the row directly would clobber the
    /// edit and this would read "from-A".
    #[test]
    fn a_promoted_entry_still_defers_to_unsent_local_work() {
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
        b.ingest_all(&a.produce(), &a.pubkey());

        // A's next two entries: an unrelated row, then the edit to r1 that would overwrite B.
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'two')", []).unwrap();
        let filler = a.produce();
        a.set_title("from-A");
        let edit = a.produce();

        // B makes its own unsent edit, then receives A's LAST entry first — so it is held.
        b.set_title("from-B");
        let held = b.ingest_reports(&edit, &a.pubkey());
        assert_eq!(held[0].outcome, IngestOutcome::AwaitingPredecessor);
        assert_eq!(b.gapped_count(), 1);

        // The predecessor arrives and promotes it. The guard must still fire.
        let report = b.ingest_reports(&filler, &a.pubkey());
        assert_eq!(
            report[0].promoted,
            vec![IngestOutcome::Retained(
                crate::table_sync::store::PendingReason::DeferredUnsentEdit.as_db_str()
            )],
            "the promoted entry defers rather than applying",
        );
        assert_eq!(b.title().as_deref(), Some("from-B"), "B's unsent edit survives promotion");
        assert_eq!(b.gapped_count(), 0, "and the entry left the held table — it is stored now");
    }

    /// Held entries are a CHAIN state, not a projection state: they carry no pending reason and
    /// must not make a refold owed. Their redemption trigger is a predecessor's arrival, not a
    /// projector-version bump — and a refold owed on every open is the per-open cost #1005's
    /// narrowing exists to avoid.
    #[test]
    fn held_entries_do_not_make_a_refold_owed() {
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'one')", []).unwrap();
        let _genesis = a.produce();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'two')", []).unwrap();
        let second = a.produce();

        // Settle any refold the fresh store owes for unrelated reasons (a first-open version
        // stamp), so what this test observes afterwards is attributable to the held entry alone.
        refold::refold_stale_projections_against(&b.conn, REGISTRY).unwrap();

        b.ingest_reports(&second, &a.pubkey());
        assert_eq!(b.gapped_count(), 1, "an entry is held");

        let pending: i64 = b
            .conn
            .query_row(
                "SELECT COUNT(*) FROM table_sync_entries WHERE pending_reason IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0, "it is not recorded as a projection gap");
        assert!(
            !refold::refold_stale_projections_against(&b.conn, REGISTRY).unwrap(),
            "and no refold is owed while it waits",
        );
    }

    /// A held entry whose predecessor turns out to be a FORK is abandoned with it.
    ///
    /// Two successors of the genesis, X and Y, plus a held entry W citing X. When the genesis
    /// arrives, one of X/Y takes the successor slot and the other is judged a fork — and a fork is
    /// never stored, so nothing will ever put its hash on the chain. W therefore can never be
    /// promoted, and it is keyed to a hash no future acceptance produces, so no later probe would
    /// examine it either. Draining only the siblings themselves would leave it in the table
    /// permanently.
    #[test]
    fn a_held_entry_behind_a_fork_is_abandoned_with_it() {
        use crate::entry;
        use crate::table_sync::row_op;

        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'one')", []).unwrap();
        let genesis = a.produce();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'two')", []).unwrap();
        let winner = a.produce();

        let stream = scope_stream_id("repo", AccountId::from_bytes([42; 32]), [0x44; 32], "demo/1");
        let genesis_hash: [u8; 32] = a
            .conn
            .query_row(
                "SELECT entry_hash FROM table_sync_entries ORDER BY lamport LIMIT 1",
                [],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .unwrap()
            .try_into()
            .unwrap();
        let remove = |id: &str| {
            row_op::encode(&row_op::RowOp::Remove {
                table: "t_demo".into(),
                pk: vec![row_op::TypedValue::Text(id.into())],
                spec_version: 1,
            })
        };
        // The losing sibling, at a HIGHER lamport than the winner so the drain order is fixed.
        let loser = entry::sign_entry_from_op_bytes(
            a.local.secret(),
            stream,
            Some(genesis_hash),
            50,
            remove("r_loser"),
        );
        // And a child of the loser — held behind an entry that will never be stored.
        let orphan = entry::sign_entry_from_op_bytes(
            a.local.secret(),
            stream,
            Some(loser.entry.entry_hash),
            60,
            remove("r_orphan"),
        );

        let held = b.ingest_reports(
            &[winner[0].clone(), loser.signed_bytes.clone(), orphan.signed_bytes.clone()],
            &a.pubkey(),
        );
        assert!(held.iter().all(|r| r.outcome == IngestOutcome::AwaitingPredecessor));
        assert_eq!(b.gapped_count(), 3, "all three wait on predecessors");

        let report = b.ingest_reports(&genesis, &a.pubkey());
        assert!(
            report[0].promoted.contains(&IngestOutcome::Forked),
            "the losing sibling is judged: {:?}",
            report[0].promoted,
        );
        assert!(
            report[0].promoted.contains(&IngestOutcome::AbandonedBehindFork),
            "and its held child is abandoned with it, not left keyed to a hash that never lands: \
             {:?}",
            report[0].promoted,
        );
        assert_eq!(b.gapped_count(), 0, "nothing is stranded");
    }

    /// Draining must continue PAST a child that fails to store, or a valid successor queued behind
    /// an invalid one is stranded and the chain stops advancing.
    ///
    /// Two children cite the genesis: one at a lamport at/below the tail (an equivocation) and the
    /// real successor above it. The invalid one sorts first, so a drain that stopped at the first
    /// non-storing child would take the genesis's only probe, leave the successor slot open with
    /// nothing left to fill it, and halt there — the successor is keyed to a hash no later
    /// acceptance revisits.
    #[test]
    fn a_rejected_child_does_not_strand_the_valid_successor_behind_it() {
        use crate::entry;
        use crate::table_sync::row_op;

        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'one')", []).unwrap();
        let genesis = a.produce();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'two')", []).unwrap();
        let successor = a.produce();

        let stream = scope_stream_id("repo", AccountId::from_bytes([42; 32]), [0x44; 32], "demo/1");
        let genesis_hash: [u8; 32] = a
            .conn
            .query_row(
                "SELECT entry_hash FROM table_sync_entries ORDER BY lamport LIMIT 1",
                [],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .unwrap()
            .try_into()
            .unwrap();
        // Lamport 0 ties the genesis's own lamport, so this classifies at/below the tail — a
        // conflict — and sorts BEFORE the real successor at lamport 1.
        let invalid = entry::sign_entry_from_op_bytes(
            a.local.secret(),
            stream,
            Some(genesis_hash),
            0,
            row_op::encode(&row_op::RowOp::Remove {
                table: "t_demo".into(),
                pk: vec![row_op::TypedValue::Text("r_bogus".into())],
                spec_version: 1,
            }),
        );

        let held =
            b.ingest_reports(&[invalid.signed_bytes.clone(), successor[0].clone()], &a.pubkey());
        assert!(held.iter().all(|r| r.outcome == IngestOutcome::AwaitingPredecessor));
        assert_eq!(b.gapped_count(), 2);

        let report = b.ingest_reports(&genesis, &a.pubkey());
        assert!(
            report[0].promoted.contains(&IngestOutcome::Forked),
            "the invalid child is judged: {:?}",
            report[0].promoted,
        );
        assert!(
            report[0].promoted.contains(&IngestOutcome::Applied),
            "and the drain continues past it to the real successor: {:?}",
            report[0].promoted,
        );
        assert_eq!(
            b.conn
                .query_row("SELECT title FROM t_demo WHERE id = 'r2'", [], |r| r
                    .get::<_, String>(0))
                .ok()
                .as_deref(),
            Some("two"),
            "the successor's row lands — the chain did not halt on the rejected child",
        );
        assert_eq!(b.gapped_count(), 0, "nothing is stranded");
    }

    /// An entry citing a predecessor from ANOTHER device's chain can never be honest — a chain
    /// links only within its own device. It is retained on arrival (until the cited hash is held,
    /// it is indistinguishable from an ordinary missing predecessor), and accepting that hash is
    /// the only moment the impossibility becomes decidable: `classify` keys the tail on the citing
    /// device's own chain, so re-examining it later reports `Gap` forever.
    #[test]
    fn a_held_entry_citing_another_devices_chain_is_discarded_when_that_entry_lands() {
        use crate::entry;
        use crate::table_sync::row_op;

        let mut a = Device::new();
        let c = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'one')", []).unwrap();
        let genesis = a.produce();
        let a_genesis_hash: [u8; 32] = a
            .conn
            .query_row("SELECT entry_hash FROM table_sync_entries LIMIT 1", [], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .unwrap()
            .try_into()
            .unwrap();

        // Device C signs an entry citing A's hash — a cross-chain link.
        let stream = scope_stream_id("repo", AccountId::from_bytes([42; 32]), [0x44; 32], "demo/1");
        let cross = entry::sign_entry_from_op_bytes(
            c.local.secret(),
            stream,
            Some(a_genesis_hash),
            5,
            row_op::encode(&row_op::RowOp::Remove {
                table: "t_demo".into(),
                pk: vec![row_op::TypedValue::Text("r_cross".into())],
                spec_version: 1,
            }),
        );

        // It arrives BEFORE A's entry, so nothing yet distinguishes it from a real gap.
        let held = b.ingest_reports(std::slice::from_ref(&cross.signed_bytes), &c.pubkey());
        assert_eq!(held[0].outcome, IngestOutcome::AwaitingPredecessor);
        assert_eq!(b.gapped_count(), 1);

        b.ingest_reports(&genesis, &a.pubkey());
        assert_eq!(
            b.gapped_count(),
            0,
            "accepting the cited entry retires the cross-chain citation, which nothing else would",
        );

        // The OTHER delivery order is decidable on arrival, and must not be retained at all: the
        // cited hash is already held, so the link is provably cross-chain right then. Retaining it
        // would create exactly the row the sweep above exists to clean up.
        let mut fresh = Device::new();
        fresh.ingest_reports(&genesis, &a.pubkey());
        let late = fresh.ingest_reports(std::slice::from_ref(&cross.signed_bytes), &c.pubkey());
        assert_eq!(
            late[0].outcome,
            IngestOutcome::Forked,
            "a citation of an already-held foreign entry is judged on arrival, not held",
        );
        assert_eq!(fresh.gapped_count(), 0);
    }

    /// A held entry citing a REJECTED entry is abandoned even when it is on a different device's
    /// chain.
    ///
    /// This is the case no later event could clean up: the cross-chain sweep fires only on an
    /// ACCEPTED hash, and a rejected sibling's hash is never accepted — so a device-filtered
    /// descendant walk would leave the row held until eviction or a repo purge.
    #[test]
    fn a_foreign_citation_of_a_rejected_entry_is_abandoned_with_it() {
        use crate::entry;
        use crate::table_sync::row_op;

        let mut a = Device::new();
        let c = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'one')", []).unwrap();
        let genesis = a.produce();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'two')", []).unwrap();
        let winner = a.produce();

        let stream = scope_stream_id("repo", AccountId::from_bytes([42; 32]), [0x44; 32], "demo/1");
        let genesis_hash: [u8; 32] = a
            .conn
            .query_row(
                "SELECT entry_hash FROM table_sync_entries ORDER BY lamport LIMIT 1",
                [],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .unwrap()
            .try_into()
            .unwrap();
        let remove = |id: &str| {
            row_op::encode(&row_op::RowOp::Remove {
                table: "t_demo".into(),
                pk: vec![row_op::TypedValue::Text(id.into())],
                spec_version: 1,
            })
        };
        // A's losing sibling, and a citation of it signed by a DIFFERENT device.
        let loser = entry::sign_entry_from_op_bytes(
            a.local.secret(),
            stream,
            Some(genesis_hash),
            50,
            remove("r_loser"),
        );
        let foreign = entry::sign_entry_from_op_bytes(
            c.local.secret(),
            stream,
            Some(loser.entry.entry_hash),
            60,
            remove("r_foreign"),
        );

        b.ingest_reports(&[winner[0].clone(), loser.signed_bytes.clone()], &a.pubkey());
        b.ingest_reports(std::slice::from_ref(&foreign.signed_bytes), &c.pubkey());
        assert_eq!(b.gapped_count(), 3, "all three wait on predecessors");

        b.ingest_reports(&genesis, &a.pubkey());
        assert_eq!(
            b.gapped_count(),
            0,
            "the loser is judged and the OTHER device's citation of it goes too — nothing else \
             could retire that row, since its predecessor is never accepted",
        );
    }

    #[test]
    fn a_row_written_on_one_device_appears_on_the_other_and_never_echoes() {
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();

        let entries = a.produce();
        assert_eq!(entries.len(), 1, "one changed row is authored");
        b.ingest_all(&entries, &a.pubkey());
        assert_eq!(b.title().as_deref(), Some("base"), "the row appears on the peer");

        // The flagship: the peer does not re-emit a row it received.
        assert!(b.produce().is_empty(), "a received row never echoes back");
        // And the author does not re-emit its own already-published row.
        assert!(a.produce().is_empty(), "a published row is not re-authored");
    }

    #[test]
    fn old_incarnation_offer_is_rejected_before_any_chain_or_projection_storage() {
        let mut old = Device::new();
        let mut current = Device::new();
        old.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'old')", []).unwrap();
        let entries = old.produce();

        current
            .conn
            .execute(
                "UPDATE account_repo_incarnation_current SET incarnation_ref = ?1
                  WHERE repository_id = 'repo'",
                [[0x55u8; 32].as_slice()],
            )
            .unwrap();
        enroll_writer(&current.conn, AccountId::from_bytes([42; 32]), old.pubkey().fingerprint());
        let tx = current.conn.transaction().unwrap();
        let ctx = SyncCtx {
            repo_id: "repo",
            account_id: AccountId::from_bytes([42; 32]),
            incarnation_ref: [0x55; 32],
            device: &current.local,
            registry: REGISTRY,
            now_ms: 0,
        };
        let error = ingest(&tx, &ctx, "demo/1", &entries[0], &old.pubkey(), None).unwrap_err();
        assert!(error.to_string().contains("different stream"));
        for table in ["table_sync_entries", "table_sync_gapped_entries", "sync_row_clocks"] {
            let count: i64 = tx
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0, "old history must not enter {table}");
        }
        tx.rollback().unwrap();
    }

    #[test]
    fn a_remote_upsert_does_not_clobber_an_unpublished_local_edit() {
        // The ingest path's half of the unsent-local-work problem. A raw local write does not
        // advance the row clock, so the LWW comparison cannot see it: a remote op simply wins and
        // records its OWN hash as published, after which the producer sees no delta and the local
        // edit is gone with nothing left to author it from.
        //
        // The refold has guarded this since #1002 because it runs at store open with no driver to
        // order it. Ingest has the identical exposure and no guard.
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
        b.ingest_all(&a.produce(), &a.pubkey());
        assert_eq!(b.title().as_deref(), Some("base"), "both devices start converged");

        // B writes locally and does not get to author before A's next entry arrives.
        b.set_title("unsent-B");
        a.set_title("from-A");
        let from_a = a.produce();
        assert_eq!(from_a.len(), 1);
        b.ingest_all(&from_a, &a.pubkey());

        assert_eq!(b.title().as_deref(), Some("unsent-B"), "the unsent local edit survives");
        assert_eq!(
            b.produce().len(),
            1,
            "and is still authorable, so it competes on the merits instead of vanishing",
        );
    }

    #[test]
    fn a_remote_upsert_does_not_resurrect_a_row_deleted_locally_but_not_yet_authored() {
        // The delete half, and the subtler one: the row is GONE, so there is no current state for a
        // comparison to catch — but the surviving published identity is exactly what the producer's
        // `Remove` branch keys on. A remote upsert recreates the row AND re-records its published
        // hash, after which the producer sees no delta and the deletion is undone for good.
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
        b.ingest_all(&a.produce(), &a.pubkey());

        // B deletes locally and does not get to author the removal.
        b.delete_row();
        a.set_title("from-A");
        b.ingest_all(&a.produce(), &a.pubkey());

        assert_eq!(b.title(), None, "the unauthored local deletion survives");
        assert_eq!(b.produce().len(), 1, "and is still authorable, so it reaches peers");
    }

    #[test]
    fn two_devices_with_unsent_edits_converge_through_the_deferral() {
        // Deferring at ingest must not cost convergence — it has to change WHEN an op is applied,
        // never whether the devices agree. Both devices hold an unsent edit, and B receives A's
        // entry before it has authored its own, so B defers.
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
        b.ingest_all(&a.produce(), &a.pubkey());

        a.set_title("from-A");
        b.set_title("from-B");

        // A authors first. B, still holding its own unsent edit, defers rather than clobbering it.
        let from_a = a.produce();
        assert_eq!(b.ingest_all(&from_a, &a.pubkey()), vec![IngestOutcome::Retained(
            crate::table_sync::store::PendingReason::DeferredUnsentEdit.as_db_str()
        )],);
        assert_eq!(b.title().as_deref(), Some("from-B"), "B's edit is intact");

        // B authors, which takes `MAX(lamport) + 1` counting the entry it parked — so B's edit is
        // causally later — and settles that entry in the same pass.
        let from_b = b.produce();
        assert_eq!(from_b.len(), 1);
        a.ingest_all(&from_b, &b.pubkey());

        assert_eq!(
            (a.title().as_deref(), b.title().as_deref()),
            (Some("from-B"), Some("from-B")),
            "both devices converge on the causally-later edit, not on a coin flip",
        );
        for device in [&a, &b] {
            assert_eq!(
                device
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM table_sync_entries WHERE pending_reason IS NOT NULL",
                        [],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                0,
                "and nothing is left outstanding on either side",
            );
        }
        // Steady state: neither device has anything more to say.
        assert!(a.produce().is_empty() && b.produce().is_empty());
    }

    #[test]
    fn a_later_edit_supersedes_across_devices_and_both_converge() {
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
        b.ingest_all(&a.produce(), &a.pubkey());

        // A edits, syncs to B; then B edits (its lamport is now one past A's) and syncs back. B's
        // edit is causally later, so it wins on BOTH devices — a proper Lamport-clock supersede,
        // not a fingerprint coin-flip.
        a.set_title("from-A");
        b.ingest_all(&a.produce(), &a.pubkey());
        assert_eq!(b.title().as_deref(), Some("from-A"), "A's edit reached B");

        b.set_title("from-B");
        a.ingest_all(&b.produce(), &b.pubkey());

        assert_eq!(a.title(), b.title(), "the devices converge");
        assert_eq!(a.title().as_deref(), Some("from-B"), "the causally-later edit wins on both");

        // Steady state: nothing left to produce on either side.
        assert!(a.produce().is_empty());
        assert!(b.produce().is_empty());
    }

    #[test]
    fn a_scope_with_multiple_tables_routes_each_op_to_its_table() {
        const TA: TableSpec = TableSpec {
            name: "t_a",
            scope_id: "multi/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        const TB: TableSpec = TableSpec {
            name: "t_b",
            scope_id: "multi/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        const MULTI: &[TableSpec] = &[TA, TB];

        let setup = || {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
            seed_incarnation(&conn);
            conn.execute_batch(
                "CREATE TABLE t_a(id TEXT PRIMARY KEY, v TEXT) STRICT;
                 CREATE TABLE t_b(id TEXT PRIMARY KEY, v TEXT) STRICT;",
            )
            .unwrap();
            let local = crate::local_device(&conn, 0).unwrap();
            (conn, local)
        };
        let (mut a_conn, a_dev) = setup();
        let (mut b_conn, b_dev) = setup();
        let account = AccountId::from_bytes([42; 32]);

        // A writes a row into each table (both tables share the `multi/1` scope stream).
        a_conn.execute("INSERT INTO t_a(id, v) VALUES ('r', 'in-a')", []).unwrap();
        a_conn.execute("INSERT INTO t_b(id, v) VALUES ('r', 'in-b')", []).unwrap();
        let entries = {
            let tx = a_conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &a_dev,
                registry: MULTI,
                now_ms: 0,
            };
            let e = produce_and_author(&tx, &ctx).unwrap();
            tx.commit().unwrap();
            e
        };
        assert_eq!(entries.len(), 2, "one op per table in the scope");

        // B has folded A's DeviceAdd, so A is an effective writer here (else the #935 gate drops
        // it).
        enroll_writer(&b_conn, account, a_dev.secret().public().fingerprint());
        // B ingests both over the ONE shared scope stream; each must route to its own table.
        {
            let tx = b_conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                incarnation_ref: [0x44; 32],
                device: &b_dev,
                registry: MULTI,
                now_ms: 0,
            };
            for bytes in &entries {
                assert_eq!(
                    ingest(&tx, &ctx, "multi/1", bytes, &a_dev.secret().public(), None)
                        .unwrap()
                        .outcome,
                    IngestOutcome::Applied,
                );
            }
            tx.commit().unwrap();
        }
        let a_val: String =
            b_conn.query_row("SELECT v FROM t_a WHERE id = 'r'", [], |r| r.get(0)).unwrap();
        let b_val: String =
            b_conn.query_row("SELECT v FROM t_b WHERE id = 'r'", [], |r| r.get(0)).unwrap();
        assert_eq!(
            (a_val.as_str(), b_val.as_str()),
            ("in-a", "in-b"),
            "each op landed in its own table"
        );
    }
}
