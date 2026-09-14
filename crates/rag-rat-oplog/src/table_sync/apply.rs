//! The apply pipeline: fold one decoded row op into its table under WHOLE-ROW last-writer-wins.
//!
//! Each row carries one write clock (`sync_row_clocks`). An upsert wins the ENTIRE row iff its
//! `(lamport, device_fingerprint)` beats that clock (higher lamport, ties broken by the smaller
//! fingerprint) — the winner replaces the whole row atomically; a loser is a no-op.
//! Insert-vs-update is decided by row existence, never a blind upsert. A cell whose value disagrees
//! with its column's declared type quarantines the whole op (a broken producer, surfaced, not
//! silently coerced); an op naming a column this binary's registry doesn't know is PARKED whole (a
//! newer producer — see [`apply_upsert`]), never applied in part, so every local row stays a
//! complete after-image some device actually authored. After a winning apply the row's
//! synced-column hash is recorded WITH the projector version whose column set it covers, which is
//! what stops the producer re-emitting a row it just received (see [`super::produce`]).
//! Deletes and the resurrection guard use the same row clock plus a per-row tombstone; a losing op
//! never touches the published hash, so an unsent local edit is never silently marked as sent.
//!
//! A tombstone is merge state (the latest delete of a row, permanent) and the entry that states it
//! is its delivery. The two are kept apart (#1295): `sync_row_tombstones` holds the identity, and
//! `sync_tombstone_statements` holds, per chain that states the current identity, the lamport of
//! that chain's newest statement — the entry retention pins while the deleted row has no live
//! successor. A `Remove` states its own identity; a `Restate` re-carries earlier deletes at their
//! original identities, so the signer's statement moves to the tail and the entry that first
//! stated the delete can be reclaimed. When a newer delete wins the row, the old identity's
//! statements go with it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension, Transaction, params_from_iter};

use super::diagnostics::{self, TableSyncRowCause};
use super::registry::{self, DefaultValue, TableSpec, ValueType};
use super::row_op::{self, Cell, RowOp, StatedDelete, TypedValue};
use super::store::PendingReason;
use crate::op::OpMeta;
use crate::stream::StreamId;

/// The result of applying one row op.
///
/// `Quarantined` means the op was structurally storable but its content is unprojectable in a way a
/// later binary will NOT fix (a type mismatch, a constraint violation, a partial after-image from a
/// broken producer) — the entry is retained, the row is left untouched.
///
/// `Unprojectable` means this binary does not understand the payload YET — a newer producer used a
/// column this registry lacks. Nothing is written and the entry is marked pending, so a later
/// binary that learns the column replays it. The distinction matters: quarantine is terminal,
/// pending is a version gap.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ApplyOutcome {
    Applied,
    /// The op landed but did NOT take effect: a newer write already owns the row, or a tombstone
    /// suppresses it. A correct fold, not outstanding work — receivers treat it exactly like
    /// `Applied`.
    ///
    /// It is separate from `Applied` for ONE caller. A locally-authored op takes the stream's
    /// `MAX(lamport) + 1`, so while a row's bookkeeping belongs to the stream being authored on it
    /// cannot lose — which makes this outcome, at the produce seam, proof that the two have come
    /// apart (a row clock carrying a lamport from another stream). Folded into `Applied` that reads
    /// as settlement while nothing is published, so the producer re-derives the same delta and
    /// re-signs it on every pass.
    Superseded,
    /// Terminal: the payload does not fit the table (a broken producer), or a physical delete
    /// failed on a constraint. `changed` says whether the op moved any merge state before that —
    /// only a `Restate` can, since it settles every other stated delete first — so callers
    /// sweep what it materialised exactly as they would after `Applied`.
    Quarantined {
        why: String,
        changed: bool,
    },
    Unprojectable(PendingReason),
}

/// "No particular stream", for tests that exercise the merge rules without one. Unrelated to
/// [`StreamId::PRECONTEXT`], the persisted re-adoption placeholder that shares its bytes.
#[cfg(test)]
const NO_STREAM: StreamId = StreamId::from_bytes([0; 32]);

/// [`apply_row_op_on_stream`] on [`NO_STREAM`] — the arity the merge-rule tests use.
#[cfg(test)]
pub(crate) fn apply_row_op(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    op: &RowOp,
    meta: OpMeta,
) -> anyhow::Result<ApplyOutcome> {
    apply_row_op_on_stream(tx, spec, repo_id, NO_STREAM, op, meta)
}

/// Fold `op` into `spec`'s table for `repo_id` on `stream`, ordered by `meta`. See the module doc
/// for the merge rules. Every write goes through the caller's transaction, so a partially-applied
/// op cannot leak.
pub(crate) fn apply_row_op_on_stream(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    op: &RowOp,
    meta: OpMeta,
) -> anyhow::Result<ApplyOutcome> {
    debug_assert_eq!(op.table(), spec.name, "caller resolves the spec from the op's table");
    // A restatement carries deletes that were signed BEFORE it, at their own identities; one at
    // or above its own lamport would let a signer mint a delete identity its chain never held.
    // The wire cannot check this (it carries no entry metadata), so it is checked wherever an
    // entry is applied — accept, replay and self-apply all pass through here. Quarantined, not
    // parked: no later binary makes it sound.
    if let RowOp::Restate { deletes, .. } = op
        && deletes.iter().any(|delete| delete.lamport >= meta.lamport)
    {
        return Ok(ApplyOutcome::Quarantined {
            why: format!(
                "restate on `{}` states a delete at or above its own lamport {}",
                spec.name, meta.lamport
            ),
            changed: false,
        });
    }
    let known = match payload_verdict(spec, repo_id, op) {
        PayloadVerdict::Gap(reason) => return Ok(ApplyOutcome::Unprojectable(reason)),
        PayloadVerdict::Rejected(why) =>
            return Ok(ApplyOutcome::Quarantined { why, changed: false }),
        PayloadVerdict::RowDecides(known) => known,
    };
    match (known, op) {
        // A complete after-image: an upsert, whose whole column set the row will take.
        (Some(known), RowOp::Upsert { pk, .. }) =>
            apply_upsert(tx, spec, repo_id, stream, pk, known, meta),
        // Nothing to project — a remove names only the row identity.
        (None, RowOp::Remove { pk, .. }) => apply_remove(tx, spec, repo_id, stream, pk, meta),
        (None, RowOp::Restate { deletes, .. }) =>
            apply_restate(tx, spec, repo_id, stream, deletes, meta),
        (Some(_), RowOp::Remove { .. } | RowOp::Restate { .. }) | (None, RowOp::Upsert { .. }) =>
            unreachable!("payload_verdict pairs an after-image with an upsert only"),
    }
}

