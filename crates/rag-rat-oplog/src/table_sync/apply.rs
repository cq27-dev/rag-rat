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

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension, Transaction, params_from_iter};

use super::registry::{DefaultValue, TableSpec, ValueType};
use super::row_op::{self, Cell, RowOp, TypedValue};
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
    Quarantined(String),
    Unprojectable(PendingReason),
}

/// Fold `op` into `spec`'s table for `repo_id`, ordered by `meta`. See the module doc for the merge
/// rules. Every write goes through the caller's transaction, so a partially-applied op cannot leak.
pub(crate) fn apply_row_op(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    op: &RowOp,
    meta: OpMeta,
) -> anyhow::Result<ApplyOutcome> {
    debug_assert_eq!(op.table(), spec.name, "caller resolves the spec from the op's table");
    let known = match payload_verdict(spec, repo_id, op) {
        PayloadVerdict::Gap(reason) => return Ok(ApplyOutcome::Unprojectable(reason)),
        PayloadVerdict::Rejected(why) => return Ok(ApplyOutcome::Quarantined(why)),
        PayloadVerdict::RowDecides(known) => known,
    };
    let pk_vals = op.pk();
    match known {
        // A complete after-image: an upsert, whose whole column set the row will take.
        Some(known) => apply_upsert(tx, spec, repo_id, pk_vals, known, meta),
        // Nothing to project — a remove names only the row identity.
        None => apply_remove(tx, spec, repo_id, pk_vals, meta),
    }
}

