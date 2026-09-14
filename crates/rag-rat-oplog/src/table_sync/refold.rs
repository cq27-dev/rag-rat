//! The forward-compat refold: replay the entries this store retained but could not project.
//!
//! Storage is gated on the CHAIN, application on the PAYLOAD, so a chain-continuous entry is kept
//! even when this binary cannot project it — an unknown column, an unknown op-kind, an undecodable
//! payload, a table outside this scope. Each such entry is marked pending ([`super::store`]), and
//! this module is what redeems the mark: when a later binary understands more, it replays exactly
//! the outstanding set. Without the replay the payload is unrecoverable, because redelivery
//! short-circuits on `entry_exists` and never reconsiders the op.
//!
//! An entry can also be outstanding for a reason that has nothing to do with understanding: the row
//! it would land on holds LOCAL WORK no peer has seen, or its repository-incarnation authority is
//! unresolved. The two families are marked apart
//! ([`PendingReason::is_deferral`]) because they are redeemed by different events, and therefore
//! need different retry triggers: a version gap clears when the binary widens, which the projector
//! version records; a deferral clears when mutable row or account-authority state changes, which no
//! table-projector version can express. See [`refold_owed`].
//!
//! Two properties keep the replay boring, and both are deliberate:
//!
//! - **It goes through the unmodified [`super::apply::apply_row_op_on_stream`] gates**, with each
//!   entry's ORIGINAL `OpMeta`. No clock bypass, no reordering. That is what makes every
//!   interaction correct for free: a parked entry superseded by a later winner loses the LWW
//!   comparison; one superseded by a delete loses to the tombstone and cannot resurrect the row; a
//!   parked `Remove` needs no winner lookup. A bypass would have to re-derive all of that, and a
//!   bypass that skips the clock comparison is one refactor away from skipping the tombstone
//!   comparison too.
//! - **It is bounded by the pending set**, not the log: the steady state (nothing pending) costs
//!   one indexed probe, so the cost is proportional to what is actually outstanding. A pass owed
//!   ONLY by a deferral narrows further, to the deferral family alone — that trigger fires at every
//!   open while mutable state blocks progress, so it must not drag the rest along.
//!
//! Version discipline mirrors the `/3` content projector: [`refold_stale_table_sync_projections`]
//! is the ONLY writer of the stamps, and a store stamped by a NEWER projector is refused rather
//! than folded down by an older binary. Two versions are stamped, answering different questions —
//! [`TABLE_SYNC_PROJECTOR_VERSION`] for what this binary can PROJECT, and
//! [`TABLE_SYNC_DEFERRAL_VOCABULARY`] for how it CLASSIFIES what it could not.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::apply::{self, ApplyOutcome};
use super::registry::TableSpec;
use super::row_op::{self, DecodedRowOp, RowOp};
use super::store::{self, PendingEntry, PendingReason, Worklist};
use crate::entry;
use crate::op::OpMeta;

/// What this binary can project into synced tables.
///
/// BUMP THIS on any change that widens understanding — a table registered, a column added to a
/// spec, a new row-op kind — because that is exactly when retained entries become projectable and
/// when previously recorded anti-echo hashes stop covering the current column set. Forgetting the
/// bump leaves pending entries unreplayed (data that arrived is never applied); it cannot corrupt,
/// because the per-row `spec_version` still marks stale hashes as not comparable.
///
/// This is NOT left to discipline for the registry-driven cases. It equals the length of
/// [`super::registry::PROJECTOR_GENERATIONS`], whose last entry must match the live registry — so
/// registering a table or widening a spec forces an append, and an append is the bump. A widening
/// that is not a registry change (a new row-op kind) still has to append a generation by hand,
/// repeating the previous snapshot.
pub(crate) const TABLE_SYNC_PROJECTOR_VERSION: i64 = 10;

const TABLE_SYNC_PROJECTOR_VERSION_KEY: &str = "table_sync_projector_version";