/// What [`apply_row_op_on_stream`] decides on the PAYLOAD ALONE, before any row state is read.
#[derive(Debug, PartialEq)]
pub(crate) enum PayloadVerdict {
    /// Nothing in the payload stands in the way; the ROW STATE decides what happens next. Carries
    /// the complete after-image for an upsert, and `None` for a remove or a restate, which name
    /// row identities and no column set.
    RowDecides(Option<Vec<(&'static str, TypedValue)>>),
    /// A version gap: this binary does not understand the payload yet. No row read changes that.
    Gap(PendingReason),
    /// Terminal on its own merits — a broken producer. No row read changes that either.
    Rejected(String),
}

/// Everything [`apply_row_op_on_stream`] can settle without touching the database, in the order it
/// settles it.
///
/// Factored out because it has a SECOND caller with a different question. The refold has to know,
/// before it consults [`unsent_work_blocking_replay`], whether an entry's fate depends on the row
/// at all: a version gap and a terminal payload are both settled here, and filing either as a
/// deferral would move it into the family replayed at every store open, where nothing could redeem
/// it. Two copies of these checks would drift, and the drift would be invisible — each copy reads
/// correctly on its own, and the disagreement only shows up as an entry stuck in the wrong retry
/// family.
pub(crate) fn payload_verdict(spec: &TableSpec, repo_id: &str, op: &RowOp) -> PayloadVerdict {
    // "We do not understand this generation" OUTRANKS "this op looks malformed to us", so the
    // version gate goes before every structural check below — those all return `Quarantined`, which
    // is TERMINAL. A newer producer's UPSERT that happens to trip one of them would be discarded
    // for good rather than parked for the binary that understands it, which is the exact
    // failure this version exists to prevent. The additive-only rule makes that unreachable
    // today (pk shape and types cannot change within a table's life), but that rule is
    // explicitly un-lintable, so it must not be what stands between a recoverable payload and
    // permanent loss.
    //
    // `Remove` is EXCLUDED, and must stay excluded: it names only the row identity, so no column
    // set is involved and there is nothing a later binary would understand better. Parking one
    // would delay a deletion across a version skew for no benefit, and a row deleted after a column
    // change would become permanently undeletable — a convergence wedge.
    if matches!(op, RowOp::Upsert { .. }) && op.spec_version() > spec.spec_version {
        return PayloadVerdict::Gap(PendingReason::NewerSpecVersion);
    }
    // Every identity the op names is checked before anything is written, so a restatement with
    // one malformed element quarantines whole, with no effect.
    for pk_vals in op.pks() {
        if pk_vals.len() != spec.pk.len() {
            return PayloadVerdict::Rejected(format!(
                "pk arity {} does not match `{}`'s {} identity columns",
                pk_vals.len(),
                spec.name,
                spec.pk.len()
            ));
        }
        // A NULL identity value is unaddressable: `WHERE pk = NULL` never matches, so upserts
        // would insert duplicate unreachable rows and removes could never delete them. Reject the
        // whole op.
        if pk_vals.iter().any(|v| matches!(v, TypedValue::Null)) {
            return PayloadVerdict::Rejected(format!(
                "a null primary-key value is not addressable on `{}`",
                spec.name
            ));
        }
        // Validate each pk value against its declared type before it reaches a WHERE clause.
        // SQLite affinity would otherwise coerce a mismatched pk (e.g. `I64(1)` matching a TEXT
        // key `'1'`) onto a different physical row than its type-exact `row_pk` clock identity,
        // splitting the row's bookkeeping and allowing resurrection. (Arity is checked above, so
        // `zip` covers every pk.)
        for (column, value) in spec.pk.iter().zip(pk_vals) {
            if !value_matches(value, column.value_type) {
                return PayloadVerdict::Rejected(format!(
                    "pk column `{}` value does not match its declared type on `{}`",
                    column.name, spec.name
                ));
            }
        }
        // Repo-identity gate: for a table scoped by a pk column, an op naming a different repo
        // than the stream being synced is rejected — a peer cannot write another project's rows
        // through this stream. (The producer already only emits the local repo's rows.)
        if let Some(idx) = spec.repo_pk_index()
            && pk_vals.get(idx) != Some(&TypedValue::Text(repo_id.to_string()))
        {
            return PayloadVerdict::Rejected(format!(
                "op names a different repo than the `{}` stream being synced",
                spec.name
            ));
        }
    }
    match op {
        // A remove names only the row identity, so no column set is involved: its `spec_version` is
        // carried for wire symmetry and diagnostics, never acted on. Gating a deletion on a version
        // skew would delay it for no benefit. A restate names identities the same way.
        RowOp::Remove { .. } | RowOp::Restate { .. } => PayloadVerdict::RowDecides(None),
        // Resolve the payload into the full after-image THIS registry expects, from the payload
        // alone, so the decision is deterministic and idempotent.
        RowOp::Upsert { spec_version, cells, .. } =>
            match project_cells(spec, *spec_version, cells) {
                Projection::Complete(known) => PayloadVerdict::RowDecides(Some(known)),
                Projection::Park(reason) => PayloadVerdict::Gap(reason),
                Projection::Quarantine(why) => PayloadVerdict::Rejected(why),
            },
    }
}

/// Apply a delete at the entry's own identity, stated by the entry itself.
fn apply_remove(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    pk_vals: &[TypedValue],
    meta: OpMeta,
) -> anyhow::Result<ApplyOutcome> {
    let identity = DeleteIdentity { lamport: meta.lamport, device_hex: meta.device.to_string() };
    let effect = settle_delete(tx, spec, repo_id, stream, pk_vals, &identity, &identity)?;
    Ok(match effect.row {
        RowFate::Quarantined(why) => ApplyOutcome::Quarantined { why, changed: false },
        RowFate::Won { .. } => ApplyOutcome::Applied,
        // The tombstone is raised either way, but a delete a newer write outranks did not delete
        // anything — and, crucially, left the published record in place. Say so.
        RowFate::Kept => ApplyOutcome::Superseded,
    })
}

/// Apply a restatement: every stated delete settles at ITS OWN identity exactly as the entry that
/// first stated it did, and the signer's statement of each identity that is current moves to
/// this entry. The batch was validated whole by [`payload_verdict`] before anything is written.
/// A physical delete that fails on a constraint quarantines the ENTRY (retained, never replayed)
/// but not the batch: every other row still settles, since each is correct merge state on its
/// own and nothing would ever restate them here again — the entries that first stated them may
/// be gone from every peer. No savepoint is taken. `Superseded` when nothing changed — every
/// stated delete was already outranked, and the signer already stated what is current — which
/// only the compaction and re-adoption callers can see and which they never produce.
fn apply_restate(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    deletes: &[StatedDelete],
    meta: OpMeta,
) -> anyhow::Result<ApplyOutcome> {
    let signer = DeleteIdentity { lamport: meta.lamport, device_hex: meta.device.to_string() };
    let mut changed = false;
    let mut quarantined = None;
    for delete in deletes {
        let identity =
            DeleteIdentity { lamport: delete.lamport, device_hex: delete.device.to_string() };
        let effect = settle_delete(tx, spec, repo_id, stream, &delete.pk, &identity, &signer)?;
        match effect.row {
            RowFate::Quarantined(why) => {
                quarantined.get_or_insert(why);
            },
            RowFate::Won { deleted } => changed |= deleted || effect.merge_changed,
            RowFate::Kept => changed |= effect.merge_changed,
        }
    }
    Ok(match quarantined {
        Some(why) => ApplyOutcome::Quarantined { why, changed },
        None if changed => ApplyOutcome::Applied,
        None => ApplyOutcome::Superseded,
    })
}

/// A delete's `(lamport, device)` — what it competes under, and what its statements name.
type DeleteIdentity = RowClock;

/// What one delete did to its row.
enum RowFate {
    /// The delete won the row: no write outranks it. `deleted` says whether a physical row went
    /// (a delete of a row already absent — the producer's own `Remove` after a local deletion —
    /// wins with nothing to delete).
    Won { deleted: bool },
    /// A write strictly newer than the delete kept the row.
    Kept,
    /// The physical delete failed on a constraint; the entry is retained, the row untouched.
    Quarantined(String),
}

/// What one delete changed: its row, and whether the merge state (tombstone identity or the
/// signer's statement) moved at all.
struct DeleteEffect {
    row: RowFate,
    merge_changed: bool,
}

/// The one delete decision, run by a `Remove` at its own identity and by a `Restate` once per
/// stated delete: the delete at `identity` wins the row unless the row's write clock is strictly
/// newer (a concurrent later write keeps the row); either way the row's tombstone is raised to the
/// identity if it beats the current one, so a later `Upsert` older than the delete cannot
/// resurrect the row, and `signer`'s statement of a current identity advances to `signer.lamport`.
/// Order-independent: a stale delete arriving after a newer write loses, and an even older insert
/// arriving after the delete is suppressed by the tombstone.
fn settle_delete(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    pk_vals: &[TypedValue],
    identity: &DeleteIdentity,
    signer: &DeleteIdentity,
) -> anyhow::Result<DeleteEffect> {
    let row_pk = &row_op::row_pk_string(pk_vals);
    let key = RowKey { stream, repo_id, table: spec.name, row_pk };
    let survives = match current_row_clock_on_stream(tx, &key)? {
        // A write strictly newer than the delete keeps the row alive.
        Some(stored) => stored.beats(identity),
        // No recorded write — nothing can outrank the delete.
        None => false,
    };
    let row = if survives {
        RowFate::Kept
    } else {
        // Attempt the physical delete BEFORE raising the tombstone. A constraint violation — an FK
        // RESTRICT child row, an `ON DELETE RESTRICT`, a trigger abort — means the remove cannot
        // apply; quarantine it (leaving the tombstone/clock untouched) so the already-stored entry
        // is retained and the chain advances, instead of erroring and rolling back the entry (which
        // would wedge every later entry on that device's chain as a permanent MissingPredecessor).
        let deleted = match delete_row(tx, spec, pk_vals) {
            Ok(deleted) => deleted,
            Err(err) if is_constraint_violation(&err) =>
                return Ok(DeleteEffect {
                    row: RowFate::Quarantined(format!(
                        "remove violates a column constraint on `{}`",
                        spec.name
                    )),
                    merge_changed: false,
                }),
            Err(err) => return Err(err),
        };
        clear_row_clock(tx, &key)?;
        clear_published(tx, &key)?;
        RowFate::Won { deleted }
    };
    // Raise the tombstone only once the remove has actually applied (the row was deleted, or a
    // newer write kept it): the tombstone guards against an older upsert resurrecting the row.
    let merge_changed = raise_tombstone(tx, &key, identity, signer)?;
    Ok(DeleteEffect { row, merge_changed })
}

/// What an op's cells resolve to under THIS registry.
#[derive(Debug, PartialEq)]
enum Projection {
    /// A full after-image: every synced column, in registry order.
    Complete(Vec<(&'static str, TypedValue)>),
    /// Not projectable YET — a version gap a later binary (or a later sender) redeems.
    Park(PendingReason),
    /// Not projectable EVER — the values do not fit the declared types.
    Quarantine(String),
}

/// Resolve an upsert's cells into the complete after-image this registry expects, from the payload
/// alone (#1002). The single place the spec-version rule lives, shared by the applier and by the
/// producer's stale-row comparison — any divergence between those two would be a convergence bug.
///
/// - **Op NEWER than this binary** → park. We cannot know what a later column set means, and the op
///   may name columns we lack. #1001's refold replays it once this binary catches up.
/// - **Op EQUAL** → strict full after-image, as before.
/// - **Op OLDER** → columns it predates are filled from their DECLARED defaults, yielding a
///   complete row. That is what unfreezes older→newer replication.
///
/// The fill window is PER COLUMN — an op is completed only for the columns its own version
/// predates. A column absent from an op old enough to lack it is filled; a column absent from an op
/// whose version already had it is a partial after-image and parks, rather than being invented.
///
/// Whole-row semantics are preserved deliberately: an older producer's winning op resets a
/// newly-added column to its default on every receiver. That is what a whole-row write from a
/// device that does not know the column MEANS — deterministic and convergent. Letting the receiver
/// keep its own value there would be a per-column merge, which this engine does not do.
fn project_cells(spec: &TableSpec, op_spec_version: u32, cells: &[Cell]) -> Projection {
    if op_spec_version > spec.spec_version {
        return Projection::Park(PendingReason::NewerSpecVersion);
    }
    // A cell naming a column we do not know, at or below our own version, is a producer that
    // mis-stamped (most likely a forgotten bump) — park, so the binary that stamps correctly
    // redeems it, rather than quarantining the likeliest operator error terminally.
    for cell in cells {
        let Some(column) = spec.columns.iter().find(|column| column.name == cell.column) else {
            return Projection::Park(PendingReason::UnknownColumn);
        };
        // A cell for a column introduced AFTER the version the op claims. The op contradicts
        // itself — a producer at version V cannot have known a column added at V+1 — so the stamp
        // is wrong, and trusting it would default-fill every column the (understated) version
        // predates, resetting them for every receiver. This is the one half of the advisory stamp a
        // receiver CAN check without registry history, because `in_version` is a fixed historical
        // fact under additive-only evolution. Park, like every other mis-stamp.
        if column.added.is_some_and(|added| op_spec_version < added.in_version) {
            return Projection::Park(PendingReason::MisstampedSpecVersion);
        }
    }
    let mut complete = Vec::with_capacity(spec.columns.len());
    for column in spec.columns {
        match cells.iter().find(|cell| cell.column == column.name) {
            Some(cell) => {
                if !value_matches(&cell.value, column.value_type) {
                    return Projection::Quarantine(format!(
                        "cell `{}` value does not match declared type on `{}`",
                        cell.column, spec.name
                    ));
                }
                complete.push((column.name, cell.value.clone()));
            },
            // Absent. Filled from the declared default only if the op PREDATES THE COLUMN — its
            // own introducing version, not merely the spec's current one. An op stamped at or
            // above that version was obliged to carry the column, so its absence is a partial
            // after-image and parks. (Keying on the spec's current version instead would, once a
            // table reached a third version, silently default a column the op's own version
            // already had — resetting it for every receiver under whole-row LWW.)
            None => match column.added.filter(|added| op_spec_version < added.in_version) {
                Some(added) => complete.push((column.name, default_as_value(added.default))),
                None => return Projection::Park(PendingReason::PartialAfterImage),
            },
        }
    }
    Projection::Complete(complete)
}

fn default_as_value(default: DefaultValue) -> TypedValue {
    match default {
        DefaultValue::Null => TypedValue::Null,
        DefaultValue::Bool(b) => TypedValue::Bool(b),
        DefaultValue::I64(n) => TypedValue::I64(n),
        DefaultValue::Text(text) => TypedValue::Text(text.to_string()),
        DefaultValue::Blob(bytes) => TypedValue::Blob(bytes.to_vec()),
    }
}

/// Apply an upsert whose after-image [`payload_verdict`] has already resolved — every synced column
/// this registry knows, in registry order.
fn apply_upsert(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    pk_vals: &[TypedValue],
    known: Vec<(&'static str, TypedValue)>,
    meta: OpMeta,
) -> anyhow::Result<ApplyOutcome> {
    let row_pk = &row_op::row_pk_string(pk_vals);
    let incoming = RowClock { lamport: meta.lamport, device_hex: meta.device.to_string() };
    let device_hex = &incoming.device_hex;
    let key = RowKey { stream, repo_id, table: spec.name, row_pk };

    // A row deleted at a clock this op cannot beat stays deleted: the delete is newer than this
    // edit, so the edit must not resurrect the row. (Suppressed, but the entry is still stored, so
    // redelivery stays idempotent.)
    if let Some(stored) = current_tombstone(tx, &key)?
        && !incoming.beats(&stored)
    {
        return Ok(ApplyOutcome::Superseded);
    }

    // Whole-row LWW: the op wins the ENTIRE row iff it beats the row's write clock (or the row is
    // new). A losing op is a no-op — it never partially overwrites, and it must not touch the
    // published hash (that would mark an unsent local edit as sent and make the producer drop it).
    let wins = match current_row_clock_on_stream(tx, &key)? {
        Some(stored) => incoming.beats(&stored),
        None => true, // no prior write — this op establishes the row.
    };
    if !wins {
        return Ok(ApplyOutcome::Superseded);
    }

    // The winner replaces the whole row in ONE statement (so a constraint failure can't leave a
    // half-written row), then owns its write clock and published hash. A constraint violation — a
    // NULL in a NOT NULL column, a failed CHECK — means the op's data doesn't fit the table
    // (malformed producer / schema skew); it is quarantined, NOT propagated as an error, so the
    // already-stored entry is retained and the chain advances instead of wedging.
    let write = if row_exists(tx, spec, pk_vals)? {
        // A winner that CHANGES the row's synced columns is a new authored statement, and this
        // store's local resolution of the old one goes with it (`registry::reset_on_upsert`); a
        // winner restating the row already held — a redelivery, a sibling's identical write —
        // leaves it alone.
        let reset = registry::reset_on_upsert(spec);
        let restated = !reset.is_empty()
            && matches!(
                read_synced_cells(tx, spec, pk_vals)?,
                SyncedRow::Cells(ref held)
                    if held.iter().map(|cell| (cell.column.as_str(), &cell.value))
                        .eq(known.iter().map(|(name, value)| (*name, value)))
            );
        update_row(tx, spec, &known, pk_vals, if restated { &[] } else { reset })
    } else {
        insert_row(tx, spec, pk_vals, &known)
    };
    if let Err(err) = write {
        if is_constraint_violation(&err) {
            return Ok(ApplyOutcome::Quarantined {
                why: format!("op violates a column constraint on `{}`", spec.name),
                changed: false,
            });
        }
        return Err(err);
    }
    raise_row_clock(tx, &key, meta.lamport, device_hex)?;

    // Anti-echo: the winning op now owns the whole current row state, so record its synced hash.
    // (A losing op returned above without touching the published hash.)
    if let Some(hash) = synced_row_hash(tx, spec, pk_vals)? {
        record_published(tx, &key, &hash, spec.spec_version)?;
        diagnostics::clear(tx, &key)?;
    }
    Ok(ApplyOutcome::Applied)
}

/// The total order on clocks: `(lamport, device_hex)` beats `(other_lamport, other_device)` iff its
/// lamport is higher, or equal with a lexicographically-smaller fingerprint (fixed-width lowercase
/// hex orders exactly as the raw fingerprint bytes). The one comparison every whole-row LWW
/// decision uses — the row write clock, the tombstone, and remove-vs-edit.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RowClock {
    pub(crate) lamport: u64,
    pub(crate) device_hex: String,
}

impl RowClock {
    fn beats(&self, other: &Self) -> bool {
        self.lamport > other.lamport
            || (self.lamport == other.lamport && self.device_hex < other.device_hex)
    }
}

/// One synced row's coordinates on a stream — the key both LWW clock tables are addressed by.
#[derive(Clone, Copy)]
pub(crate) struct RowKey<'a> {
    pub(crate) stream: StreamId,
    pub(crate) repo_id: &'a str,
    pub(crate) table: &'a str,
    pub(crate) row_pk: &'a str,
}

/// The two whole-row LWW clock tables. They share a shape — `(lamport, device_fingerprint)` per
/// row key, raised only by a clock that [`RowClock::beats`] the stored one — and differ only in
/// what they record: the latest write, or the latest delete.
#[derive(Clone, Copy)]
enum ClockTable {
    Rows,
    Tombstones,
}

impl ClockTable {
    fn sql_table(self) -> &'static str {
        match self {
            Self::Rows => "sync_row_clocks",
            Self::Tombstones => "sync_row_tombstones",
        }
    }
}

/// The stored clock for `key` in `table`, or `None` if absent.
fn stored_clock(
    tx: &Transaction<'_>,
    table: ClockTable,
    key: &RowKey<'_>,
) -> anyhow::Result<Option<RowClock>> {
    let row = tx
        .query_row(
            &format!(
                "SELECT lamport, device_fingerprint FROM {}
                 WHERE stream_id = ?1 AND repo_id = ?2 AND table_name = ?3 AND row_pk = ?4",
                table.sql_table()
            ),
            rusqlite::params![key.stream.to_bytes().as_slice(), key.repo_id, key.table, key.row_pk],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(lamport, device)| {
        Ok(RowClock { lamport: u64::try_from(lamport)?, device_hex: device })
    })
    .transpose()
}

/// Raise `key`'s clock in `table` to `(lamport, device_hex)` under LWW — a clock that does not
/// [`RowClock::beats`] the stored one never lowers it.
fn raise_clock(
    tx: &Transaction<'_>,
    table: ClockTable,
    key: &RowKey<'_>,
    lamport: u64,
    device_hex: &str,
) -> anyhow::Result<()> {
    let incoming = RowClock { lamport, device_hex: device_hex.to_owned() };
    if let Some(stored) = stored_clock(tx, table, key)?
        && !incoming.beats(&stored)
    {
        return Ok(());
    }
    tx.execute(
        &format!(
            "INSERT INTO {}(
                 stream_id, repo_id, table_name, row_pk, lamport, device_fingerprint
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(stream_id, table_name, row_pk)
             DO UPDATE SET lamport = excluded.lamport, device_fingerprint = \
             excluded.device_fingerprint",
            table.sql_table()
        ),
        rusqlite::params![
            key.stream.to_bytes().as_slice(),
            key.repo_id,
            key.table,
            key.row_pk,
            i64::try_from(lamport)?,
            device_hex,
        ],
    )?;
    Ok(())
}

/// The row's latest-write clock, or `None` if it has never been written on this device. Recorded on
/// every write (including an insert-only row, which has no per-column clock), it is what a delete
/// and the anti-echo gate compare against. Fingerprints retain their lowercase hex spelling.
pub(crate) fn current_row_clock_on_stream(
    tx: &Transaction<'_>,
    key: &RowKey<'_>,
) -> anyhow::Result<Option<RowClock>> {
    stored_clock(tx, ClockTable::Rows, key)
}

#[cfg(test)]
fn current_row_clock(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
) -> anyhow::Result<Option<RowClock>> {
    current_row_clock_on_stream(tx, &RowKey { stream: NO_STREAM, repo_id, table, row_pk })
}

/// Raise the row's write clock to `(lamport, device_hex)` under LWW — a later-arriving but older
/// write never lowers it.
fn raise_row_clock(
    tx: &Transaction<'_>,
    key: &RowKey<'_>,
    lamport: u64,
    device_hex: &str,
) -> anyhow::Result<()> {
    raise_clock(tx, ClockTable::Rows, key, lamport, device_hex)
}

pub(crate) fn current_tombstone(
    tx: &Transaction<'_>,
    key: &RowKey<'_>,
) -> anyhow::Result<Option<RowClock>> {
    stored_clock(tx, ClockTable::Tombstones, key)
}

/// Raise the row's tombstone to `identity` under LWW — a lower clock never lowers it — and record
/// `signer`'s statement of it. A newly winning identity replaces the old identity's statements
/// (the deletes they carried are outranked, so nothing needs their entries any more) with the
/// signer's; the same identity restated by `signer` advances that chain's statement, never
/// lowering it. Returns whether anything moved.
fn raise_tombstone(
    tx: &Transaction<'_>,
    key: &RowKey<'_>,
    identity: &DeleteIdentity,
    signer: &DeleteIdentity,
) -> anyhow::Result<bool> {
    let current = current_tombstone(tx, key)?;
    let is_current = current.as_ref().is_some_and(|stored| stored == identity);
    let wins = match &current {
        Some(stored) => identity.beats(stored),
        None => true,
    };
    if wins {
        raise_clock(tx, ClockTable::Tombstones, key, identity.lamport, &identity.device_hex)?;
        tx.execute(
            "DELETE FROM sync_tombstone_statements
              WHERE stream_id = ?1 AND table_name = ?2 AND row_pk = ?3",
            rusqlite::params![key.stream.to_bytes().as_slice(), key.table, key.row_pk],
        )?;
    } else if !is_current {
        return Ok(false);
    }
    let advanced = tx.execute(
        "INSERT INTO sync_tombstone_statements(
             stream_id, repo_id, table_name, row_pk, device_fingerprint, lamport
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(stream_id, table_name, row_pk, device_fingerprint)
         DO UPDATE SET lamport = excluded.lamport
         WHERE excluded.lamport > sync_tombstone_statements.lamport",
        rusqlite::params![
            key.stream.to_bytes().as_slice(),
            key.repo_id,
            key.table,
            key.row_pk,
            signer.device_hex,
            i64::try_from(signer.lamport)?,
        ],
    )?;
    Ok(wins || advanced > 0)
}

/// The lamport at which `signer_hex`'s chain last stated the row's current tombstone, if it
/// states it at all.
#[cfg(test)]
pub(crate) fn statement_on_stream(
    tx: &Transaction<'_>,
    key: &RowKey<'_>,
    signer_hex: &str,
) -> anyhow::Result<Option<u64>> {
    let lamport: Option<i64> = tx
        .query_row(
            "SELECT lamport FROM sync_tombstone_statements
              WHERE stream_id = ?1 AND table_name = ?2 AND row_pk = ?3 AND device_fingerprint = ?4",
            rusqlite::params![key.stream.to_bytes().as_slice(), key.table, key.row_pk, signer_hex],
            |row| row.get(0),
        )
        .optional()?;
    lamport.map(u64::try_from).transpose().map_err(Into::into)
}

/// The row's current synced-column hash (the anti-echo identity), or `None` if the row is absent.
/// Shared by the applier (records it) and the producer (compares against it), so both hash the
/// identical read-back and a received row never re-produces.
pub(crate) fn synced_row_hash(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    pk_vals: &[TypedValue],
) -> anyhow::Result<Option<String>> {
    // Absent and unreadable collapse to `None` HERE, and only here, because this function's one
    // caller is the applier's post-write read-back, where both are equally "there is no hash to
    // record". Neither is reachable at that point — the winner just wrote every synced column from
    // typed cells — and the consequence of being wrong is bounded: an unrecorded hash makes the
    // producer reconsider the row, not corrupt it.
    Ok(match read_synced_cells(tx, spec, pk_vals)? {
        SyncedRow::Cells(cells) => Some(row_op::cells_hash(&cells)),
        SyncedRow::Absent | SyncedRow::Unreadable(_) => None,
    })
}

/// What a read of a row's synced columns found.
///
/// `Unreadable` is NOT interchangeable with `Absent`, and conflating them is a real bug rather than
/// a tidiness point: the refold's guard reads an absent row as a local delete awaiting authorship
/// and refuses to replay over it, which for a row that merely cannot be mapped back to its declared
/// types would block that entry for good.
pub(crate) enum SyncedRow {
    /// No row carries this pk.
    Absent,
    /// The row exists but at least one synced column has no value of its declared type (see
    /// [`ReadCell`]), so the row has no comparable hash and cannot be carried in an op.
    Unreadable(String),
    Cells(Vec<Cell>),
}

/// Read a row's synced columns as typed cells (in the registry's column order), mapping each stored
/// value back to its declared type so the hash matches what the applier wrote.
pub(crate) fn read_synced_cells(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    pk_vals: &[TypedValue],
) -> anyhow::Result<SyncedRow> {
    let select = spec.columns.iter().map(|c| quote_ident(c.name)).collect::<Vec<_>>().join(", ");
    let sql =
        format!("SELECT {select} FROM {} WHERE {} LIMIT 1", quote_ident(spec.name), pk_where(spec));
    let row = tx
        .query_row(&sql, params_from_iter(pk_params(pk_vals)), |row| {
            let mut cells = Vec::with_capacity(spec.columns.len());
            for (idx, column) in spec.columns.iter().enumerate() {
                match read_typed(row, idx, column.value_type)? {
                    ReadCell::Value(value) =>
                        cells.push(Cell { column: column.name.to_string(), value }),
                    ReadCell::Malformed(why) =>
                        return Ok(SyncedRow::Unreadable(format!("`{}`: {why}", column.name))),
                }
            }
            Ok(SyncedRow::Cells(cells))
        })
        .optional()?;
    Ok(row.unwrap_or(SyncedRow::Absent))
}

/// One row of the producer's scan. The two unreadable cases are split because the producer must
/// treat them DIFFERENTLY, and getting that wrong deletes data: a row it does not see at all reads
/// as a local delete, and the producer authors a `Remove` for it that removes it from every peer.
pub(crate) enum ScannedRow {
    /// Fully readable: emit it, or skip it if it is already published unchanged.
    Readable { pk: Vec<TypedValue>, cells: Vec<Cell> },
    /// A synced column is unreadable ([`SyncedRow::Unreadable`]), so the row cannot be carried in
    /// an op — but it is still addressable and still LIVE, and its identity has to count as
    /// such.
    Unpublishable { pk: Vec<TypedValue> },
    /// A PK column is unreadable, so the row cannot be named at all. No identity to keep alive: as
    /// far as the pk that was published is concerned, nothing carries it any more, which is the
    /// same thing the row having been deleted means.
    Unaddressable,
}

/// Every current row of `spec`'s table FOR `repo_id`, the producer's scan input. A repo-scoped
/// table is filtered by its `repo_column`, so a multi-repo store never emits one repo's rows into
/// another repo's stream. Both pk values and synced cells are read by their DECLARED type — a
/// `Bool` pk is stored as INTEGER 0/1, and reading it as `I64` would emit a `TypedValue` the
/// applier's typed-pk check rejects, so the producer would sign ops its own self-apply quarantines.
pub(crate) fn read_all_rows(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
) -> anyhow::Result<Vec<ScannedRow>> {
    let pk_select = spec.pk.iter().map(|c| quote_ident(c.name));
    let col_select = spec.columns.iter().map(|c| quote_ident(c.name));
    let select = pk_select.chain(col_select).collect::<Vec<_>>().join(", ");
    let (where_sql, bind): (String, Vec<SqlValue>) = match spec.repo_column {
        Some(col) =>
            (format!(" WHERE {} = ?", quote_ident(col)), vec![SqlValue::Text(repo_id.to_string())]),
        None => (String::new(), Vec::new()),
    };
    let sql = format!("SELECT {select} FROM {}{where_sql}", quote_ident(spec.name));
    let mut stmt = tx.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(bind), |row| {
            let mut pk = Vec::with_capacity(spec.pk.len());
            for (idx, column) in spec.pk.iter().enumerate() {
                match read_typed(row, idx, column.value_type)? {
                    ReadCell::Value(value) => pk.push(value),
                    ReadCell::Malformed(_) => return Ok(ScannedRow::Unaddressable),
                }
            }
            let mut cells = Vec::with_capacity(spec.columns.len());
            for (offset, column) in spec.columns.iter().enumerate() {
                match read_typed(row, spec.pk.len() + offset, column.value_type)? {
                    ReadCell::Value(value) =>
                        cells.push(Cell { column: column.name.to_string(), value }),
                    ReadCell::Malformed(_) => return Ok(ScannedRow::Unpublishable { pk }),
                }
            }
            Ok(ScannedRow::Readable { pk, cells })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ── row writes ───────────────────────────────────────────────────────────────────────────────

fn row_exists(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    pk_vals: &[TypedValue],
) -> anyhow::Result<bool> {
    let sql = format!("SELECT 1 FROM {} WHERE {} LIMIT 1", quote_ident(spec.name), pk_where(spec));
    Ok(tx.query_row(&sql, params_from_iter(pk_params(pk_vals)), |_| Ok(())).optional()?.is_some())
}

fn insert_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    pk_vals: &[TypedValue],
    winning: &[(&'static str, TypedValue)],
) -> anyhow::Result<()> {
    let mut columns: Vec<String> = spec.pk.iter().map(|c| quote_ident(c.name)).collect();
    let mut values: Vec<SqlValue> = pk_vals.iter().map(sql_value).collect();
    for (name, value) in winning {
        columns.push(quote_ident(name));
        values.push(sql_value(value));
    }
    let placeholders = (0..values.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "INSERT INTO {}({}) VALUES ({placeholders})",
        quote_ident(spec.name),
        columns.join(", ")
    );
    tx.execute(&sql, params_from_iter(values))?;
    Ok(())
}

/// Replace an existing row's synced columns in ONE statement (whole-row), so a constraint failure
/// is atomic — never a half-written row. `reset` names local columns nulled in the same statement
/// (see `registry::reset_on_upsert`), except on rows the spec's `reset_on_upsert_keeps` predicate
/// selects.
fn update_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    cells: &[(&'static str, TypedValue)],
    pk_vals: &[TypedValue],
    reset: &[&str],
) -> anyhow::Result<()> {
    let keep = registry::reset_on_upsert_keeps(spec);
    let assignments = cells
        .iter()
        .map(|(name, _)| format!("{} = ?", quote_ident(name)))
        .chain(reset.iter().map(|name| {
            let name = quote_ident(name);
            match keep {
                Some(keep) => format!("{name} = IIF({keep}, {name}, NULL)"),
                None => format!("{name} = NULL"),
            }
        }))
        .collect::<Vec<_>>()
        .join(", ");
    let sql =
        format!("UPDATE {} SET {assignments} WHERE {}", quote_ident(spec.name), pk_where(spec));
    let mut params: Vec<SqlValue> = cells.iter().map(|(_, value)| sql_value(value)).collect();
    params.extend(pk_vals.iter().map(sql_value));
    tx.execute(&sql, params_from_iter(params))?;
    Ok(())
}

/// Whether an error is a SQLite constraint violation (NOT NULL, CHECK, …) — an op whose data does
/// not fit the table, which the applier quarantines rather than propagating as a fatal error.
fn is_constraint_violation(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// Delete the row, reporting whether one was there.
fn delete_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    pk_vals: &[TypedValue],
) -> anyhow::Result<bool> {
    let sql = format!("DELETE FROM {} WHERE {}", quote_ident(spec.name), pk_where(spec));
    Ok(tx.execute(&sql, params_from_iter(pk_params(pk_vals)))? > 0)
}

// ── row clock + published-row bookkeeping ────────────────────────────────────────────────────

fn clear_row_clock(tx: &Transaction<'_>, key: &RowKey<'_>) -> anyhow::Result<()> {
    tx.execute(
        "DELETE FROM sync_row_clocks
          WHERE stream_id = ?1 AND repo_id = ?2 AND table_name = ?3 AND row_pk = ?4",
        rusqlite::params![key.stream.to_bytes().as_slice(), key.repo_id, key.table, key.row_pk],
    )?;
    Ok(())
}

/// The row's recorded anti-echo hash and the projector version whose column set it covers, or
/// `None` if the producer has not published it yet. Read by [`super::produce`].
///
/// The version is NOT decoration. `cells_hash` hashes the cell LIST over `spec.columns` of
/// whichever binary computed it, so a bare hash means "this row under column set C" with C
/// implicit. Comparing hashes across different column sets is meaningless — they differ
/// structurally even when the row is untouched — so the producer must know which set a stored hash
/// covers before trusting a mismatch as a local change.
pub(crate) fn published_hash_on_stream(
    tx: &Transaction<'_>,
    key: &RowKey<'_>,
) -> anyhow::Result<Option<(String, u32)>> {
    let row = tx
        .query_row(
            "SELECT synced_hash, spec_version FROM sync_published_rows
             WHERE stream_id = ?1 AND repo_id = ?2 AND table_name = ?3 AND row_pk = ?4",
            rusqlite::params![key.stream.to_bytes().as_slice(), key.repo_id, key.table, key.row_pk],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    row.map(|(hash, version)| Ok((hash, u32::try_from(version)?))).transpose()
}

#[cfg(test)]
pub(crate) fn published_hash(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
) -> anyhow::Result<Option<(String, u32)>> {
    published_hash_on_stream(tx, &RowKey { stream: NO_STREAM, repo_id, table, row_pk })
}

/// How a row whose published record predates the current spec version compares against the op that
/// actually established it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StaleRow {
    /// The row still matches its winning op, projected under the current spec — untouched since it
    /// landed, so its bookkeeping can simply be restamped.
    Unchanged,
    /// The row differs from its winning op: a local change nothing has authored yet.
    LocallyChanged,
    /// Nothing can be concluded — the winning entry is gone, or does not project here.
    Unknown(TableSyncRowCause),
}

/// Compare a stale-version row against its own winning op, projected under the CURRENT spec.
/// Persists or clears its local diagnostic observation in the caller's transaction; not read-only.
///
/// This is what lets a column-set change resolve instead of freezing. The published hash and the
/// current hash cover different cell lists, so comparing them proves nothing — but the row's
/// WINNING ENTRY is the exact op that produced the row, and projecting it under today's registry
/// (filling columns it predates from their declared defaults) yields what the row SHOULD look like
/// now. Equal means untouched; different means a genuine local change.
///
/// Deliberately a comparison and NOT a re-apply: re-applying the winner through the LWW gates
/// writes nothing (its clock equals the row's by definition, so it never beats it), and bypassing
/// those gates to force it would overwrite exactly the local changes this exists to detect.
pub(crate) fn stale_row_disposition(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    pk_vals: &[TypedValue],
    current: &[Cell],
) -> anyhow::Result<StaleRow> {
    let outcome = compare_stale_row(tx, spec, repo_id, stream, pk_vals, current)?;
    let row_pk = row_op::row_pk_string(pk_vals);
    let key = RowKey { stream, repo_id, table: spec.name, row_pk: &row_pk };
    match outcome {
        StaleRow::Unknown(cause) => diagnostics::record(tx, &key, cause)?,
        StaleRow::Unchanged | StaleRow::LocallyChanged =>
            diagnostics::clear_observed(tx, &key, true)?,
    }
    Ok(outcome)
}

fn compare_stale_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    pk_vals: &[TypedValue],
    current: &[Cell],
) -> anyhow::Result<StaleRow> {
    let row_pk = row_op::row_pk_string(pk_vals);
    let Some(clock) = current_row_clock_on_stream(tx, &RowKey {
        stream,
        repo_id,
        table: spec.name,
        row_pk: &row_pk,
    })?
    else {
        return Ok(StaleRow::Unknown(TableSyncRowCause::MissingClock));
    };
    let op = match super::store::winning_entry_op(tx, stream, &clock.device_hex, clock.lamport)? {
        Ok(op) => op,
        Err(cause) => return Ok(StaleRow::Unknown(cause)),
    };
    // The entry is located by `(stream, device, lamport)`, which identifies it uniquely WITHIN a
    // stream — so the hit is this row's op only while the row's clock and the stream being queried
    // belong to the same stream. That holds today, but it is not an enforced property: a table's
    // stream is derived from `(repo_id, account_id, incarnation_ref, scope_id)`, and moving a
    // registered table to a different scope is an ordinary registry edit. The row's clock would
    // then carry a lamport allocated on the OLD stream, and the same `(device, lamport)` on the new
    // one belongs to some SIBLING table's op. Verify the identity rather than trusting the
    // derivation: a mismatch must read as "cannot resolve" (and be handled conservatively), never
    // as a verdict about this row.
    // Without this, two tables with coincidentally similar columns can project `Complete` and
    // return `Unchanged` for a row that actually holds an unsent edit — which lets the refold
    // replay straight over it.
    // A winning REMOVE clears the row clock, so a live clock can only ever point at an upsert.
    let RowOp::Upsert { spec_version, cells, pk, .. } = &op else {
        return Ok(StaleRow::Unknown(TableSyncRowCause::WrongOperation));
    };
    if op.table() != spec.name {
        return Ok(StaleRow::Unknown(TableSyncRowCause::WrongTable));
    }
    if pk != pk_vals {
        return Ok(StaleRow::Unknown(TableSyncRowCause::WrongKey));
    }
    match project_cells(spec, *spec_version, cells) {
        Projection::Complete(projected) => {
            let as_cells: Vec<Cell> = projected
                .into_iter()
                .map(|(column, value)| Cell { column: column.to_string(), value })
                .collect();
            Ok(if row_op::cells_hash(&as_cells) == row_op::cells_hash(current) {
                StaleRow::Unchanged
            } else {
                StaleRow::LocallyChanged
            })
        },
        Projection::Park(_) | Projection::Quarantine(_) =>
            Ok(StaleRow::Unknown(TableSyncRowCause::UnprojectableWinner)),
    }
}

/// The unsent local work that blocks replaying `op` over this row, or `None` when the replay is
/// safe. Every reason it returns is a [`PendingReason::is_deferral`]: the entry is waiting on the
/// ROW, not on a later binary, so naming which one is what lets the refold retry it at the right
/// time (#1005) instead of leaving it silently skipped.
///
/// The question is asked about the OP and not only the row, because the two kinds have different
/// floors when the row's state cannot be established: an `Upsert` rewrites every synced column and
/// can therefore repair a row, while a `Remove` deletes it outright, local-only columns included,
/// and repairs nothing. See the `Unreadable` arm.
///
/// A raw local write does not advance `sync_row_clocks` — only authoring-and-self-applying does —
/// so the ordinary LWW comparison cannot see an unsent edit at all: it compares the incoming op
/// against the clock of whatever was last *published*, and happily wins. The live ingest path
/// accepts that exposure because the driver's contract is to author local rows before applying
/// remote ones; the refold has no such driver, and runs at store open where an edit made just
/// before the last exit is exactly what is sitting here.
///
/// Reports only what it can PROVE, which is what keeps it useful: a cross-column-set hash
/// comparison is meaningless (the hashes cover different cell lists), so a row published by an
/// older projector is NOT called unsent — treating it as such would make the refold skip precisely
/// the rows it exists to repair. Those rows are in the ordinary last-writer-wins regime, where the
/// refold behaves exactly as live ingest does and the driver's author-before-apply ordering
/// governs.
pub(crate) fn unsent_work_blocking_replay(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    op: &RowOp,
) -> anyhow::Result<Option<PendingReason>> {
    match op {
        RowOp::Upsert { pk, .. } => unsent_work_on_row(tx, spec, repo_id, stream, pk, false),
        RowOp::Remove { pk, .. } => unsent_work_on_row(tx, spec, repo_id, stream, pk, true),
        // A restatement is asked row by row, and only about the rows it would physically remove:
        // a stated delete whose row is absent, or whose clock beats it, changes no row (it may
        // still raise or leave the tombstone and advance the statement) and can never park the
        // entry — so the refold's "defer on any doubt" cannot re-park a restate on rows it would
        // not touch. Whether the tombstone table already holds the stated identity is irrelevant:
        // a locally recreated, unpublished row under an existing tombstone is still a row the
        // delete would destroy, and it gets the exact protection a `Remove` of it would.
        RowOp::Restate { deletes, .. } => {
            // Every row is asked, and a PROVEN blocker outranks an unprovable one: the batch is
            // exempted or deferred as a whole, so the first unprovable verdict must not hide a
            // later row's genuine unsent edit behind the removal exemption.
            let mut unprovable = None;
            for delete in deletes {
                if !delete_would_remove_row(tx, spec, repo_id, stream, delete)? {
                    continue;
                }
                match unsent_work_on_row(tx, spec, repo_id, stream, &delete.pk, true)? {
                    Some(reason) if reason.is_proven_unsent_work() => return Ok(Some(reason)),
                    Some(reason) => unprovable.get_or_insert(reason),
                    None => continue,
                };
            }
            Ok(unprovable)
        },
    }
}

/// Whether a stated delete would physically remove its row here: the row is present and no write
/// clock beats the delete (ties included; a row with no clock counts). The same test
/// [`settle_delete`] applies, asked before anything is written.
///
/// An ABSENT row is never "would remove", so a restatement skips the `DeferredUnsentDelete`
/// protection a `Remove` gets for a row deleted locally but not yet authored: the stated delete
/// then clears that row's published record. That converges — the row ends deleted either way,
/// and the local producer's own `Remove` had nothing left to say — and it is what keeps a
/// restatement from parking on rows it would not touch.
fn delete_would_remove_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    delete: &StatedDelete,
) -> anyhow::Result<bool> {
    if delete.pk.len() != spec.pk.len() || !row_exists(tx, spec, &delete.pk)? {
        return Ok(false);
    }
    let row_pk = row_op::row_pk_string(&delete.pk);
    let key = RowKey { stream, repo_id, table: spec.name, row_pk: &row_pk };
    Ok(match current_row_clock_on_stream(tx, &key)? {
        Some(stored) => !stored
            .beats(&RowClock { lamport: delete.lamport, device_hex: delete.device.to_string() }),
        None => true,
    })
}

/// The part of a `Restate` that can settle NOW while the rest waits: every stated delete that
/// would not physically remove a row, or whose row holds no unsent work. `None` for any other op
/// kind or when nothing in the batch is settleable. A parked entry is replayed whole later, and
/// [`settle_delete`] is idempotent, so applying this subset first and parking the entry is safe —
/// and it is what keeps one row's unsent edit from holding hundreds of unrelated deletes (and,
/// through the pending clamp, the chain's floor) behind it.
pub(crate) fn restate_settleable_now(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    op: &RowOp,
) -> anyhow::Result<Option<RowOp>> {
    let RowOp::Restate { table, spec_version, deletes } = op else {
        return Ok(None);
    };
    let mut settleable = Vec::with_capacity(deletes.len());
    for delete in deletes {
        if !delete_would_remove_row(tx, spec, repo_id, stream, delete)?
            || unsent_work_on_row(tx, spec, repo_id, stream, &delete.pk, true)?.is_none()
        {
            settleable.push(delete.clone());
        }
    }
    Ok((!settleable.is_empty() && settleable.len() < deletes.len()).then(|| RowOp::Restate {
        table: table.clone(),
        spec_version: *spec_version,
        deletes: settleable,
    }))
}

/// [`unsent_work_blocking_replay`] for one row: `removing` says whether the op deletes the row
/// outright (a `Remove`, or a stated delete that would remove it) rather than rewriting it.
fn unsent_work_on_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    pk_vals: &[TypedValue],
    removing: bool,
) -> anyhow::Result<Option<PendingReason>> {
    // A malformed key never reached `apply_row_op_on_stream`'s arity check (an entry parked as
    // out-of-scope or unknown-kind was never validated), and binding it against `spec.pk`'s
    // placeholders would be a parameter-count ERROR — which, propagating out of the refold,
    // would roll back the transaction and fail every subsequent store open on the same entry.
    // Defer to the normal path, which quarantines it.
    if pk_vals.len() != spec.pk.len() {
        return Ok(None);
    }
    let row_pk = row_op::row_pk_string(pk_vals);
    let current_cells = match read_synced_cells(tx, spec, pk_vals)? {
        SyncedRow::Cells(cells) => cells,
        SyncedRow::Absent => {
            diagnostics::clear_observed(
                tx,
                &RowKey { stream, repo_id, table: spec.name, row_pk: &row_pk },
                false,
            )?;
            // No row — but a surviving published identity means the row was DELETED locally and not
            // yet authored. That is precisely what the producer's `Remove` branch keys on, so
            // replaying an upsert here would recreate the row and discard the unsent deletion for
            // good.
            return Ok(published_hash_on_stream(tx, &RowKey {
                stream,
                repo_id,
                table: spec.name,
                row_pk: &row_pk,
            })?
            .is_some()
            .then_some(PendingReason::DeferredUnsentDelete));
        },
        // The row is there but has no hash, so nothing about it can be PROVEN either way — and what
        // to do about that is NOT the same for the two op kinds.
        //
        // An `Upsert` may replay. This verdict has two readers that must not both defer, and the
        // producer cannot author an unreadable row either ([`ScannedRow::Unpublishable`]), so
        // answering "there may be an unsent edit" for every op would leave the row unauthorable AND
        // permanently block its own pending entries, with no way out. The upsert has a floor: it
        // still has to win the ordinary clock comparison, and a winner rewrites every synced
        // column, which is the only thing that makes the row syncable again.
        //
        // A `Remove` has no such floor. It deletes the row outright — local-only columns included —
        // and repairs nothing, so a winning remove would destroy an unsent local edit that merely
        // happens to be unreadable. Deferring it is the safe stuck state: the row survives, and the
        // entry replays on the merits once the cell is repaired.
        SyncedRow::Unreadable(_) => {
            diagnostics::record(
                tx,
                &RowKey { stream, repo_id, table: spec.name, row_pk: &row_pk },
                TableSyncRowCause::UnreadableRow,
            )?;
            return Ok(removing.then_some(PendingReason::DeferredUnreadableRow));
        },
    };
    let current = row_op::cells_hash(&current_cells);
    Ok(
        match published_hash_on_stream(tx, &RowKey {
            stream,
            repo_id,
            table: spec.name,
            row_pk: &row_pk,
        })? {
            // Comparable: a differing hash is a demonstrably unsent local change.
            Some((published, version)) if version == spec.spec_version => {
                diagnostics::clear_observed(
                    tx,
                    &RowKey { stream, repo_id, table: spec.name, row_pk: &row_pk },
                    false,
                )?;
                (published != current).then_some(PendingReason::DeferredUnsentEdit)
            },
            // Published under a different column set, so the hashes cannot be compared — but the
            // row's WINNING op can be, projected under this spec. This proof path is
            // required, not an optimization: once an older-spec op can be filled from
            // declared defaults (#1002) a parked entry can WIN over an unsent raw edit
            // here, where before it could not apply at all. Unprovable stays
            // conservative: refuse to replay rather than risk overwriting.
            Some(_) =>
                match stale_row_disposition(tx, spec, repo_id, stream, pk_vals, &current_cells)? {
                    StaleRow::LocallyChanged => Some(PendingReason::DeferredUnsentEdit),
                    StaleRow::Unknown(_) => Some(PendingReason::DeferredUnresolvedWinner),
                    StaleRow::Unchanged => None,
                },
            // A live row no apply ever published is purely local: the only content there came from
            // this device, and no peer has seen it.
            None => {
                diagnostics::clear_observed(
                    tx,
                    &RowKey { stream, repo_id, table: spec.name, row_pk: &row_pk },
                    false,
                )?;
                Some(PendingReason::DeferredUnsentEdit)
            },
        },
    )
}

/// How much doubt a caller acts on when the row's state cannot be established either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowDoubt {
    /// Defer on ANY doubt. The refold runs at store open, with no driver to have authored local
    /// work first, so an unprovable row is treated as unsafe to write over.
    DeferOnAnyDoubt,
    /// Defer on any doubt EXCEPT an unprovable verdict against a `Remove`, which is applied.
    ///
    /// The live ingest path takes this, and the exemption is deliberately that narrow. Holding a
    /// deletion back on a verdict that may never resolve is the convergence wedge
    /// [`apply_row_op_on_stream`] keeps `Remove` clear of — a row deleted after a column change
    /// would become undeletable across the skew.
    ///
    /// An `Upsert` gets NO exemption. It is already version-gated, so deferring one costs nothing a
    /// skew was not costing anyway, and applying it on an unprovable verdict destroys exactly the
    /// unsent edit this guard exists to protect. The confidence split alone is the wrong axis: it
    /// is the OP KIND that decides whether the convergence argument applies at all.
    DeferExceptUnprovableRemoval,
    /// Nothing here is unsent, because nothing here can ever be sent: the local device has never
    /// been enrolled on the account as a writer (a read-only enrolment, a store with no identity
    /// yet). Whatever it holds locally — a raw edit, a row a migration seeded, a summary it
    /// regenerated for itself — is derived state the writers' rows override, and a received row
    /// is applied on the merits. Holding it back would park every update to that row for good:
    /// the deferral's only redeemer is the producer, and a device that cannot author has none.
    ///
    /// "Never enrolled" (per the stored account log), not "not currently effective": applying
    /// over local state cannot be undone, and effectiveness is a projection a contested roster
    /// fold rebuilds without the device — a removed or transiently-uneffective writer keeps the
    /// guard, since what it edited can still be published once it authors again.
    NothingUnsent,
}

impl RowDoubt {
    /// The doubt a caller acts on for a row received on `account`: `when_writer` if the local
    /// device (`None` on a store without an identity) was ever enrolled there as a writer, else
    /// [`RowDoubt::NothingUnsent`].
    pub(crate) fn for_local_device(
        memo: &LocalWriterMemo,
        tx: &Transaction<'_>,
        account: crate::AccountId,
        device: Option<crate::op::DeviceFingerprint>,
        when_writer: RowDoubt,
    ) -> anyhow::Result<RowDoubt> {
        let writer = match device {
            Some(device) => memo.ever_writer(tx, account, device)?,
            None => false,
        };
        Ok(if writer { when_writer } else { RowDoubt::NothingUnsent })
    }
}

/// "Was the local device ever a writer on this account", memoised across the entries of a sync
/// pass. On a device that never was a writer the roster projection is silent by construction, so
/// the answer comes from a walk of the stored control log, which must not run once per received
/// row: a sync session ingests one entry per call, so the session's table store owns the memo
/// and hands a clone (shared) to each call, and the refold pass owns one of its own.
///
/// The memo cannot go stale. The control log is append-only, so its row count for the account
/// is a version of everything the answer depends on: a cached `false` is reused only while the
/// count is unchanged (one indexed count per lookup), and re-derived otherwise. That covers an
/// enrolment ingested on ANOTHER connection while this pass is in flight — the resident host
/// accepts account and table sessions concurrently — and it is what lets a `true` be reused
/// without checking at all, since it can only ever have been made truer.
#[derive(Clone, Default)]
pub struct LocalWriterMemo(
    Arc<Mutex<HashMap<(crate::AccountId, crate::op::DeviceFingerprint), Answer>>>,
);

/// A memoised answer and the control-log length it was derived at.
#[derive(Clone, Copy)]
struct Answer {
    control_len: u64,
    writer: bool,
}

impl LocalWriterMemo {
    pub(crate) fn ever_writer(
        &self,
        tx: &Transaction<'_>,
        account: crate::AccountId,
        device: crate::op::DeviceFingerprint,
    ) -> anyhow::Result<bool> {
        let mut memo = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let known = memo.get(&(account, device)).copied();
        if known.is_some_and(|known| known.writer) {
            return Ok(true);
        }
        let control_len = crate::account::held_control_log_len(tx, account)?;
        if let Some(known) = known
            && known.control_len == control_len
        {
            return Ok(known.writer);
        }
        let writer = crate::account::device_ever_enrolled_as_writer(tx, account, device)?;
        memo.insert((account, device), Answer { control_len, writer });
        Ok(writer)
    }
}

/// Everything that must be settled BEFORE `op` is handed to [`apply_row_op_on_stream`], in the
/// order it has to be settled in.
///
/// Both the live ingest path and the refold need this exact sequence, and the ordering is easy to
/// get right in one and wrong in the other — so it lives here once. The payload goes first:
/// whatever [`payload_verdict`] decides, no row read can change, and filing a version gap or a
/// terminal payload under a row-state reason would move it into the retry family replayed at every
/// store open, where nothing could redeem it. Only when the payload leaves the question open does
/// the ROW get a say.
pub(crate) fn pre_apply(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
    op: &RowOp,
    entry_lamport: u64,
    doubt: RowDoubt,
) -> anyhow::Result<PreApply> {
    match payload_verdict(spec, repo_id, op) {
        PayloadVerdict::Gap(reason) => return Ok(PreApply::Park(reason)),
        // Terminal on its own merits. Hand it to `apply_row_op_on_stream`, which quarantines it
        // without writing — one writer of that verdict rather than two, and a terminal
        // payload must not enter a retry family it can never leave.
        PayloadVerdict::Rejected(_) => return Ok(PreApply::Apply),
        PayloadVerdict::RowDecides(_) => {},
    }
    // Same rule for a restatement that names a lamport its chain never held: terminal, so it
    // must reach the applier's quarantine rather than park behind a row it will never touch.
    if let RowOp::Restate { deletes, .. } = op
        && deletes.iter().any(|delete| delete.lamport >= entry_lamport)
    {
        return Ok(PreApply::Apply);
    }
    if doubt == RowDoubt::NothingUnsent {
        return Ok(PreApply::Apply);
    }
    let Some(deferral) = unsent_work_blocking_replay(tx, spec, repo_id, stream, op)? else {
        return Ok(PreApply::Apply);
    };
    // The one case a caller may act through: a deletion held back on a verdict that may never
    // resolve. Everything else defers, including an unprovable verdict against an `Upsert` — see
    // [`RowDoubt::DeferExceptUnprovableRemoval`] for why the op kind, not the confidence, is what
    // makes the difference.
    let unprovable_removal = !deferral.is_proven_unsent_work()
        && matches!(op, RowOp::Remove { .. } | RowOp::Restate { .. });
    Ok(match doubt {
        RowDoubt::DeferExceptUnprovableRemoval if unprovable_removal => PreApply::Apply,
        RowDoubt::DeferOnAnyDoubt | RowDoubt::DeferExceptUnprovableRemoval =>
            PreApply::Park(deferral),
        RowDoubt::NothingUnsent => unreachable!("returned before the row was read"),
    })
}

/// The outcome of [`pre_apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreApply {
    /// Do not apply: record this reason and leave the entry outstanding.
    Park(PendingReason),
    /// Nothing stands in the way — hand it to [`apply_row_op_on_stream`].
    Apply,
}

/// Claim `row_pk` as a COMPLETE projection: `hash` covers every synced column this binary knows,
/// stamped with the TABLE's spec version, which is what defines that column set. Deliberately not
/// the store-global projector version — that would make an unrelated table's registration mark this
/// row incomparable. This also serves bookkeeping-only version refreshes; diagnostic settlement
/// belongs to the winning apply path, not this hash write.
pub(crate) fn record_published(
    tx: &Transaction<'_>,
    key: &RowKey<'_>,
    hash: &str,
    spec_version: u32,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO sync_published_rows(
             stream_id, repo_id, table_name, row_pk, synced_hash, spec_version
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(stream_id, table_name, row_pk) DO UPDATE
             SET synced_hash = excluded.synced_hash, spec_version = excluded.spec_version",
        rusqlite::params![
            key.stream.to_bytes().as_slice(),
            key.repo_id,
            key.table,
            key.row_pk,
            hash,
            spec_version,
        ],
    )?;
    Ok(())
}

fn clear_published(tx: &Transaction<'_>, key: &RowKey<'_>) -> anyhow::Result<()> {
    tx.execute(
        "DELETE FROM sync_published_rows
          WHERE stream_id = ?1 AND repo_id = ?2 AND table_name = ?3 AND row_pk = ?4",
        rusqlite::params![key.stream.to_bytes().as_slice(), key.repo_id, key.table, key.row_pk],
    )?;
    diagnostics::clear(tx, key)?;
    Ok(())
}

/// Build the re-adoption `Remove` for an orphaned tombstone: just the row pk (a delete carries no
/// after-image).
pub(crate) fn readopt_remove(spec: &TableSpec, row_pk: &str) -> anyhow::Result<RowOp> {
    Ok(RowOp::Remove {
        table: spec.name.to_string(),
        spec_version: spec.spec_version,
        pk: row_op::row_pk_values(row_pk)?,
    })
}

// ── value / identifier plumbing ──────────────────────────────────────────────────────────────

/// A `col = ?` conjunction over the pk columns, in registry order (the `pk_params` order matches).
fn pk_where(spec: &TableSpec) -> String {
    spec.pk.iter().map(|c| format!("{} = ?", quote_ident(c.name))).collect::<Vec<_>>().join(" AND ")
}

fn pk_params(pk_vals: &[TypedValue]) -> Vec<SqlValue> {
    pk_vals.iter().map(sql_value).collect()
}

/// Double-quote a SQL identifier (table/column). Names come only from the `&'static` registry, so
/// this is defense in depth, not untrusted-input escaping.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn sql_value(value: &TypedValue) -> SqlValue {
    match value {
        TypedValue::Null => SqlValue::Null,
        TypedValue::Bool(b) => SqlValue::Integer(i64::from(*b)),
        TypedValue::I64(n) => SqlValue::Integer(*n),
        TypedValue::Text(s) => SqlValue::Text(s.clone()),
        TypedValue::Blob(b) => SqlValue::Blob(b.clone()),
    }
}

