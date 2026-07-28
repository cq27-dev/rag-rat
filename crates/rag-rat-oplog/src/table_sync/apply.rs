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

use super::registry::{TableSpec, ValueType};
use super::row_op::{self, Cell, RowOp, TypedValue};
use super::store::PendingReason;
use crate::op::OpMeta;

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
    let pk_vals = op.pk();
    if pk_vals.len() != spec.pk.len() {
        return Ok(ApplyOutcome::Quarantined(format!(
            "pk arity {} does not match `{}`'s {} identity columns",
            pk_vals.len(),
            spec.name,
            spec.pk.len()
        )));
    }
    // A NULL identity value is unaddressable: `WHERE pk = NULL` never matches, so upserts would
    // insert duplicate unreachable rows and removes could never delete them. Reject the whole op.
    if pk_vals.iter().any(|v| matches!(v, TypedValue::Null)) {
        return Ok(ApplyOutcome::Quarantined(format!(
            "a null primary-key value is not addressable on `{}`",
            spec.name
        )));
    }
    // Validate each pk value against its declared type before it reaches a WHERE clause. SQLite
    // affinity would otherwise coerce a mismatched pk (e.g. `I64(1)` matching a TEXT key `'1'`)
    // onto a different physical row than its type-exact `row_pk` clock identity, splitting the
    // row's bookkeeping and allowing resurrection. (Arity is checked above, so `zip` covers
    // every pk.)
    for (column, value) in spec.pk.iter().zip(pk_vals) {
        if !value_matches(value, column.value_type) {
            return Ok(ApplyOutcome::Quarantined(format!(
                "pk column `{}` value does not match its declared type on `{}`",
                column.name, spec.name
            )));
        }
    }
    // Repo-identity gate: for a table scoped by a pk column, an op naming a different repo than the
    // stream being synced is rejected — a peer cannot write another project's rows through this
    // stream. (The producer already only emits the local repo's rows.)
    if let Some(idx) = spec.repo_pk_index()
        && pk_vals.get(idx) != Some(&TypedValue::Text(repo_id.to_string()))
    {
        return Ok(ApplyOutcome::Quarantined(format!(
            "op names a different repo than the `{}` stream being synced",
            spec.name
        )));
    }
    match op {
        RowOp::Remove { .. } => apply_remove(tx, spec, repo_id, pk_vals, meta),
        RowOp::Upsert { cells, .. } => apply_upsert(tx, spec, repo_id, pk_vals, cells, meta),
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
    Ok(ApplyOutcome::Applied)
}