/// How this binary CLASSIFIES a retained entry — the [`PendingReason`] vocabulary and, through
/// [`PendingReason::is_deferral`], which retry trigger each reason answers to.
///
/// BUMP THIS whenever a reason changes families, or a new reason lands in a family an existing
/// state used to be recorded under. Deliberately separate from
/// [`TABLE_SYNC_PROJECTOR_VERSION`], which answers a different question — what this binary can
/// PROJECT — and is mechanically pinned to the registry, so it cannot be moved for a
/// classification change and would say the wrong thing if it were.
///
/// It exists because the classification is recorded DURABLY, and a mark left by a binary with an
/// older vocabulary is indistinguishable from one this binary would write. #1005 is the worked
/// example: before it, an entry blocked behind local row state kept whatever reason it was parked
/// with at ingest, so after the upgrade it reads as a version gap at the current version — no
/// trigger owns it, and it is stranded exactly the way #1005 exists to prevent. One full pass
/// re-derives every pending entry's reason under the current vocabulary, which is all it takes.
///
/// One store-global stamp is enough, and it is worth saying why, because "an older binary shares
/// this store and could re-create the legacy state after we stamped" is the obvious objection.
/// There are exactly TWO writers of a pending mark, and neither can:
///
/// - **Ingest** ([`super::engine`]) reaches a version gap only through
///   [`super::apply::PayloadVerdict::Gap`], decided on the payload alone — so the reason it records
///   there means the same thing under every vocabulary, whatever the row holds. (Since #1056 it
///   records deferrals too, but only reasons from THIS vocabulary: a binary predating a vocabulary
///   has no way to name its states, so it cannot write them.)
/// - **[`repark`]** only runs inside a pass, and a pass is gated on [`refold_owed`]. An older
///   binary's triggers are all version-based, so once the projector stamp is current and every
///   pending entry sits at that version, its refold does not run at all — which is precisely the
///   state this stamp is written in.
///
/// So the legacy shape can only be created by a refold that predates this vocabulary, and that
/// refold cannot run again once the vocabulary is stamped. Binding the vocabulary per mark (a
/// migration) would buy nothing over dominating the one path that writes stale classifications.
const TABLE_SYNC_DEFERRAL_VOCABULARY: i64 = 2;

const TABLE_SYNC_DEFERRAL_VOCABULARY_KEY: &str = "table_sync_deferral_vocabulary";

/// Replay every pending entry against this binary's registry, then stamp the projector version.
/// Returns whether a refold ran.
///
/// Call at store open, BEFORE producing: an upgraded binary that produces first will simply emit
/// nothing for rows whose hashes are no longer comparable (safe, by design), but the entries
/// waiting on the new understanding stay unapplied until this runs.
pub fn refold_stale_table_sync_projections(conn: &Connection) -> anyhow::Result<bool> {
    refold_stale_projections_against(conn, super::registry::SYNCABLE_TABLES)
}