/// Mapping one stored value back to its declared type: the value, or the reason it has none.
///
/// A REASON rather than an `Err`, because every read below sits under a path that must not fail —
/// see [`SyncedRow`].
enum ReadCell {
    Value(TypedValue),
    Malformed(String),
}

/// Map the stored value at `idx` to its DECLARED type.
///
/// TOTAL over (declared type, storage class) by construction: every pair either produces a value or
/// names why it cannot, and the only `Err` left is a genuine statement fault (a column index that
/// does not exist). That totality is the whole point — this runs under the refold at STORE OPEN,
/// where an error fails the open itself rather than one read, so no stored byte pattern may be able
/// to reach a `?`.
///
/// It is deliberately NOT argued from `STRICT`. STRICT pins the storage CLASS, not the value's
/// domain within it, and two domains escape it:
///   * a `Bool` needs `CHECK (col IN (0, 1))` to exclude other integers, and no pragma exposes that
///     for the registry lint to require;
///   * a `Text` column can hold bytes that are not valid UTF-8 (`CAST(X'80' AS TEXT)` stores with
///     `typeof() = 'text'`), which is a conversion failure the moment it is read as a `String`.
///
/// A mismatched storage class is folded in for the same reason: it should be unreachable on a
/// STRICT table, and "should be unreachable" is exactly the argument that made the previous version
/// fail an open.
fn read_typed(row: &rusqlite::Row<'_>, idx: usize, vt: ValueType) -> rusqlite::Result<ReadCell> {
    use rusqlite::types::ValueRef;

    let raw = row.get_ref(idx)?;
    let value = match (vt, raw) {
        (_, ValueRef::Null) => TypedValue::Null,
        // Not `get::<String>`: that maps invalid UTF-8 to a conversion ERROR, which is precisely
        // the failure this function exists to keep out of the store-open path.
        (ValueType::Text, ValueRef::Text(bytes)) => match std::str::from_utf8(bytes) {
            Ok(text) => TypedValue::Text(text.to_string()),
            Err(_) =>
                return Ok(ReadCell::Malformed(
                    "a Text column holds bytes that are not valid UTF-8".to_string(),
                )),
        },
        (ValueType::I64, ValueRef::Integer(n)) => TypedValue::I64(n),
        (ValueType::Blob, ValueRef::Blob(bytes)) => TypedValue::Blob(bytes.to_vec()),
        // A Bool column must hold exactly 0 or 1. Coercing any other integer to `true` (the old
        // `n != 0`) would silently rewrite the source value to 1 on self-apply and replicate a
        // value that differs from the row.
        (ValueType::Bool, ValueRef::Integer(0)) => TypedValue::Bool(false),
        (ValueType::Bool, ValueRef::Integer(1)) => TypedValue::Bool(true),
        (ValueType::Bool, ValueRef::Integer(other)) =>
            return Ok(ReadCell::Malformed(format!("a Bool column holds {other}, not 0 or 1"))),
        (declared, other) =>
            return Ok(ReadCell::Malformed(format!(
                "a {declared:?} column holds {:?} storage",
                other.data_type()
            ))),
    };
    Ok(ReadCell::Value(value))
}

/// Whether a value is storable in a column of the declared type. `Null` fits any column
/// (nullability is the DB's constraint); every other value must match exactly.
fn value_matches(value: &TypedValue, vt: ValueType) -> bool {
    matches!(
        (value, vt),
        (TypedValue::Null, _)
            | (TypedValue::Bool(_), ValueType::Bool)
            | (TypedValue::I64(_), ValueType::I64)
            | (TypedValue::Text(_), ValueType::Text)
            | (TypedValue::Blob(_), ValueType::Blob)
    )
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