fn apply_upsert(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    pk_vals: &[TypedValue],
    cells: &[Cell],
    meta: OpMeta,
) -> anyhow::Result<ApplyOutcome> {
    // FORWARD COMPATIBILITY, decided on the PAYLOAD ALONE (before any row state is read): an op
    // naming a column this registry does not know cannot be projected into a complete after-image.
    // PARK it — store the entry, write NOTHING — rather than applying the subset we understand.
    //
    // Applying the known cells and skipping the rest would leave a row that NO device ever authored
    // ({known: new, unknown: whatever this row already held}), and recording its anti-echo hash
    // would then claim that chimera as a complete projection. Two failures follow: the skipped
    // value is unrecoverable locally (redelivery short-circuits on `entry_exists`), and after an
    // upgrade that learns the column the producer re-authors the row with the HOLE at a fresh
    // winning lamport — destroying the real value on every peer. Parking makes the incomplete
    // projection unrepresentable instead of containing it after the fact (#1001).
    if let Some(unknown) =
        cells.iter().find(|cell| !spec.columns.iter().any(|known| known.name == cell.column))
    {
        debug_assert!(!unknown.column.is_empty());
        return Ok(ApplyOutcome::Unprojectable(PendingReason::UnknownColumn));
    }

    let row_pk = &row_op::row_pk_string(pk_vals);
    let device_hex = &meta.device.to_string();

    // A row deleted at a clock this op cannot beat stays deleted: the delete is newer than this
    // edit, so the edit must not resurrect the row. (Suppressed, but the entry is still stored, so
    // redelivery stays idempotent.)
    if let Some((t_lamport, t_device)) = current_tombstone(tx, repo_id, spec.name, row_pk)?
        && !beats(meta.lamport, device_hex, t_lamport, &t_device)
    {
        return Ok(ApplyOutcome::Applied);
    }

    // Type-check each cell. Every cell's column is known here — an unknown one parked the whole op
    // above, before any row state was touched. A type mismatch quarantines the op before any write.
    let mut known: Vec<(&str, &TypedValue)> = Vec::new();
    for cell in cells {
        let Some(column) = spec.columns.iter().find(|c| c.name == cell.column) else {
            debug_assert!(false, "an unknown column parks the op before any cell is classified");
            return Ok(ApplyOutcome::Unprojectable(PendingReason::UnknownColumn));
        };
        if !value_matches(&cell.value, column.value_type) {
            return Ok(ApplyOutcome::Quarantined(format!(
                "cell `{}` value does not match declared type on `{}`",
                cell.column, spec.name
            )));
        }
        known.push((column.name, &cell.value));
    }

    // Whole-row LWW requires a full after-image: a winning op replaces the ENTIRE row, so an op
    // missing a synced column would leave that column at a stale value under the winning clock —
    // two peers with different prior values there would diverge. The producer always emits every
    // synced column (`read_all_rows`), so a partial op is malformed or a version-skew op that
    // cannot cleanly replace this binary's row; quarantine it rather than half-apply. (Decode
    // rejects duplicate columns and an unknown column parks the op, so `known` holds distinct
    // synced columns — a full count means all are present.)
    // PARK, not quarantine: the overwhelmingly likely cause is an OLDER producer whose complete row
    // under its narrower spec is a partial one under ours — a version gap, redeemed from the
    // sender's side by #1002's declared column defaults, not a broken producer whose data can never
    // fit. Quarantining would drop it off the refold's worklist permanently; parking keeps it
    // outstanding so the binary that can rebuild the missing cells lands it.
    if known.len() != spec.columns.len() {
        return Ok(ApplyOutcome::Unprojectable(PendingReason::PartialAfterImage));
    }

    // Whole-row LWW: the op wins the ENTIRE row iff it beats the row's write clock (or the row is
    // new). A losing op is a no-op — it never partially overwrites, and it must not touch the
    // published hash (that would mark an unsent local edit as sent and make the producer drop it).
    let wins = match current_row_clock(tx, repo_id, spec.name, row_pk)? {
        Some((c_lamport, c_device)) => beats(meta.lamport, device_hex, c_lamport, &c_device),
        None => true, // no prior write — this op establishes the row.
    };
    if !wins {
        return Ok(ApplyOutcome::Applied);
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
        record_published(tx, repo_id, spec.name, row_pk, &hash)?;
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
    Ok(read_synced_cells(tx, spec, pk_vals)?.map(|cells| row_op::cells_hash(&cells)))
}

/// Read a row's synced columns as typed cells (sorted by the registry's column order), mapping each
/// stored value back to its declared type so the hash matches what the applier wrote. `None` if the
/// row is absent.
pub(crate) fn read_synced_cells(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    pk_vals: &[TypedValue],
) -> anyhow::Result<Option<Vec<Cell>>> {
    let select = spec.columns.iter().map(|c| quote_ident(c.name)).collect::<Vec<_>>().join(", ");
    let sql =
        format!("SELECT {select} FROM {} WHERE {} LIMIT 1", quote_ident(spec.name), pk_where(spec));
    let cells = tx
        .query_row(&sql, params_from_iter(pk_params(pk_vals)), |row| {
            let mut cells = Vec::with_capacity(spec.columns.len());
            for (idx, column) in spec.columns.iter().enumerate() {
                cells.push(Cell {
                    column: column.name.to_string(),
                    value: read_typed(row, idx, column.value_type)?,
                });
            }
            Ok(cells)
        })
        .optional()?;
    Ok(cells)
}

/// Every current row of `spec`'s table FOR `repo_id` as `(pk values, synced cells)`, the producer's
/// scan input. A repo-scoped table is filtered by its `repo_column`, so a multi-repo store never
/// emits one repo's rows into another repo's stream. Pk values are read by their runtime storage
/// type (identities are text/int/blob); synced cells by their declared type so the hash matches the
/// applier's.
pub(crate) fn read_all_rows(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
) -> anyhow::Result<Vec<(Vec<TypedValue>, Vec<Cell>)>> {
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
            // Read each pk value by its DECLARED type, not its storage type: a `Bool` pk is stored
            // as INTEGER 0/1, and reading it as `I64` would emit a `TypedValue` the applier's
            // typed- pk check rejects — the producer would then sign ops its own
            // self-apply quarantines.
            let mut pk = Vec::with_capacity(spec.pk.len());
            for (idx, column) in spec.pk.iter().enumerate() {
                pk.push(read_typed(row, idx, column.value_type)?);
            }
            let mut cells = Vec::with_capacity(spec.columns.len());
            for (offset, column) in spec.columns.iter().enumerate() {
                cells.push(Cell {
                    column: column.name.to_string(),
                    value: read_typed(row, spec.pk.len() + offset, column.value_type)?,
                });
            }
            Ok((pk, cells))
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
    winning: &[(&str, &TypedValue)],
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
    cells: &[(&str, &TypedValue)],
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
) -> anyhow::Result<Option<(String, i64)>> {
    Ok(tx
        .query_row(
            "SELECT synced_hash, projector_version FROM sync_published_rows
             WHERE repo_id = ?1 AND table_name = ?2 AND row_pk = ?3",
            rusqlite::params![repo_id, table, row_pk],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?)
}

/// Whether this row holds a local change that has not been authored yet, so a caller replaying an
/// older retained entry over it would DESTROY work no peer has seen.
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
pub(crate) fn row_has_unsent_local_change(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    pk_vals: &[TypedValue],
) -> anyhow::Result<bool> {
    let Some(current) = synced_row_hash(tx, spec, pk_vals)? else {
        // No row: nothing local to lose (the entry is establishing it).
        return Ok(false);
    };
    let row_pk = row_op::row_pk_string(pk_vals);
    Ok(match published_hash(tx, repo_id, spec.name, &row_pk)? {
        // Comparable: a differing hash is a demonstrably unsent local change.
        Some((published, version)) if version == super::refold::TABLE_SYNC_PROJECTOR_VERSION =>
            published != current,
        // Published under a different column set — not comparable, so nothing is proven either way.
        Some(_) => false,
        // A live row no apply ever published is purely local: the only content there came from this
        // device, and no peer has seen it.
        None => true,
    })
}

/// Claim `row_pk` as a COMPLETE projection: `hash` covers every synced column this binary knows,
/// stamped with the projector version that defines that column set.
pub(crate) fn record_published(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
    row_pk: &str,
    hash: &str,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO sync_published_rows(repo_id, table_name, row_pk, synced_hash,
                                         projector_version)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(repo_id, table_name, row_pk) DO UPDATE
             SET synced_hash = excluded.synced_hash,
                 projector_version = excluded.projector_version",
        rusqlite::params![
            repo_id,
            table,
            row_pk,
            hash,
            super::refold::TABLE_SYNC_PROJECTOR_VERSION
        ],
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

fn read_typed(row: &rusqlite::Row<'_>, idx: usize, vt: ValueType) -> rusqlite::Result<TypedValue> {
    match vt {
        ValueType::Text =>
            Ok(row.get::<_, Option<String>>(idx)?.map_or(TypedValue::Null, TypedValue::Text)),
        ValueType::I64 =>
            Ok(row.get::<_, Option<i64>>(idx)?.map_or(TypedValue::Null, TypedValue::I64)),
        // A Bool column must hold exactly 0 or 1. Coercing any other integer to `true` (the old
        // `n != 0`) would silently rewrite the source value to 1 on self-apply and replicate a
        // value that differs from the row — surface the malformed value instead of
        // normalizing it away.
        ValueType::Bool => match row.get::<_, Option<i64>>(idx)? {
            None => Ok(TypedValue::Null),
            Some(0) => Ok(TypedValue::Bool(false)),
            Some(1) => Ok(TypedValue::Bool(true)),
            Some(other) => Err(rusqlite::Error::FromSqlConversionFailure(
                idx,
                rusqlite::types::Type::Integer,
                format!("a Bool column holds {other}, not 0 or 1").into(),
            )),
        },
        ValueType::Blob =>
            Ok(row.get::<_, Option<Vec<u8>>>(idx)?.map_or(TypedValue::Null, TypedValue::Blob)),
    }
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
        pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
        columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }],
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
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }, ColumnSpec {
                name: "count",
                value_type: ValueType::I64,
            }],
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
        apply_row_op(&tx, &TWO_COL, "repo", &full("B", 2), OpMeta {
            lamport: 5,
            device: device(1),
        })
        .unwrap();
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
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }, ColumnSpec {
                name: "count",
                value_type: ValueType::I64,
            }],
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
        assert_eq!(version, super::super::refold::TABLE_SYNC_PROJECTOR_VERSION);
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
            &RowOp::Remove { table: "t_demo".into(), pk: vec![TypedValue::Text("r1".into())] },
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
        RowOp::Remove { table: "t_demo".to_string(), pk: vec![TypedValue::Text("r1".to_string())] }
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
        apply_row_op(&tx, &SPEC, "repo", &remove(), OpMeta { lamport: 3, device: device(2) })
            .unwrap();
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
        apply_row_op(
            &tx,
            &SPEC,
            "repo",
            &upsert(&[("title", TypedValue::Text("ghost".into()))]),
            OpMeta { lamport: 3, device: device(2) },
        )
        .unwrap();
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
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "hash", value_type: ValueType::Text }],
            local_columns: &[],
            repo_column: None,
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch("CREATE TABLE t_io(id TEXT PRIMARY KEY, hash TEXT) STRICT;").unwrap();
        let tx = c.transaction().unwrap();
        let io_upsert = RowOp::Upsert {
            table: "t_io".to_string(),
            pk: vec![TypedValue::Text("r".into())],
            cells: vec![Cell { column: "hash".into(), value: TypedValue::Text("h".into()) }],
        };
        let io_remove =
            RowOp::Remove { table: "t_io".to_string(), pk: vec![TypedValue::Text("r".into())] };

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
            pk: &[ColumnSpec { name: "repo_id", value_type: ValueType::Text }, ColumnSpec {
                name: "id",
                value_type: ValueType::Text,
            }],
            columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }],
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
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }],
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
            pk: &[ColumnSpec { name: "active", value_type: ValueType::Bool }],
            columns: &[ColumnSpec { name: "label", value_type: ValueType::Text }],
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
        assert_eq!(
            rows[0].0,
            vec![TypedValue::Bool(true)],
            "a Bool pk is emitted as Bool, not I64"
        );

        // The op the producer would sign applies cleanly on a peer (the typed-pk check passes).
        let op = RowOp::Upsert {
            table: "t_flag".to_string(),
            pk: rows[0].0.clone(),
            cells: rows[0].1.clone(),
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

    #[test]
    fn a_bool_column_holding_a_non_boolean_int_is_rejected_not_normalized() {
        // A `Bool` column that somehow holds an integer other than 0/1 must be surfaced, not
        // coerced to `true` — coercing would replicate a value (1) that differs from the
        // stored row.
        const FLAGGED: TableSpec = TableSpec {
            name: "t_flagged",
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "flag", value_type: ValueType::Bool }],
            local_columns: &[],
            repo_column: None,
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch("CREATE TABLE t_flagged(id TEXT PRIMARY KEY, flag INTEGER) STRICT;")
            .unwrap();
        c.execute("INSERT INTO t_flagged(id, flag) VALUES ('r', 2)", []).unwrap();
        let tx = c.transaction().unwrap();
        assert!(
            read_all_rows(&tx, &FLAGGED, "repo").is_err(),
            "a Bool column holding 2 is rejected, not silently normalized to true",
        );
    }

    #[test]
    fn a_remove_blocked_by_a_foreign_key_is_quarantined_not_wedged() {
        // A delete that hits an FK RESTRICT (a child references the row) must quarantine — NOT
        // return a hard error, which would roll back the already-stored entry and wedge the chain.
        const PARENT: TableSpec = TableSpec {
            name: "parent",
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }],
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
        let op =
            RowOp::Remove { table: "parent".to_string(), pk: vec![TypedValue::Text("r".into())] };
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