/// [`refold_stale_table_sync_projections`] against an explicit registry — the seam engine tests use
/// to exercise synthetic specs without changing the production registry.
pub(crate) fn refold_stale_projections_against(
    conn: &Connection,
    registry: &[TableSpec],
) -> anyhow::Result<bool> {
    // A store mid-migration has no projection state to refold and nothing to stamp against; mirrors
    // the `/3` projector's pre-V070 guard, and runs before the meta read so a bare DB never errors.
    if !projection_state_present(conn)? {
        return Ok(false);
    }
    let owed = refold_owed(conn)?;
    if !owed.any() {
        return Ok(false);
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // `refold_owed` already excludes a newer stamp; re-assert inside the write txn so this stays
    // honest if that predicate ever changes.
    assert_projector_not_newer(&tx)?;
    replay_worklist(&tx, registry, owed.worklist(), &apply::LocalWriterMemo::default())?;
    stamp_fold_versions(&tx)?;
    tx.commit()?;
    Ok(true)
}

/// Replay entries deferred behind LOCAL ROW STATE, inside the CALLER's transaction.
///
/// The producer's counterpart to the store-open pass, and the reason a deferral is a usable state
/// rather than one that only clears on restart: the moment local work is authored, the thing those
/// entries were blocked behind is gone, so this is exactly when they are worth another look.
///
/// Running it here is also what makes "author local before applying remote" hold BY CONSTRUCTION at
/// this seam, instead of being a convention every future driver has to remember. It is deliberately
/// narrowed to the deferral family — authoring says nothing about a version gap — and deliberately
/// does not stamp: it is not a full fold, and claiming one would let a genuine gap go unreplayed.
pub(crate) fn replay_deferred_entries(
    tx: &Transaction<'_>,
    registry: &[TableSpec],
    local_writer: &apply::LocalWriterMemo,
) -> anyhow::Result<()> {
    replay_worklist(tx, registry, Worklist::Deferrals, local_writer)
}

fn replay_worklist(
    tx: &Transaction<'_>,
    registry: &[TableSpec],
    worklist: Worklist,
    local_writer: &apply::LocalWriterMemo,
) -> anyhow::Result<()> {
    for pending in store::pending_entries(tx, worklist)? {
        replay_pending_entry(tx, registry, &pending, local_writer)?;
    }
    Ok(())
}

/// What a refold pass is owed, and therefore how much of the pending set it has to replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RefoldOwed {
    /// Triggers 1 or 2 — this binary may understand more than whatever last evaluated the pending
    /// set, so every entry's outcome is back in question.
    widened: bool,
    /// Trigger 3 — at least one entry is blocked behind mutable row or account-authority state.
    deferrals: bool,
}

impl RefoldOwed {
    const NOTHING: Self = Self { widened: false, deferrals: false };

    fn any(self) -> bool {
        self.widened || self.deferrals
    }

    /// Narrow the pass to the deferral family when a deferral is the ONLY thing owing it.
    ///
    /// Safe because a version gap's outcome is decided on the PAYLOAD ALONE — the version gate and
    /// the projection, neither of which reads row state — so under an unchanged binary replaying
    /// one is guaranteed to re-park it at the same reason. And the stamp stays honest: "trigger 3
    /// alone" means the stamp and every pending entry are already AT this projector version, so
    /// re-stamping claims nothing that was not already claimed.
    fn worklist(self) -> Worklist {
        if self.widened { Worklist::All } else { Worklist::Deferrals }
    }
}

/// Whether a refold is owed. THREE independent triggers, because neither the store-global stamp nor
/// the per-entry one is a complete record of what could have changed:
///
/// 1. **The stamp is behind** — the ordinary upgrade: this binary understands more than whatever
///    last folded the store.
/// 2. **Some entry was evaluated by an OLDER projector than this one**, even though the stamp is
///    current. A shared store reached by binaries of different versions (linked worktrees are a
///    first-class configuration here) produces exactly this: a newer binary stamps the store, an
///    older one then ingests and parks an entry it cannot project, marking it with ITS version. On
///    the stamp alone the newer binary would short-circuit and never replay an entry it fully
///    understands — and redelivery cannot rescue it, because that short-circuits on `entry_exists`.
/// 3. **Some entry is deferred behind MUTABLE STATE** ([`PendingReason::is_deferral`]), at any
///    version. Versions cannot carry this trigger: such an entry is redeemed by a row changing or
///    repository-incarnation authority resolving, which no table-projector stamp records and no
///    binary upgrade implies. Parked by an OLDER binary it is already covered by trigger 2; parked
///    by THIS one it carries the current version, and without this trigger nothing would ever look
///    at it again (#1005). Its cost is bounded by narrowing the pass — see
///    [`RefoldOwed::worklist`].
///
/// Trigger 1 covers the CLASSIFICATION vocabulary as well as the projector version, because a mark
/// written under an older vocabulary is not re-derivable from the mark itself
/// ([`TABLE_SYNC_DEFERRAL_VOCABULARY`]). It owes a FULL pass, not a narrowed one: the entries
/// needing reclassification are exactly the ones the deferral probe cannot yet recognize.
///
/// A newer stamp is never a trigger, for any of the three: an older binary must not re-park what a
/// newer one understood, and must not act on deferral tokens it may not even have names for.
fn refold_owed(conn: &Connection) -> anyhow::Result<RefoldOwed> {
    let stamp_behind = match stored_projector_version(conn)? {
        Some(version) => {
            if version > TABLE_SYNC_PROJECTOR_VERSION {
                return Ok(RefoldOwed::NOTHING);
            }
            version < TABLE_SYNC_PROJECTOR_VERSION
        },
        None => true,
    };
    let vocabulary_behind = stored_meta_i64(conn, TABLE_SYNC_DEFERRAL_VOCABULARY_KEY)?
        .is_none_or(|stored| stored < TABLE_SYNC_DEFERRAL_VOCABULARY);
    let pending = pending_summary(conn)?;
    Ok(RefoldOwed {
        widened: stamp_behind
            || vocabulary_behind
            || pending.oldest_projector_version.is_some_and(|v| v < TABLE_SYNC_PROJECTOR_VERSION),
        deferrals: pending.any_deferral,
    })
}