/// What [`apply_row_op`] decides on the PAYLOAD ALONE, before any row state is read.
#[derive(Debug, PartialEq)]
pub(crate) enum PayloadVerdict {
    /// Nothing in the payload stands in the way; the ROW STATE decides what happens next. Carries
    /// the complete after-image for an upsert, and `None` for a remove, which has no column set.
    RowDecides(Option<Vec<(&'static str, TypedValue)>>),
    /// A version gap: this binary does not understand the payload yet. No row read changes that.
    Gap(PendingReason),
    /// Terminal on its own merits — a broken producer. No row read changes that either.
    Rejected(String),
}

/// Everything [`apply_row_op`] can settle without touching the database, in the order it settles
/// it.
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
    let pk_vals = op.pk();
    if pk_vals.len() != spec.pk.len() {
        return PayloadVerdict::Rejected(format!(
            "pk arity {} does not match `{}`'s {} identity columns",
            pk_vals.len(),
            spec.name,
            spec.pk.len()
        ));
    }
    // A NULL identity value is unaddressable: `WHERE pk = NULL` never matches, so upserts would
    // insert duplicate unreachable rows and removes could never delete them. Reject the whole op.
    if pk_vals.iter().any(|v| matches!(v, TypedValue::Null)) {
        return PayloadVerdict::Rejected(format!(
            "a null primary-key value is not addressable on `{}`",
            spec.name
        ));
    }
    // Validate each pk value against its declared type before it reaches a WHERE clause. SQLite
    // affinity would otherwise coerce a mismatched pk (e.g. `I64(1)` matching a TEXT key `'1'`)
    // onto a different physical row than its type-exact `row_pk` clock identity, splitting the
    // row's bookkeeping and allowing resurrection. (Arity is checked above, so `zip` covers
    // every pk.)
    for (column, value) in spec.pk.iter().zip(pk_vals) {
        if !value_matches(value, column.value_type) {
            return PayloadVerdict::Rejected(format!(
                "pk column `{}` value does not match its declared type on `{}`",
                column.name, spec.name
            ));
        }
    }
    // Repo-identity gate: for a table scoped by a pk column, an op naming a different repo than the
    // stream being synced is rejected — a peer cannot write another project's rows through this
    // stream. (The producer already only emits the local repo's rows.)
    if let Some(idx) = spec.repo_pk_index()
        && pk_vals.get(idx) != Some(&TypedValue::Text(repo_id.to_string()))
    {
        return PayloadVerdict::Rejected(format!(
            "op names a different repo than the `{}` stream being synced",
            spec.name
        ));
    }
    match op {
        // A remove names only the row identity, so no column set is involved: its `spec_version` is
        // carried for wire symmetry and diagnostics, never acted on. Gating a deletion on a version
        // skew would delay it for no benefit.
        RowOp::Remove { .. } => PayloadVerdict::RowDecides(None),
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

/// Apply a delete. It wins unless the row's write clock is strictly newer than the delete (a
/// concurrent later write keeps the row); either way it raises the row's tombstone so a later
/// Upsert older than the delete cannot resurrect the row. Order-independent: a stale delete
/// arriving after a newer write loses, and an even older insert arriving after the delete is
/// suppressed by the tombstone.
fn apply_remove(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    pk_vals: &[TypedValue],
    meta: OpMeta,
) -> anyhow::Result<ApplyOutcome> {
    let row_pk = &row_op::row_pk_string(pk_vals);
    let device_hex = &meta.device.to_string();
    let survives = match current_row_clock(tx, repo_id, spec.name, row_pk)? {
        // A write strictly newer than the delete keeps the row alive.
        Some((clock_lamport, clock_device)) =>
            beats(clock_lamport, &clock_device, meta.lamport, device_hex),
        // No recorded write — nothing can outrank the delete.
        None => false,
    };
    if !survives {
        // Attempt the physical delete BEFORE raising the tombstone. A constraint violation — an FK
        // RESTRICT child row, an `ON DELETE RESTRICT`, a trigger abort — means the remove cannot
        // apply; quarantine it (leaving the tombstone/clock untouched) so the already-stored entry
        // is retained and the chain advances, instead of erroring and rolling back the entry (which
        // would wedge every later entry on that device's chain as a permanent MissingPredecessor).
        if let Err(err) = delete_row(tx, spec, pk_vals) {
            if is_constraint_violation(&err) {
                return Ok(ApplyOutcome::Quarantined(format!(
                    "remove violates a column constraint on `{}`",
                    spec.name
                )));
            }
            return Err(err);
        }
        clear_row_clock(tx, repo_id, spec.name, row_pk)?;
        clear_published(tx, repo_id, spec.name, row_pk)?;
    }
    // Raise the tombstone only once the remove has actually applied (the row was deleted, or a
    // newer write kept it): the tombstone guards against an older upsert resurrecting the row.
    raise_tombstone(tx, repo_id, spec.name, row_pk, meta.lamport, device_hex)?;
    // The tombstone is raised either way, but a delete a newer write outranks did not delete
    // anything — and, crucially, left the published record in place. Say so.
    Ok(if survives { ApplyOutcome::Superseded } else { ApplyOutcome::Applied })
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
    pk_vals: &[TypedValue],
    known: Vec<(&'static str, TypedValue)>,
    meta: OpMeta,
) -> anyhow::Result<ApplyOutcome> {
    let row_pk = &row_op::row_pk_string(pk_vals);
    let device_hex = &meta.device.to_string();

    // A row deleted at a clock this op cannot beat stays deleted: the delete is newer than this
    // edit, so the edit must not resurrect the row. (Suppressed, but the entry is still stored, so
    // redelivery stays idempotent.)
    if let Some((t_lamport, t_device)) = current_tombstone(tx, repo_id, spec.name, row_pk)?
        && !beats(meta.lamport, device_hex, t_lamport, &t_device)
    {
        return Ok(ApplyOutcome::Superseded);
    }

    // Whole-row LWW: the op wins the ENTIRE row iff it beats the row's write clock (or the row is
    // new). A losing op is a no-op — it never partially overwrites, and it must not touch the
    // published hash (that would mark an unsent local edit as sent and make the producer drop it).
    let wins = match current_row_clock(tx, repo_id, spec.name, row_pk)? {
        Some((c_lamport, c_device)) => beats(meta.lamport, device_hex, c_lamport, &c_device),
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
        update_row(tx, spec, &known, pk_vals)
    } else {
        insert_row(tx, spec, pk_vals, &known)
    };
    if let Err(err) = write {
        if is_constraint_violation(&err) {
            return Ok(ApplyOutcome::Quarantined(format!(
                "op violates a column constraint on `{}`",
                spec.name
            )));
        }
        return Err(err);
    }
    raise_row_clock(tx, repo_id, spec.name, row_pk, meta.lamport, device_hex)?;

    // Anti-echo: the winning op now owns the whole current row state, so record its synced hash.
    // (A losing op returned above without touching the published hash.)
    if let Some(hash) = synced_row_hash(tx, spec, pk_vals)? {
        record_published(tx, repo_id, spec.name, row_pk, &hash, spec.spec_version)?;
    }
    Ok(ApplyOutcome::Applied)
}

/// The total order on clocks: `(lamport, device_hex)` beats `(other_lamport, other_device)` iff its
/// lamport is higher, or equal with a lexicographically-smaller fingerprint (fixed-width lowercase
/// hex orders exactly as the raw fingerprint bytes). The one comparison every whole-row LWW
/// decision uses — the row write clock, the tombstone, and remove-vs-edit.
fn beats(lamport: u64, device_hex: &str, other_lamport: u64, other_device: &str) -> bool {
    lamport > other_lamport || (lamport == other_lamport && device_hex < other_device)
}

/// The row's latest-write clock, or `None` if it has never been written on this device. Recorded on
/// every write (including an insert-only row, which has no per-column clock), it is what a delete
/// and the anti-echo gate compare against.
fn current_row_clock(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
) -> anyhow::Result<Option<(u64, String)>> {
    let row = tx
        .query_row(
            "SELECT lamport, device_fingerprint FROM sync_row_clocks
             WHERE repo_id = ?1 AND table_name = ?2 AND row_pk = ?3",
            rusqlite::params![repo_id, table, row_pk],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(lamport, device)| Ok((u64::try_from(lamport)?, device))).transpose()
}

/// Raise the row's write clock to `(lamport, device_hex)` under LWW — a later-arriving but older
/// write never lowers it.
fn raise_row_clock(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
    lamport: u64,
    device_hex: &str,
) -> anyhow::Result<()> {
    if let Some((old_lamport, old_device)) = current_row_clock(tx, repo_id, table, row_pk)?
        && !beats(lamport, device_hex, old_lamport, &old_device)
    {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO sync_row_clocks(repo_id, table_name, row_pk, lamport, device_fingerprint)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(repo_id, table_name, row_pk)
         DO UPDATE SET lamport = excluded.lamport, device_fingerprint = excluded.device_fingerprint",
        rusqlite::params![repo_id, table, row_pk, i64::try_from(lamport)?, device_hex],
    )?;
    Ok(())
}

fn current_tombstone(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
) -> anyhow::Result<Option<(u64, String)>> {
    let row = tx
        .query_row(
            "SELECT lamport, device_fingerprint FROM sync_row_tombstones
             WHERE repo_id = ?1 AND table_name = ?2 AND row_pk = ?3",
            rusqlite::params![repo_id, table, row_pk],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(lamport, device)| Ok((u64::try_from(lamport)?, device))).transpose()
}

/// Raise the row's tombstone to `(lamport, device_hex)` under LWW — a lower clock never lowers it.
fn raise_tombstone(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
    lamport: u64,
    device_hex: &str,
) -> anyhow::Result<()> {
    if let Some((old_lamport, old_device)) = current_tombstone(tx, repo_id, table, row_pk)?
        && !beats(lamport, device_hex, old_lamport, &old_device)
    {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO sync_row_tombstones(repo_id, table_name, row_pk, lamport, device_fingerprint)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(repo_id, table_name, row_pk)
         DO UPDATE SET lamport = excluded.lamport, device_fingerprint = excluded.device_fingerprint",
        rusqlite::params![repo_id, table, row_pk, i64::try_from(lamport)?, device_hex],
    )?;
    Ok(())
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
/// is atomic — never a half-written row.
fn update_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    cells: &[(&'static str, TypedValue)],
    pk_vals: &[TypedValue],
) -> anyhow::Result<()> {
    let assignments = cells
        .iter()
        .map(|(name, _)| format!("{} = ?", quote_ident(name)))
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

fn delete_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    pk_vals: &[TypedValue],
) -> anyhow::Result<()> {
    let sql = format!("DELETE FROM {} WHERE {}", quote_ident(spec.name), pk_where(spec));
    tx.execute(&sql, params_from_iter(pk_params(pk_vals)))?;
    Ok(())
}

// ── row clock + published-row bookkeeping ────────────────────────────────────────────────────

fn clear_row_clock(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
) -> anyhow::Result<()> {
    tx.execute(
        "DELETE FROM sync_row_clocks WHERE repo_id = ?1 AND table_name = ?2 AND row_pk = ?3",
        rusqlite::params![repo_id, table, row_pk],
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
pub(crate) fn published_hash(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
) -> anyhow::Result<Option<(String, u32)>> {
    let row = tx
        .query_row(
            "SELECT synced_hash, spec_version FROM sync_published_rows
             WHERE repo_id = ?1 AND table_name = ?2 AND row_pk = ?3",
            rusqlite::params![repo_id, table, row_pk],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    row.map(|(hash, version)| Ok((hash, u32::try_from(version)?))).transpose()
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
    Unknown,
}

/// Compare a stale-version row against its own winning op, projected under the CURRENT spec.
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
    let row_pk = row_op::row_pk_string(pk_vals);
    let Some((lamport, device_hex)) = current_row_clock(tx, repo_id, spec.name, &row_pk)? else {
        return Ok(StaleRow::Unknown);
    };
    let Some(op) = super::store::winning_entry_op(tx, stream, &device_hex, lamport)? else {
        // The entry is gone. Nothing prunes today; when retention lands it must refresh a row
        // before dropping that row's winner, or this arm becomes a permanent stale state.
        return Ok(StaleRow::Unknown);
    };
    // The entry is located by `(stream, device, lamport)`, which identifies it uniquely WITHIN a
    // stream — so the hit is this row's op only while the row's clock and the stream being queried
    // belong to the same stream. That holds today, but it is not an enforced property: a table's
    // stream is derived from `(repo_id, account_id, scope_id)`, and moving a registered table to a
    // different scope is an ordinary registry edit. The row's clock would then carry a lamport
    // allocated on the OLD stream, and the same `(device, lamport)` on the new one belongs to some
    // SIBLING table's op. Verify the identity rather than trusting the derivation: a mismatch must
    // read as "cannot resolve" (and be handled conservatively), never as a verdict about this row.
    // Without this, two tables with coincidentally similar columns can project `Complete` and
    // return `Unchanged` for a row that actually holds an unsent edit — which lets the refold
    // replay straight over it.
    if op.table() != spec.name || op.pk() != pk_vals {
        return Ok(StaleRow::Unknown);
    }
    // A winning REMOVE clears the row clock, so a live clock can only ever point at an upsert.
    let RowOp::Upsert { spec_version, cells, .. } = &op else {
        return Ok(StaleRow::Unknown);
    };
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
        Projection::Park(_) | Projection::Quarantine(_) => Ok(StaleRow::Unknown),
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
    let pk_vals = op.pk();
    // A malformed key never reached `apply_row_op`'s arity check (an entry parked as out-of-scope
    // or unknown-kind was never validated), and binding it against `spec.pk`'s placeholders
    // would be a parameter-count ERROR — which, propagating out of the refold, would roll back
    // the transaction and fail every subsequent store open on the same entry. Defer to the
    // normal path, which quarantines it.
    if pk_vals.len() != spec.pk.len() {
        return Ok(None);
    }
    let row_pk = row_op::row_pk_string(pk_vals);
    let current_cells = match read_synced_cells(tx, spec, pk_vals)? {
        SyncedRow::Cells(cells) => cells,
        SyncedRow::Absent => {
            // No row — but a surviving published identity means the row was DELETED locally and not
            // yet authored. That is precisely what the producer's `Remove` branch keys on, so
            // replaying an upsert here would recreate the row and discard the unsent deletion for
            // good.
            return Ok(published_hash(tx, repo_id, spec.name, &row_pk)?
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
        SyncedRow::Unreadable(_) =>
            return Ok(
                matches!(op, RowOp::Remove { .. }).then_some(PendingReason::DeferredUnreadableRow)
            ),
    };
    let current = row_op::cells_hash(&current_cells);
    Ok(match published_hash(tx, repo_id, spec.name, &row_pk)? {
        // Comparable: a differing hash is a demonstrably unsent local change.
        Some((published, version)) if version == spec.spec_version =>
            (published != current).then_some(PendingReason::DeferredUnsentEdit),
        // Published under a different column set, so the hashes cannot be compared — but the row's
        // WINNING op can be, projected under this spec. This proof path is required, not an
        // optimization: once an older-spec op can be filled from declared defaults (#1002) a parked
        // entry can WIN over an unsent raw edit here, where before it could not apply at all.
        // Unprovable stays conservative: refuse to replay rather than risk overwriting.
        Some(_) => match stale_row_disposition(tx, spec, repo_id, stream, pk_vals, &current_cells)?
        {
            StaleRow::LocallyChanged => Some(PendingReason::DeferredUnsentEdit),
            StaleRow::Unknown => Some(PendingReason::DeferredUnresolvedWinner),
            StaleRow::Unchanged => None,
        },
        // A live row no apply ever published is purely local: the only content there came from this
        // device, and no peer has seen it.
        None => Some(PendingReason::DeferredUnsentEdit),
    })
}

/// Claim `row_pk` as a COMPLETE projection: `hash` covers every synced column this binary knows,
/// stamped with the TABLE's spec version, which is what defines that column set. Deliberately not
/// the store-global projector version — that would make an unrelated table's registration mark this
/// row incomparable.
pub(crate) fn record_published(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
    hash: &str,
    spec_version: u32,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO sync_published_rows(repo_id, table_name, row_pk, synced_hash, spec_version)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(repo_id, table_name, row_pk) DO UPDATE
             SET synced_hash = excluded.synced_hash, spec_version = excluded.spec_version",
        rusqlite::params![repo_id, table, row_pk, hash, spec_version],
    )?;
    Ok(())
}

fn clear_published(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
) -> anyhow::Result<()> {
    tx.execute(
        "DELETE FROM sync_published_rows WHERE repo_id = ?1 AND table_name = ?2 AND row_pk = ?3",
        rusqlite::params![repo_id, table, row_pk],
    )?;
    Ok(())
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
mod tests {
    use super::*;
    use crate::op::DeviceFingerprint;
    use crate::table_sync::registry::ColumnSpec;

    const SPEC: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("title", ValueType::Text)],
        local_columns: &["resolved_rowid"],
        repo_column: None,
    };

    fn conn() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch(
            "CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT, resolved_rowid INTEGER) STRICT;",
        )
        .unwrap();
        c
    }

    fn device(seed: u8) -> DeviceFingerprint {
        DeviceFingerprint::from_bytes([seed; 32])
    }

    fn upsert(cells: &[(&str, TypedValue)]) -> RowOp {
        RowOp::Upsert {
            spec_version: 1,
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text("r1".to_string())],
            cells: cells
                .iter()
                .map(|(c, v)| Cell { column: (*c).to_string(), value: v.clone() })
                .collect(),
        }
    }

    fn title(conn: &rusqlite::Connection) -> Option<String> {
        conn.query_row("SELECT title FROM t_demo WHERE id = 'r1'", [], |r| r.get(0))
            .optional()
            .unwrap()
    }

    #[test]
    fn an_insert_writes_the_row_and_records_the_clock_and_hash() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        let out = apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("hi".to_string()))]),
            OpMeta { lamport: 5, device: device(2) },
        )
        .unwrap();
        assert_eq!(out, ApplyOutcome::Applied);
        let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
        assert!(published_hash(&tx, "repo", "t_demo", &row_pk).unwrap().is_some());
        assert_eq!(current_row_clock(&tx, "repo", "t_demo", &row_pk).unwrap().unwrap().0, 5);
        tx.commit().unwrap();
        assert_eq!(title(&c).as_deref(), Some("hi"));
    }

    #[test]
    fn whole_row_lww_the_winner_takes_the_whole_row_no_per_column_merge() {
        // The chosen semantics: a concurrent edit to a DIFFERENT column does NOT merge in — the
        // higher-lamport op owns the entire row. Deterministic (both peers converge to the same
        // winner) and matches the whole-row real tables. Two-column table so the merge could
        // differ.
        const TWO_COL: TableSpec = TableSpec {
            name: "t_two",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("count", ValueType::I64),
            ],
            local_columns: &[],
            repo_column: None,
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch(
            "CREATE TABLE t_two(id TEXT PRIMARY KEY, title TEXT, count INTEGER) STRICT;",
        )
        .unwrap();
        let full = |title: &str, count: i64| RowOp::Upsert {
            spec_version: 1,
            table: "t_two".to_string(),
            pk: vec![TypedValue::Text("r1".into())],
            cells: vec![
                Cell { column: "title".into(), value: TypedValue::Text(title.into()) },
                Cell { column: "count".into(), value: TypedValue::I64(count) },
            ],
        };
        let tx = c.transaction().unwrap();
        // Higher lamport sets {A, 1}...
        apply_row_op(&tx, &TWO_COL, "repo", &full("A", 1), OpMeta {
            lamport: 6,
            device: device(2),
        })
        .unwrap();
        // ...a lower-lamport op editing a different column loses the WHOLE row, not just `title`.
        let out = apply_row_op(&tx, &TWO_COL, "repo", &full("B", 2), OpMeta {
            lamport: 5,
            device: device(1),
        })
        .unwrap();
        assert_eq!(out, ApplyOutcome::Superseded, "outranked by the row's write clock");
        tx.commit().unwrap();
        let row: (String, i64) = c
            .query_row("SELECT title, count FROM t_two WHERE id = 'r1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(
            (row.0.as_str(), row.1),
            ("A", 1),
            "the whole row is the winning op's — no merge"
        );
    }

    #[test]
    fn a_higher_lamport_wins_and_a_lower_lamport_loses() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("a".into()))]),
            OpMeta { lamport: 5, device: device(2) },
        )
        .unwrap();
        // A later lamport overwrites.
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("b".into()))]),
            OpMeta { lamport: 6, device: device(2) },
        )
        .unwrap();
        // A stale (lower-lamport) op is ignored.
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("stale".into()))]),
            OpMeta { lamport: 4, device: device(9) },
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(title(&c).as_deref(), Some("b"));
    }

    #[test]
    fn a_lamport_tie_is_broken_by_the_smaller_fingerprint() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("dev5".into()))]),
            OpMeta { lamport: 7, device: device(5) },
        )
        .unwrap();
        // Same lamport, SMALLER fingerprint → wins.
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("dev1".into()))]),
            OpMeta { lamport: 7, device: device(1) },
        )
        .unwrap();
        // Same lamport, LARGER fingerprint → loses.
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("dev9".into()))]),
            OpMeta { lamport: 7, device: device(9) },
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(title(&c).as_deref(), Some("dev1"), "the smaller fingerprint wins the tie");
    }

    #[test]
    fn a_type_mismatch_quarantines_the_op_and_writes_nothing() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        // `title` is declared Text; an I64 value is a broken producer.
        let out =
            apply_row_op(&tx, &SPEC, "repo", &upsert(&[("title", TypedValue::I64(7))]), OpMeta {
                lamport: 1,
                device: device(2),
            })
            .unwrap();
        assert!(matches!(out, ApplyOutcome::Quarantined(_)));
        tx.commit().unwrap();
        assert_eq!(title(&c), None, "a quarantined op leaves the table untouched");
    }

    #[test]
    fn a_partial_upsert_missing_a_synced_column_is_parked() {
        // Whole-row LWW needs a full after-image; a two-column table given only one column can't be
        // cleanly replaced, so the op is quarantined rather than applied as a hybrid row.
        const TWO_COL: TableSpec = TableSpec {
            name: "t_two",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("count", ValueType::I64),
            ],
            local_columns: &[],
            repo_column: None,
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch(
            "CREATE TABLE t_two(id TEXT PRIMARY KEY, title TEXT, count INTEGER) STRICT;",
        )
        .unwrap();
        let tx = c.transaction().unwrap();
        let partial = RowOp::Upsert {
            spec_version: 1,
            table: "t_two".to_string(),
            pk: vec![TypedValue::Text("r1".into())],
            cells: vec![Cell {
                column: "title".into(),
                value: TypedValue::Text("only-title".into()),
            }],
        };
        let out =
            apply_row_op(&tx, &TWO_COL, "repo", &partial, OpMeta { lamport: 1, device: device(2) })
                .unwrap();
        assert_eq!(
            out,
            ApplyOutcome::Unprojectable(PendingReason::PartialAfterImage),
            "a partial after-image is PARKED, not quarantined: the likely cause is an older \
             producer whose narrower complete row is partial under this spec, and #1002's \
             declared defaults redeem it — quarantining would drop it off the refold worklist for \
             good"
        );
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM t_two", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "nothing was written");
    }

    #[test]
    fn a_newer_op_parks_on_its_version_alone_even_carrying_only_known_columns() {
        // The version gate must do work the unknown-column gate does not. Every "newer" fixture
        // elsewhere ALSO names a column the receiver lacks, so deleting the version check entirely
        // only changed a reason string — the op parked either way. Here the op is well within this
        // registry's vocabulary and is refused purely for claiming a later generation, which is the
        // whole point: a later spec may mean something by these same columns that we cannot know.
        assert_eq!(
            project_cells(&SPEC, SPEC.spec_version + 1, &[Cell {
                column: "title".into(),
                value: TypedValue::Text("known".into()),
            }]),
            Projection::Park(PendingReason::NewerSpecVersion),
            "a newer generation parks on the version alone"
        );
        // And the converse, so the gate cannot simply park everything.
        assert!(matches!(
            project_cells(&SPEC, SPEC.spec_version, &[Cell {
                column: "title".into(),
                value: TypedValue::Text("known".into()),
            }]),
            Projection::Complete(_)
        ));
    }

    #[test]
    fn a_cell_newer_than_the_ops_own_claimed_version_is_a_misstamp() {
        // Self-contradictory: a producer at v1 cannot have known a column introduced at v2. Left
        // unchecked, the understated version would default-fill every column it claims to predate,
        // resetting them on every receiver — so this is the one half of the advisory stamp a
        // receiver can verify against its own registry.
        const WIDE: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
            spec_version: 2,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("d")),
            ],
            local_columns: &[],
            repo_column: None,
        };
        let cells = |version: u32| {
            project_cells(&WIDE, version, &[
                Cell { column: "later".into(), value: TypedValue::Text("v".into()) },
                Cell { column: "title".into(), value: TypedValue::Text("t".into()) },
            ])
        };
        assert_eq!(
            cells(1),
            Projection::Park(PendingReason::MisstampedSpecVersion),
            "claiming v1 while carrying a v2 column is a mis-stamp, not an old complete row"
        );
        assert!(matches!(cells(2), Projection::Complete(_)), "the honest stamp projects");
    }

    #[test]
    fn every_default_variant_fills_its_own_typed_value() {
        // One case per `DefaultValue` variant. Without this, only `Text` and `Null` were exercised,
        // and collapsing `Bool`/`I64`/`Blob` to `TypedValue::Null` passed the whole suite — which
        // on a NOT NULL column quarantines the op terminally, and on a nullable one
        // diverges from the migration's backfill at the same clock.
        const TYPED: TableSpec = TableSpec {
            name: "t_typed",
            scope_id: "demo/1",
            spec_version: 2,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[
                ColumnSpec::added("flag", ValueType::Bool, 2, DefaultValue::Bool(true)),
                ColumnSpec::added("count", ValueType::I64, 2, DefaultValue::I64(7)),
                ColumnSpec::added("note", ValueType::Text, 2, DefaultValue::Text("d")),
                ColumnSpec::added("raw", ValueType::Blob, 2, DefaultValue::Blob(&[1, 2])),
                ColumnSpec::added("empty", ValueType::Text, 2, DefaultValue::Null),
            ],
            local_columns: &[],
            repo_column: None,
        };
        assert_eq!(
            project_cells(&TYPED, 1, &[]),
            Projection::Complete(vec![
                ("flag", TypedValue::Bool(true)),
                ("count", TypedValue::I64(7)),
                ("note", TypedValue::Text("d".into())),
                ("raw", TypedValue::Blob(vec![1, 2])),
                ("empty", TypedValue::Null),
            ]),
            "each declared default fills as its own typed value, not as NULL"
        );
    }

    #[test]
    fn the_default_fill_window_is_per_column_not_merely_older_than_the_spec() {
        // Once a table reaches a THIRD version, "older than the current spec" stops being a safe
        // test for "predates this column". A v2 op that omits a column v2 already had is a broken
        // partial, and must park — filling it would reset that column to its default for EVERY
        // receiver under whole-row LWW, silently and with no local edit to signal it. Only an op
        // older than the column's OWN introducing version may be filled.
        const THREE: TableSpec = TableSpec {
            name: "t_three",
            scope_id: "demo/1",
            spec_version: 3,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("v2-default")),
                ColumnSpec::added("latest", ValueType::Text, 3, DefaultValue::Text("v3-default")),
            ],
            local_columns: &[],
            repo_column: None,
        };
        let project = |version: u32, cells: &[(&str, &str)]| {
            let cells: Vec<Cell> = cells
                .iter()
                .map(|(c, v)| Cell {
                    column: (*c).to_string(),
                    value: TypedValue::Text((*v).to_string()),
                })
                .collect();
            project_cells(&THREE, version, &cells)
        };

        // v1 predates BOTH added columns — each is filled from its own declared default.
        assert_eq!(
            project(1, &[("title", "t")]),
            Projection::Complete(vec![
                ("title", TypedValue::Text("t".into())),
                ("later", TypedValue::Text("v2-default".into())),
                ("latest", TypedValue::Text("v3-default".into())),
            ]),
            "an op older than both columns is completed from both defaults"
        );

        // v2 predates only `latest`; `later` existed in v2, so a v2 op carrying it is complete.
        assert_eq!(
            project(2, &[("title", "t"), ("later", "sent")]),
            Projection::Complete(vec![
                ("title", TypedValue::Text("t".into())),
                ("later", TypedValue::Text("sent".into())),
                ("latest", TypedValue::Text("v3-default".into())),
            ]),
            "only the column the op's own version predates is filled"
        );

        // THE REGRESSION: a v2 op omitting `later` is a broken partial, not an old complete row.
        assert_eq!(
            project(2, &[("title", "t")]),
            Projection::Park(PendingReason::PartialAfterImage),
            "a column the op's own version already had must never be defaulted — the op is a \
             partial and parks"
        );
    }

    #[test]
    fn an_unknown_column_parks_the_whole_op_and_writes_nothing() {
        // A newer producer's column: this op cannot become a COMPLETE after-image here, so nothing
        // is written and the entry is left for the refold. Applying the known cell instead (the
        // pre-#1001 behavior) would leave a row no device authored and — once this binary learned
        // the column — re-author it with the hole at a winning lamport, destroying the real value
        // on every peer.
        let mut c = conn();
        let tx = c.transaction().unwrap();
        let out = apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[
                ("title", TypedValue::Text("kept".into())),
                ("future_col", TypedValue::Text("dropped".into())),
            ]),
            OpMeta { lamport: 1, device: device(2) },
        )
        .unwrap();
        assert_eq!(out, ApplyOutcome::Unprojectable(PendingReason::UnknownColumn));

        let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
        assert!(published_hash(&tx, "repo", "t_demo", &row_pk).unwrap().is_none());
        assert!(current_row_clock(&tx, "repo", "t_demo", &row_pk).unwrap().is_none());
        tx.commit().unwrap();
        assert_eq!(title(&c), None, "a parked op leaves the table untouched");
    }

    #[test]
    fn an_unknown_column_parks_without_disturbing_the_row_it_would_have_replaced() {
        // The dangerous variant: the row already exists from an earlier, fully-understood entry.
        // The parked op must not partially overwrite it, and must not touch its clock or
        // published hash — the existing row stays exactly the complete after-image its
        // author signed.
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("v1".into()))]),
            OpMeta { lamport: 5, device: device(2) },
        )
        .unwrap();
        let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
        let published_before = published_hash(&tx, "repo", "t_demo", &row_pk).unwrap();

        // A LATER op (higher lamport, would otherwise win) carrying an unknown column.
        let out = apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[
                ("title", TypedValue::Text("v2".into())),
                ("future_col", TypedValue::Text("unknown".into())),
            ]),
            OpMeta { lamport: 9, device: device(2) },
        )
        .unwrap();
        assert_eq!(out, ApplyOutcome::Unprojectable(PendingReason::UnknownColumn));
        assert_eq!(
            current_row_clock(&tx, "repo", "t_demo", &row_pk).unwrap().unwrap().0,
            5,
            "a parked op does not advance the row clock"
        );
        assert_eq!(
            published_hash(&tx, "repo", "t_demo", &row_pk).unwrap(),
            published_before,
            "a parked op does not touch the anti-echo record"
        );
        tx.commit().unwrap();
        assert_eq!(title(&c).as_deref(), Some("v1"), "the previous complete row survives intact");
    }

    #[test]
    fn a_published_hash_carries_the_column_set_it_covers() {
        // The hash alone is ambiguous across column-set changes, so the version it was recorded
        // under is stored with it. `produce` relies on this to tell "changed locally" from
        // "hashed under a different column set" (see `super::produce`).
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("v".into()))]),
            OpMeta { lamport: 1, device: device(2) },
        )
        .unwrap();
        let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
        let (_, version) = published_hash(&tx, "repo", "t_demo", &row_pk).unwrap().unwrap();
        assert_eq!(version, SPEC.spec_version);
    }

    #[test]
    fn a_remove_deletes_the_row_and_its_bookkeeping() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("x".into()))]),
            OpMeta { lamport: 1, device: device(2) },
        )
        .unwrap();
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &RowOp::Remove {
                spec_version: 1,
                table: "t_demo".into(),
                pk: vec![TypedValue::Text("r1".into())],
            },
            OpMeta { lamport: 2, device: device(2) },
        )
        .unwrap();
        let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
        assert!(published_hash(&tx, "repo", "t_demo", &row_pk).unwrap().is_none());
        assert!(current_row_clock(&tx, "repo", "t_demo", &row_pk).unwrap().is_none());
        tx.commit().unwrap();
        assert_eq!(title(&c), None);
    }

    fn remove() -> RowOp {
        RowOp::Remove {
            spec_version: 1,
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text("r1".to_string())],
        }
    }

    #[test]
    fn a_stale_remove_after_a_newer_upsert_does_not_delete() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("keep".into()))]),
            OpMeta { lamport: 5, device: device(2) },
        )
        .unwrap();
        // A delete older than the row's cell clock loses — the row survives.
        let out =
            apply_row_op(&tx, &SPEC, "repo", &remove(), OpMeta { lamport: 3, device: device(2) })
                .unwrap();
        assert_eq!(
            out,
            ApplyOutcome::Superseded,
            "the delete landed but did not delete: reported distinctly from a delete that took \
             effect, because a locally-authored op can never legitimately land here"
        );
        tx.commit().unwrap();
        assert_eq!(title(&c).as_deref(), Some("keep"), "a stale delete cannot remove a newer row");
    }

    #[test]
    fn an_upsert_older_than_a_remove_cannot_resurrect() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(&tx, &SPEC, "repo", &remove(), OpMeta { lamport: 5, device: device(2) })
            .unwrap();
        // An insert older than the tombstone is suppressed.
        let out = apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("ghost".into()))]),
            OpMeta { lamport: 3, device: device(2) },
        )
        .unwrap();
        assert_eq!(out, ApplyOutcome::Superseded, "suppressed by the tombstone, not applied");
        tx.commit().unwrap();
        assert_eq!(title(&c), None, "an insert older than the delete cannot resurrect the row");
    }

    #[test]
    fn an_upsert_newer_than_a_remove_resurrects() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(&tx, &SPEC, "repo", &remove(), OpMeta { lamport: 3, device: device(2) })
            .unwrap();
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("back".into()))]),
            OpMeta { lamport: 5, device: device(2) },
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(
            title(&c).as_deref(),
            Some("back"),
            "an insert newer than the delete resurrects"
        );
    }

    #[test]
    fn a_remove_and_upsert_converge_regardless_of_arrival_order() {
        let end_state = |ops: &[(RowOp, OpMeta)]| {
            let mut c = conn();
            let tx = c.transaction().unwrap();
            for (op, meta) in ops {
                apply_row_op(&tx, &SPEC, "repo", op, *meta).unwrap();
            }
            tx.commit().unwrap();
            title(&c)
        };
        let up = (upsert(&[("title", TypedValue::Text("v".into()))]), OpMeta {
            lamport: 5,
            device: device(2),
        });
        let rm = (remove(), OpMeta { lamport: 3, device: device(2) });
        assert_eq!(
            end_state(&[up.clone(), rm.clone()]),
            end_state(&[rm, up]),
            "the fold converges regardless of the order the delete and edit arrive",
        );
    }

    #[test]
    fn a_losing_upsert_does_not_publish_an_unsent_local_edit() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        // Establish + publish a row at lamport 5 (as authoring would).
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("v1".into()))]),
            OpMeta { lamport: 5, device: device(2) },
        )
        .unwrap();
        let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
        let published_before = published_hash(&tx, "repo", "t_demo", &row_pk).unwrap();
        // A local direct edit (no op authored yet): the row changes but stays unpublished.
        tx.execute("UPDATE t_demo SET title = 'local-edit' WHERE id = 'r1'", []).unwrap();
        // A STALE remote upsert (lower lamport) loses the LWW; it must NOT publish the local edit.
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("stale".into()))]),
            OpMeta { lamport: 3, device: device(9) },
        )
        .unwrap();
        assert_eq!(
            published_hash(&tx, "repo", "t_demo", &row_pk).unwrap(),
            published_before,
            "a losing op must not advance the published hash over an unsent local edit",
        );
        let current = synced_row_hash(&tx, &SPEC, &[TypedValue::Text("r1".into())]).unwrap();
        assert_ne!(
            current,
            published_before.map(|(hash, _version)| hash),
            "the local edit is still pending, not silently dropped"
        );
    }

    #[test]
    fn a_delete_races_the_row_write_clock_on_a_content_addressed_row() {
        // A content-hash-keyed row (the shape the old insert-only flag targeted) records a row
        // write clock like any other, so a stale delete loses and a newer one wins.
        const HASHED: TableSpec = TableSpec {
            name: "t_io",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("hash", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch("CREATE TABLE t_io(id TEXT PRIMARY KEY, hash TEXT) STRICT;").unwrap();
        let tx = c.transaction().unwrap();
        let io_upsert = RowOp::Upsert {
            spec_version: 1,
            table: "t_io".to_string(),
            pk: vec![TypedValue::Text("r".into())],
            cells: vec![Cell { column: "hash".into(), value: TypedValue::Text("h".into()) }],
        };
        let io_remove = RowOp::Remove {
            spec_version: 1,
            table: "t_io".to_string(),
            pk: vec![TypedValue::Text("r".into())],
        };

        apply_row_op(&tx, &HASHED, "repo", &io_upsert, OpMeta { lamport: 5, device: device(2) })
            .unwrap();
        // A stale remove (lamport 3) must NOT delete the newer row.
        apply_row_op(&tx, &HASHED, "repo", &io_remove, OpMeta { lamport: 3, device: device(2) })
            .unwrap();
        let count: i64 =
            tx.query_row("SELECT COUNT(*) FROM t_io WHERE id = 'r'", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "a stale delete cannot drop a newer row");
        // A newer remove (lamport 7) does delete it.
        apply_row_op(&tx, &HASHED, "repo", &io_remove, OpMeta { lamport: 7, device: device(2) })
            .unwrap();
        let after: i64 =
            tx.query_row("SELECT COUNT(*) FROM t_io WHERE id = 'r'", [], |r| r.get(0)).unwrap();
        assert_eq!(after, 0, "a delete newer than the row's write clock removes it");
    }

    #[test]
    fn an_op_naming_a_foreign_repo_is_quarantined() {
        const SCOPED: TableSpec = TableSpec {
            name: "t_scoped",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[
                ColumnSpec::required("repo_id", ValueType::Text),
                ColumnSpec::required("id", ValueType::Text),
            ],
            columns: &[ColumnSpec::required("title", ValueType::Text)],
            local_columns: &[],
            repo_column: Some("repo_id"),
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch(
            "CREATE TABLE t_scoped(
                 repo_id TEXT NOT NULL, id TEXT NOT NULL, title TEXT, PRIMARY KEY(repo_id, id)
             ) STRICT;",
        )
        .unwrap();
        let tx = c.transaction().unwrap();
        let foreign = RowOp::Upsert {
            spec_version: 1,
            table: "t_scoped".to_string(),
            pk: vec![TypedValue::Text("B".into()), TypedValue::Text("r1".into())],
            cells: vec![Cell { column: "title".into(), value: TypedValue::Text("x".into()) }],
        };
        // Applied on repo A's stream but naming repo B → rejected, nothing written.
        let out =
            apply_row_op(&tx, &SCOPED, "A", &foreign, OpMeta { lamport: 1, device: device(2) })
                .unwrap();
        assert!(matches!(out, ApplyOutcome::Quarantined(_)), "a foreign-repo op is rejected");
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM t_scoped", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "no cross-repo row was written");
        // The matching-repo op applies.
        let own = RowOp::Upsert {
            spec_version: 1,
            table: "t_scoped".to_string(),
            pk: vec![TypedValue::Text("A".into()), TypedValue::Text("r1".into())],
            cells: vec![Cell { column: "title".into(), value: TypedValue::Text("x".into()) }],
        };
        assert_eq!(
            apply_row_op(&tx, &SCOPED, "A", &own, OpMeta { lamport: 2, device: device(2) })
                .unwrap(),
            ApplyOutcome::Applied,
        );
    }

    #[test]
    fn a_null_primary_key_is_quarantined() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        let op = RowOp::Upsert {
            spec_version: 1,
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Null],
            cells: vec![Cell { column: "title".to_string(), value: TypedValue::Text("x".into()) }],
        };
        let out = apply_row_op(&tx, &SPEC, "repo", &op, OpMeta { lamport: 1, device: device(2) })
            .unwrap();
        assert!(matches!(out, ApplyOutcome::Quarantined(_)), "a null pk is not addressable");
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM t_demo", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "a quarantined null-pk op writes nothing");
    }

    #[test]
    fn a_pk_value_of_the_wrong_type_is_quarantined() {
        // `t_demo`'s pk `id` is declared Text. An I64 pk would take SQLite affinity onto a
        // different physical row than its type-exact `row_pk` clock identity — quarantine
        // before the WHERE.
        let mut c = conn();
        let tx = c.transaction().unwrap();
        let op = RowOp::Upsert {
            spec_version: 1,
            table: "t_demo".to_string(),
            pk: vec![TypedValue::I64(1)],
            cells: vec![Cell { column: "title".to_string(), value: TypedValue::Text("x".into()) }],
        };
        let out = apply_row_op(&tx, &SPEC, "repo", &op, OpMeta { lamport: 1, device: device(2) })
            .unwrap();
        assert!(
            matches!(out, ApplyOutcome::Quarantined(_)),
            "a pk value that disagrees with its declared type is quarantined"
        );
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM t_demo", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "a quarantined type-mismatched-pk op writes nothing");
    }

    #[test]
    fn a_null_in_a_not_null_column_is_quarantined_not_a_fatal_error() {
        // A NOT NULL constraint violation on a synced column must surface as a quarantine (the
        // entry is retained, the row untouched), never a hard error that would wedge the
        // ingest loop.
        const NOT_NULL: TableSpec = TableSpec {
            name: "t_nn",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("title", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch("CREATE TABLE t_nn(id TEXT PRIMARY KEY, title TEXT NOT NULL) STRICT;")
            .unwrap();
        let tx = c.transaction().unwrap();
        // A well-typed NULL cell (Text column, Null value) passes the type check but violates the
        // table's NOT NULL constraint on insert.
        let op = RowOp::Upsert {
            spec_version: 1,
            table: "t_nn".to_string(),
            pk: vec![TypedValue::Text("r".into())],
            cells: vec![Cell { column: "title".to_string(), value: TypedValue::Null }],
        };
        let out =
            apply_row_op(&tx, &NOT_NULL, "repo", &op, OpMeta { lamport: 1, device: device(2) })
                .unwrap();
        assert!(
            matches!(out, ApplyOutcome::Quarantined(_)),
            "a constraint violation is quarantined, not a fatal error"
        );
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM t_nn", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "a quarantined constraint-violating op writes nothing");
    }

    #[test]
    fn the_producer_reads_a_bool_pk_as_bool_and_the_applier_accepts_it() {
        // A `Bool` pk is stored as INTEGER 0/1. The producer must emit `TypedValue::Bool` (not
        // `I64`), or the op it signs fails the applier's typed-pk check and self-quarantines.
        const FLAG: TableSpec = TableSpec {
            name: "t_flag",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("active", ValueType::Bool)],
            columns: &[ColumnSpec::required("label", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let mut src = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&src, &crate::test_hooks()).unwrap();
        src.execute_batch("CREATE TABLE t_flag(active INTEGER PRIMARY KEY, label TEXT) STRICT;")
            .unwrap();
        src.execute("INSERT INTO t_flag(active, label) VALUES (1, 'on')", []).unwrap();
        let src_tx = src.transaction().unwrap();
        let rows = read_all_rows(&src_tx, &FLAG, "repo").unwrap();
        assert_eq!(rows.len(), 1);
        let ScannedRow::Readable { pk, cells } = &rows[0] else {
            panic!("a 0/1 Bool pk is readable");
        };
        assert_eq!(pk, &vec![TypedValue::Bool(true)], "a Bool pk is emitted as Bool, not I64");

        // The op the producer would sign applies cleanly on a peer (the typed-pk check passes).
        let op = RowOp::Upsert {
            spec_version: 1,
            table: "t_flag".to_string(),
            pk: pk.clone(),
            cells: cells.clone(),
        };
        let mut peer = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&peer, &crate::test_hooks()).unwrap();
        peer.execute_batch("CREATE TABLE t_flag(active INTEGER PRIMARY KEY, label TEXT) STRICT;")
            .unwrap();
        let peer_tx = peer.transaction().unwrap();
        assert_eq!(
            apply_row_op(&peer_tx, &FLAG, "repo", &op, OpMeta { lamport: 1, device: device(2) })
                .unwrap(),
            ApplyOutcome::Applied,
            "the applier accepts the Bool-pk op the producer emits",
        );
    }

    /// A table whose only synced column is a `Bool`, plus a store holding one row of it at `flag`.
    /// STRICT keeps every other column mapping total, so this is the one shape that can be
    /// unreadable (#1017).
    const FLAGGED: TableSpec = TableSpec {
        name: "t_flagged",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("flag", ValueType::Bool)],
        local_columns: &[],
        repo_column: None,
    };

    fn flagged_store(flag: i64) -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch("CREATE TABLE t_flagged(id TEXT PRIMARY KEY, flag INTEGER) STRICT;")
            .unwrap();
        c.execute("INSERT INTO t_flagged(id, flag) VALUES ('r', ?1)", [flag]).unwrap();
        c
    }

    #[test]
    fn a_bool_column_holding_a_non_boolean_int_is_unreadable_not_normalized() {
        // Coercing 2 to `true` would replicate a value that differs from the stored row. Reporting
        // it as unreadable is the alternative — and it must stay a VALUE, because every reader here
        // runs under a path that cannot fail (#1017).
        let mut c = flagged_store(2);
        let tx = c.transaction().unwrap();

        let rows = read_all_rows(&tx, &FLAGGED, "repo").unwrap();
        assert_eq!(rows.len(), 1, "the row is still scanned, not dropped or errored");
        match &rows[0] {
            ScannedRow::Unpublishable { pk } => {
                assert_eq!(pk, &vec![TypedValue::Text("r".into())], "it stays addressable")
            },
            _ => panic!("a Bool column holding 2 makes the row unpublishable, never `true`"),
        }
    }

    #[test]
    fn an_unreadable_row_is_distinguished_from_an_absent_one() {
        // The load-bearing distinction: the refold's guard reads `Absent` as a local delete
        // awaiting authorship and refuses to replay over it, so collapsing the two would
        // block the entry for a row that is merely unreadable.
        let mut c = flagged_store(2);
        let tx = c.transaction().unwrap();
        let present = read_synced_cells(&tx, &FLAGGED, &[TypedValue::Text("r".into())]).unwrap();
        assert!(matches!(present, SyncedRow::Unreadable(_)), "a present-but-unreadable row");

        let missing = read_synced_cells(&tx, &FLAGGED, &[TypedValue::Text("nope".into())]).unwrap();
        assert!(matches!(missing, SyncedRow::Absent), "and a genuinely absent one");
    }

    #[test]
    fn a_bool_pk_holding_a_non_boolean_int_leaves_the_row_unaddressable() {
        // The pk case is separate because the producer must treat it differently: with no readable
        // pk the row has no identity at all, so there is nothing to keep alive.
        const FLAG_PK: TableSpec = TableSpec {
            name: "t_flag",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("active", ValueType::Bool)],
            columns: &[ColumnSpec::required("label", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch("CREATE TABLE t_flag(active INTEGER PRIMARY KEY, label TEXT) STRICT;")
            .unwrap();
        c.execute("INSERT INTO t_flag(active, label) VALUES (2, 'on')", []).unwrap();
        let tx = c.transaction().unwrap();

        let rows = read_all_rows(&tx, &FLAG_PK, "repo").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0], ScannedRow::Unaddressable), "no pk means no identity");
    }

    /// A stable stream id for the guard tests, which never reach the winner lookup.
    fn guard_stream() -> StreamId {
        crate::table_sync::scope_stream::scope_stream_id(
            "repo",
            crate::AccountId::from_bytes([7; 32]),
            "demo/1",
        )
    }

    /// An op of each kind against the `FLAGGED` row, for the asymmetry below.
    fn flagged_op(remove: bool) -> RowOp {
        let pk = vec![TypedValue::Text("r".into())];
        if remove {
            RowOp::Remove { table: "t_flagged".to_string(), spec_version: 1, pk }
        } else {
            RowOp::Upsert {
                table: "t_flagged".to_string(),
                spec_version: 1,
                pk,
                cells: vec![Cell { column: "flag".to_string(), value: TypedValue::Bool(true) }],
            }
        }
    }

    #[test]
    fn an_upsert_may_replay_over_a_row_it_cannot_read() {
        // Both readers of "is there unsent work here" must not defer: the producer cannot author an
        // unreadable row either, so deferring every op would leave the row unauthorable AND
        // permanently block its own pending entries. The upsert has a floor — it still has to win
        // on the clock, and a winner rewrites the column that is unreadable.
        let mut c = flagged_store(2);
        let tx = c.transaction().unwrap();
        assert_eq!(
            unsent_work_blocking_replay(&tx, &FLAGGED, "repo", guard_stream(), &flagged_op(false))
                .unwrap(),
            None,
            "an unreadable row proves nothing against an upsert that would repair it",
        );
    }

    #[test]
    fn a_remove_may_not_replay_over_a_row_it_cannot_read() {
        // The asymmetry. A remove deletes the row outright — local-only columns included — and
        // repairs nothing, so letting it through would destroy an unsent local edit that merely
        // happens to be unreadable. Deferring is the safe stuck state: the row survives, and the
        // entry replays on the merits once the cell is repaired.
        let mut c = flagged_store(2);
        let tx = c.transaction().unwrap();
        assert_eq!(
            unsent_work_blocking_replay(&tx, &FLAGGED, "repo", guard_stream(), &flagged_op(true))
                .unwrap(),
            Some(PendingReason::DeferredUnreadableRow),
            "a remove over an unreadable row has no repair to offer, so it must defer",
        );
    }

    #[test]
    fn a_text_column_holding_invalid_utf8_is_unreadable_too() {
        // STRICT pins the storage CLASS, not the value's domain within it: `CAST(X'80' AS TEXT)`
        // stores with `typeof() = 'text'` and fails the moment it is read as a `String`. Reading it
        // as an error would fail the store open exactly the way a malformed Bool used to, so the
        // mapping is total over (declared type, storage class) rather than argued from STRICT.
        const LABELLED: TableSpec = TableSpec {
            name: "t_labelled",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("label", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch(
            "CREATE TABLE t_labelled(id TEXT PRIMARY KEY, label TEXT) STRICT;
             INSERT INTO t_labelled(id, label) VALUES ('r', CAST(X'80' AS TEXT));",
        )
        .unwrap();
        let tx = c.transaction().unwrap();

        let cells = read_synced_cells(&tx, &LABELLED, &[TypedValue::Text("r".into())]).unwrap();
        assert!(
            matches!(cells, SyncedRow::Unreadable(_)),
            "invalid UTF-8 is unreadable, not an error"
        );
        let rows = read_all_rows(&tx, &LABELLED, "repo").unwrap();
        assert!(
            matches!(&rows[0], ScannedRow::Unpublishable { .. }),
            "and the scan carries the row rather than failing the pass",
        );
    }

    #[test]
    fn a_storage_class_that_does_not_match_the_declared_type_is_unreadable() {
        // Unreachable on a STRICT table — which is exactly the argument that let the previous
        // version fail an open, so the mapping covers it as a value instead of assuming it away.
        const LABELLED: TableSpec = TableSpec {
            name: "t_loose",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("label", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        // No STRICT here: that is what lets an INTEGER land in a column declared TEXT.
        c.execute_batch(
            "CREATE TABLE t_loose(id TEXT PRIMARY KEY, label BLOB);
             INSERT INTO t_loose(id, label) VALUES ('r', 7);",
        )
        .unwrap();
        let tx = c.transaction().unwrap();
        assert!(
            matches!(
                read_synced_cells(&tx, &LABELLED, &[TypedValue::Text("r".into())]).unwrap(),
                SyncedRow::Unreadable(_)
            ),
            "a mismatched storage class is carried, not raised",
        );
    }

    #[test]
    fn a_remove_blocked_by_a_foreign_key_is_quarantined_not_wedged() {
        // A delete that hits an FK RESTRICT (a child references the row) must quarantine — NOT
        // return a hard error, which would roll back the already-stored entry and wedge the chain.
        const PARENT: TableSpec = TableSpec {
            name: "parent",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE parent(id TEXT NOT NULL PRIMARY KEY, v TEXT) STRICT;
             CREATE TABLE child(id TEXT NOT NULL PRIMARY KEY, p TEXT NOT NULL REFERENCES \
             parent(id)) STRICT;
             INSERT INTO parent(id, v) VALUES ('r', 'x');
             INSERT INTO child(id, p) VALUES ('c', 'r');",
        )
        .unwrap();
        let tx = c.transaction().unwrap();
        let op = RowOp::Remove {
            spec_version: 1,
            table: "parent".to_string(),
            pk: vec![TypedValue::Text("r".into())],
        };
        let out = apply_row_op(&tx, &PARENT, "repo", &op, OpMeta { lamport: 1, device: device(2) })
            .unwrap();
        assert!(
            matches!(out, ApplyOutcome::Quarantined(_)),
            "an FK-blocked delete quarantines, it does not error: {out:?}"
        );
        let n: i64 =
            tx.query_row("SELECT COUNT(*) FROM parent WHERE id = 'r'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "the FK-blocked parent row is left untouched");
    }
}