/// Re-attempt one retained entry under this binary's understanding.
///
/// The stored bytes were signature-verified when they were accepted and have not left this store
/// since, so replay decodes without re-verifying: it is a re-projection of our own log, not a trust
/// decision. (Authority was likewise settled at accept time — the roster gate cannot be re-run
/// here, because a device legitimately removed since then would have its long-accepted entries
/// dropped.)
fn replay_pending_entry(
    tx: &Transaction<'_>,
    registry: &[TableSpec],
    pending: &PendingEntry,
    local_writer: &apply::LocalWriterMemo,
) -> anyhow::Result<()> {
    // The stream id is a ONE-WAY hash of (repo_id, account_id, incarnation_ref, scope_id), so an
    // entry with no directory row cannot be placed at all — there is no repo to apply it to, no
    // incarnation to validate, and no scope to resolve its spec. Record that and leave it: a purged
    // repo's history must not project, and the state is now legible rather than unexplained pending
    // work.
    //
    // Re-parking is what makes it CHEAP, not just legible. The mark is what the entry carried when
    // some older binary last touched it, so without this the entry keeps a stale version forever
    // and trigger 2 is permanently true — an IMMEDIATE transaction and a pending scan at every
    // store open, for an entry that can never be placed. `NoStreamContext` is not a deferral, so
    // once it is stamped at the current version nothing retries it.
    let Some(context) = store::stream_context(tx, pending.stream_id)? else {
        return repark(tx, pending, PendingReason::NoStreamContext);
    };
    let account_id = store::stream_account_id(tx, pending.stream_id)?;
    match crate::account::repo_incarnation_state(tx, account_id, &context.repo_id)? {
        crate::account::RepoIncarnationState::Current(current)
            if current == crate::AccountEntryHash::from_bytes(context.incarnation_ref) => {},
        // Account evidence is non-monotone: a late secrets cut can condemn the apparent successor
        // and restore this stream's reference. A different current reference is therefore no more
        // terminal than absent/contested authority for already-retained history.
        crate::account::RepoIncarnationState::Current(_)
        | crate::account::RepoIncarnationState::Absent
        | crate::account::RepoIncarnationState::Contested =>
            return repark(tx, pending, PendingReason::DeferredIncarnationAuthority),
    }
    // These bytes were signature-verified when accepted and have not left this store since, so a
    // decode failure here means LOCAL corruption. Record it TERMINALLY rather than propagating or
    // re-parking: propagating would roll the whole refold back and re-fail on every future open,
    // and leaving it pending would keep `refold_owed` true forever — an IMMEDIATE transaction and a
    // pending scan at every store open, for bytes no future binary can decode. The entry stays
    // stored as evidence, and the reason is discoverable.
    let Ok(signed) = entry::decode_signed(&pending.signed_bytes) else {
        return store::record_entry_quarantine(
            tx,
            &pending.entry_hash,
            "stored entry bytes no longer decode",
        );
    };
    let op = match row_op::decode(&signed.entry.op_bytes) {
        Ok(DecodedRowOp::Known(op)) => op,
        Ok(DecodedRowOp::Unknown { .. }) =>
            return repark(tx, pending, PendingReason::UnknownOpKind),
        Err(_) => return repark(tx, pending, PendingReason::UndecodablePayload),
    };
    let Some(spec) = registry
        .iter()
        .find(|s| s.scope_id.as_db_str() == context.scope_id && s.name == op.table())
    else {
        return repark(tx, pending, PendingReason::TableNotInScope);
    };
    // NEVER replay over unsent local work. A raw local write does not advance the row clock, so the
    // LWW comparison cannot see it and this older entry would simply win — silently destroying a
    // change no peer has ever seen, at store open, before anything has had a chance to author it.
    // Park it on WHICH state is in the way: once the producer authors that edit (at a lamport above
    // this entry's, since authoring counts parked entries), a later replay lands and loses on the
    // merits — and until then the deferral reason is what brings the entry back for another look,
    // since no version bump can signal that the row moved.
    //
    // `DeferOnAnyDoubt`: nothing ordered a producer before this pass, so a row whose state cannot
    // be established either way is treated as unsafe to write over — on a device that was ever a
    // writer. One that never was has no unsent work and no producer to redeem a deferral, so its
    // local rows never hold a replay back (`RowDoubt::NothingUnsent`).
    let doubt = apply::RowDoubt::for_local_device(
        local_writer,
        tx,
        account_id,
        crate::identity::local_device_fingerprint(tx)?,
        apply::RowDoubt::DeferOnAnyDoubt,
    )?;
    let meta = OpMeta { lamport: signed.entry.lamport, device: signed.entry.device_fingerprint };
    if let apply::PreApply::Park(reason) =
        apply::pre_apply(tx, spec, &context.repo_id, pending.stream_id, &op, meta.lamport, doubt)?
    {
        // A restatement's other deletes settle now rather than wait on one row (see
        // `restate_settleable_now`); the entry stays parked and replays whole later. What the
        // subset materialises is merge state like any other, so it re-arms re-adoption the same
        // way. A constraint failure inside the subset is NOT terminal here: every other stated
        // delete still settled (so the sweep runs), and the deletes the subset excluded are still
        // owed — the whole replay reaches the same terminal verdict once they can settle.
        if let Some(now) =
            apply::restate_settleable_now(tx, spec, &context.repo_id, pending.stream_id, &op)?
        {
            match apply::apply_row_op_on_stream(
                tx,
                spec,
                &context.repo_id,
                pending.stream_id,
                &now,
                meta,
            )? {
                // Only a subset that moved something is swept: the next open finds those
                // deletes settled already and does nothing, so a deferral costs no writes.
                ApplyOutcome::Applied | ApplyOutcome::Quarantined { changed: true, .. } => {
                    super::registry::bump_scope_lanes(tx, &context.scope_id, &context.repo_id)?;
                    rearm_removed_writers(tx, account_id, pending.stream_id, &now, meta)?;
                },
                ApplyOutcome::Superseded
                | ApplyOutcome::Quarantined { changed: false, .. }
                | ApplyOutcome::Unprojectable(_) => {},
            }
        }
        return repark(tx, pending, reason);
    }
    match apply::apply_row_op_on_stream(tx, spec, &context.repo_id, pending.stream_id, &op, meta)? {
        // Folded and CHANGED the projection: advance the Lens lanes the applied table's scope feeds
        // so a row landed by replay (e.g. an entry parked as `NewerSpecVersion` by an older binary,
        // applied here after the upgrade) surfaces in Lens without waiting for an unrelated write.
        // Mirrors the direct-ingest bump; gated on repo registration for the same reason (no
        // phantom `'__unassigned__'` `repo_meta` rows). `scope_lens_metas` returns the
        // exact lane set for the applied entry's scope (memories for anchors/overlay,
        // papertrail for distill), so an unsupported scope bumps nothing.
        ApplyOutcome::Applied => {
            super::registry::bump_scope_lanes(tx, &context.scope_id, &context.repo_id)?;
            // The merge state this replay materialised — a live clock under the signer, or a
            // tombstone identity under each stated device — may belong to a writer removed
            // while the entry sat parked, after the drain for that removal already ran and found
            // nothing. Re-arm that device's EXISTING work row so the drain (which follows the
            // deferred replay in the producer transaction) carries the row under a live chain;
            // a device with no removal fact has no row and gets none.
            rearm_removed_writers(tx, account_id, pending.stream_id, &op, meta)?;
            store::clear_entry_pending(tx, &pending.entry_hash)
        },
        // `Superseded` — outranked by a newer winner, or suppressed by a tombstone — is equally a
        // correct fold and equally not outstanding work, but it did NOT change the projection, so
        // no lane bump: the entry was evaluated and lost on the merits, and no later binary
        // changes that.
        ApplyOutcome::Superseded => store::clear_entry_pending(tx, &pending.entry_hash),
        // Terminal, so it stops being outstanding: a type mismatch or a constraint violation is a
        // BROKEN PRODUCER — the values do not fit the declared column types or the table's
        // constraints, and no future binary makes them fit. (A missing column is NOT this case: it
        // is an older producer, which reports `Unprojectable` and stays on the worklist.) Recorded
        // rather than merely cleared: this path has no caller to return an outcome to, so without a
        // durable reason a rejected payload would be indistinguishable from a projected one.
        ApplyOutcome::Quarantined { why, changed } => {
            // A restatement quarantines on one row's constraint failure AFTER settling every
            // other stated delete, so what those materialised is swept like an applied entry.
            if changed {
                super::registry::bump_scope_lanes(tx, &context.scope_id, &context.repo_id)?;
                rearm_removed_writers(tx, account_id, pending.stream_id, &op, meta)?;
            }
            store::record_entry_quarantine(tx, &pending.entry_hash, &why)
        },
        // Still ahead of us — record which gap, under this version, so the next bump can tell
        // "newly stuck" from "stuck since v1".
        ApplyOutcome::Unprojectable(reason) => repark(tx, pending, reason),
    }
}

/// Re-open the completed re-adoption of every device whose merge state an applied `op` (signed
/// by `meta.device`) may have materialised — the signer's live clock or tombstone, and each
/// identity a restatement names — when that device is no longer an effective writer. See
/// `store::reopen_readoption_work` for what a re-open is and is not.
fn rearm_removed_writers(
    tx: &Transaction<'_>,
    account_id: crate::AccountId,
    stream: crate::stream::StreamId,
    op: &RowOp,
    meta: OpMeta,
) -> anyhow::Result<()> {
    let mut devices = vec![meta.device];
    if let RowOp::Restate { deletes, .. } = op {
        devices.extend(deletes.iter().map(|delete| delete.device));
    }
    devices.sort_unstable();
    devices.dedup();
    for device in devices {
        if !crate::account::device_is_effective_writer(tx, account_id, device)? {
            store::reopen_readoption_work(tx, account_id, device, stream)?;
        }
    }
    Ok(())
}

/// Re-record why an entry is still outstanding, under THIS projector version.
///
/// Skips the write when nothing would change. A steady deferral is re-evaluated at every store open
/// by design (trigger 3), and re-marking it would rewrite an identical row every time — WAL traffic
/// proportional to opens rather than to anything happening.
fn repark(
    tx: &Transaction<'_>,
    pending: &PendingEntry,
    reason: PendingReason,
) -> anyhow::Result<()> {
    if pending.reason == Some(reason)
        && pending.projector_version == Some(TABLE_SYNC_PROJECTOR_VERSION)
    {
        return Ok(());
    }
    store::mark_entry_pending(tx, &pending.entry_hash, reason, TABLE_SYNC_PROJECTOR_VERSION)
}

/// Whether the V093 projection substrate exists yet (the directory is the load-bearing half — no
/// apply context means no replay is possible at all).
fn projection_state_present(conn: &Connection) -> anyhow::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'table_sync_streams'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Refuse to fold — or write into — a store a NEWER projector already folded: an older binary would
/// re-park entries the newer one understood, stamp the version down, and record anti-echo hashes
/// under its narrower column set, all of which the newer binary would then have to distrust.
///
/// Called both by the refold and by the engine's write entry points, so an older binary sharing a
/// store (linked worktrees on different versions are a first-class configuration here) fails loudly
/// instead of quietly degrading what the newer one already understood.
pub(crate) fn assert_projector_not_newer(conn: &Connection) -> anyhow::Result<()> {
    if let Some(stored) = stored_projector_version(conn)?
        && stored > TABLE_SYNC_PROJECTOR_VERSION
    {
        anyhow::bail!(
            "the table-sync projection was folded by a newer rag-rat (table-sync projector \
             v{stored} > v{TABLE_SYNC_PROJECTOR_VERSION}); upgrade to write this store"
        );
    }
    Ok(())
}

/// Record what this pass folded: the projection understanding it applied, and the vocabulary it
/// classified the pending set under.
///
/// The vocabulary is raised, never lowered — an older binary sharing the store must not walk it
/// back and make a newer one redo the reclassification. (The projector version is guarded more
/// strongly, by refusing the pass outright; that guard cannot be reused here, because a store
/// folded by a newer vocabulary is still perfectly foldable by this binary.)
fn stamp_fold_versions(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![TABLE_SYNC_PROJECTOR_VERSION_KEY, TABLE_SYNC_PROJECTOR_VERSION.to_string()],
    )?;
    tx.execute(
        "INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value
             WHERE CAST(excluded.value AS INTEGER) > CAST(oplog_meta.value AS INTEGER)",
        params![TABLE_SYNC_DEFERRAL_VOCABULARY_KEY, TABLE_SYNC_DEFERRAL_VOCABULARY.to_string()],
    )?;
    Ok(())
}

/// The two facts [`refold_owed`]'s triggers 2 and 3 need, read in ONE pass over the partial
/// `pending_reason` index — the same cost the single fact used to take, which matters because this
/// runs at every store open and the steady state (nothing pending) must stay one cheap probe.
struct PendingSummary {
    /// The oldest projector version that evaluated any still-pending entry; `None` when nothing is
    /// pending.
    oldest_projector_version: Option<i64>,
    any_deferral: bool,
}

fn pending_summary(conn: &Connection) -> anyhow::Result<PendingSummary> {
    let deferrals = store::deferral_tokens_sql();
    Ok(conn.query_row(
        &format!(
            "SELECT MIN(pending_projector_version), MAX(pending_reason IN ({deferrals}))
               FROM table_sync_entries
              WHERE pending_reason IS NOT NULL"
        ),
        [],
        |row| {
            Ok(PendingSummary {
                oldest_projector_version: row.get(0)?,
                any_deferral: row.get::<_, Option<bool>>(1)?.unwrap_or(false),
            })
        },
    )?)
}

fn stored_projector_version(conn: &Connection) -> anyhow::Result<Option<i64>> {
    stored_meta_i64(conn, TABLE_SYNC_PROJECTOR_VERSION_KEY)
}

fn stored_meta_i64(conn: &Connection, key: &str) -> anyhow::Result<Option<i64>> {
    conn.query_row("SELECT value FROM oplog_meta WHERE key = ?1", params![key], |row| {
        row.get::<_, String>(0)
    })
    .optional()?
    .map(|value| value.parse::<i64>().with_context(|| format!("oplog {key} is not an integer")))
    .transpose()
}

#[cfg(test)]
#[path = "refold_tests.rs"]
mod tests;
