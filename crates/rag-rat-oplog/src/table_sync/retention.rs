//! Retention for the accepted table-sync log: compact a per-(stream, device) chain prefix below
//! a recorded floor (#1127).
//!
//! Anchors/1 launches fully retained; the first high-churn scope cannot. Compaction here drops
//! accepted entries with `lamport < floor` for ONE device chain — and only SUPERSEDED ones. The
//! invariant (#1277): every entry that still carries a row's merge state stays retained. Those
//! are the chain's *pins* ([`chain_pins`]): a live whole-row winner, and this chain's newest
//! statement of a tombstone whose pk has no live row (`sync_tombstone_statements`, #1295). A
//! fresh peer folds only the retained suffix, so a dropped live winner is a row it never
//! receives; a re-rooting peer (below) never sees the region between its old tip and the floor,
//! so a dropped statement is a deleted row it keeps. A tombstone whose pk is live again is not a
//! pin: anything it would suppress also loses to the live clock. [`compact_chain_prefix`] refuses
//! a floor above a pin; the driver (`table_sync_compact_overdue`) clamps its floor at the oldest
//! pin, and on this device's own chain first carries the oldest pins to the tail — a live row
//! re-authored, its stated deletes restated in batches — when that frees at least twice what it
//! authors. A chain's statement moves only when its own writer restates, so no chain's delivery
//! of a delete ever depends on another chain.
//!
//! The floor is recorded durably (`table_sync_retained_floors`) and advertised on the wire: a
//! FRESH peer (no local chain) accepts the floor entry as its local root on exact
//! `(lamport, hash)` match and records the floor, so the signal propagates transitively. Adoption
//! is bounded by the chain-tip witness: a purged chain's retained tip is chain state, and a floor
//! at or below a different witness entry classifies as the equivocation it is. A re-offered entry
//! below the floor is idempotently ignored instead of classifying as a fork against the retained
//! tail. The driver counts and floors only reclaimable entries, clamps at the lowest pending
//! lamport so forward-compat payloads stay offerable, treats a non-advancing floor as the
//! steady-state no-op, and stays inert for anchors/1.
//!
//! A peer whose accepted TIP fell below the sender's floor (offline while the scope churned
//! past it) recovers by re-rooting: the session plans the offer AT the floor instead of a suffix
//! the peer can never link, and the receiver adopts the floor as its new root on exact coordinate
//! match. The old prefix stays stored (projections are unaffected), the tail advances, and
//! below-floor re-offers read idempotent. A peer forked AT the floor lamport gets ordinary fork
//! classification, same as at any other tip. Re-rooting also discards fork evidence in a receiver's
//! divergent below-floor prefix, even when that receiver had not compacted it.
//!
//! Accepted horizons, named rather than hidden:
//!
//! - **Pins are the retention floor.** A chain holds at least its pins, whatever the budget: live
//!   rows past the budget, or its statements of orphan tombstones, keep the chain over budget
//!   honestly instead of dropping what a peer needs. A foreign chain is never re-authored — only
//!   its writer can carry its rows forward — so it reclaims only the superseded prefix below its
//!   oldest pin. Tombstone rows and their statements are permanent: a chain that is all statements
//!   packed at the tail is its irreducible footprint.
//! - **A re-authored pin is a new write.** It carries the row's current cells at the tail, so it
//!   competes under LWW with a concurrent edit to that row this device has not received yet, and
//!   can win it — the cost re-adoption already accepts.
//! - **Below-floor equivocations are undetectable after a floor is recorded.** The accept path
//!   answers `AlreadyPresent` for any entry below the floor, so the peer discards fork evidence a
//!   peer without that floor would still classify. Accepted: `Fork` is non-storing and a
//!   below-floor entry can never apply, so the divergence costs evidence, never convergence — and
//!   evidence loss is the price of reclaiming the region the evidence would describe.

use rusqlite::{OptionalExtension, Transaction, params};

use crate::op::DeviceFingerprint;
use crate::stream::{EntryHash, StreamId};

/// What one prefix compaction did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactionReport {
    /// Accepted entries dropped from the chain prefix.
    pub dropped_entries: usize,
    /// Gapped entries swept below the floor — they can never promote once their predecessor is
    /// reclaimed, so holding them would only burn the per-chain capacity cap.
    pub swept_gapped: usize,
}

/// The recorded retained floor for one device chain, if any.
pub(crate) fn retained_floor(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: DeviceFingerprint,
) -> anyhow::Result<Option<u64>> {
    let row: Option<i64> = tx
        .query_row(
            "SELECT lamport FROM table_sync_retained_floors
             WHERE stream_id = ?1 AND device_fingerprint = ?2",
            params![stream.to_bytes().as_slice(), device.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    row.map(u64::try_from).transpose().map_err(Into::into)
}

/// Record a floor a PEER advertised and this store just accepted as a chain root (#1127 slice b).
/// Monotonic like the compaction record: an adopted floor can only advance. This is what makes
/// the signal transitive — this peer's own re-offers present the same floor to third parties.
///
/// Also sweeps gapped entries below the adopted floor, mirroring the compaction-side sweep: a
/// below-floor entry parked by in-session reordering can never promote once the floor is adopted
/// (its predecessor reports `AlreadyPresent` via the below-floor early return, so no parent
/// acceptance ever probes it), and without the sweep it burns per-chain gapped capacity forever.
pub(crate) fn record_adopted_floor(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: DeviceFingerprint,
    lamport: u64,
    entry_hash: EntryHash,
    now_ms: i64,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO table_sync_retained_floors(
             stream_id, device_fingerprint, lamport, entry_hash, compacted_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(stream_id, device_fingerprint) DO UPDATE SET
             lamport = excluded.lamport,
             entry_hash = excluded.entry_hash,
             compacted_at_ms = excluded.compacted_at_ms
         WHERE excluded.lamport > table_sync_retained_floors.lamport",
        params![
            stream.to_bytes().as_slice(),
            device.to_bytes().as_slice(),
            i64::try_from(lamport)?,
            entry_hash.as_slice(),
            now_ms,
        ],
    )?;
    tx.execute(
        "DELETE FROM table_sync_gapped_entries
         WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport < ?3",
        params![
            stream.to_bytes().as_slice(),
            device.to_bytes().as_slice(),
            i64::try_from(lamport)?
        ],
    )?;
    Ok(())
}

/// One entry on a device chain that still carries merge state, so compaction never drops it while
/// the pin stands: the live whole-row winner it wrote, or this chain's statement of one or more
/// tombstones whose pk has no live row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pin {
    pub lamport: u64,
    pub kind: PinKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PinKind {
    /// The entry's upsert owns the row.
    LiveRow { table_name: String, row_pk: String },
    /// The entry is this chain's newest statement of each of these current orphan tombstones — a
    /// `Remove` states one, a `Restate` many.
    Statements(Vec<StatedRow>),
}

/// A tombstone one chain states: the row, and the delete identity it competes under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatedRow {
    pub table_name: String,
    pub row_pk: String,
    /// The identity's device, in the lowercase hex the merge tables use.
    pub device_hex: String,
    pub lamport: u64,
}

impl Pin {
    /// How many rows the pin holds — one for a live row, one per stated tombstone.
    pub(crate) fn held(&self) -> usize {
        match &self.kind {
            PinKind::LiveRow { .. } => 1,
            PinKind::Statements(rows) => rows.len(),
        }
    }
}

/// The pins on `device`'s chain in `stream` with `from <= lamport < below`, oldest first, at most
/// `limit` of them — one per entry, its stated rows gathered.
pub(crate) fn chain_pins(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: DeviceFingerprint,
    from: u64,
    below: u64,
    limit: usize,
) -> anyhow::Result<Vec<Pin>> {
    // The merge tables store fingerprints as the lowercase hex the applier wrote. A live clock owns
    // its row whatever tombstone sits beside it; a statement pins only while its tombstone is
    // current (a statement never outlives its tombstone row — the join is that guarantee) and the
    // pk has no live clock.
    let mut stmt = tx.prepare(
        "SELECT lamport, table_name, row_pk, NULL, NULL FROM sync_row_clocks
         WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport >= ?3 AND lamport < ?4
         UNION ALL
         SELECT s.lamport, s.table_name, s.row_pk, t.device_fingerprint, t.lamport
           FROM sync_tombstone_statements s
           JOIN sync_row_tombstones t
             ON t.stream_id = s.stream_id AND t.table_name = s.table_name AND t.row_pk = s.row_pk
         WHERE s.stream_id = ?1 AND s.device_fingerprint = ?2 AND s.lamport >= ?3
           AND s.lamport < ?4
           AND NOT EXISTS (
               SELECT 1 FROM sync_row_clocks c
               WHERE c.stream_id = s.stream_id AND c.table_name = s.table_name
                 AND c.row_pk = s.row_pk
           )
         ORDER BY 1, 2, 3",
    )?;
    // Stepped lazily: a caller wanting only the oldest pin stops as soon as the next entry
    // begins, without materialising the rest.
    let rows = stmt.query_map(
        params![
            stream.to_bytes().as_slice(),
            device.to_string(),
            i64::try_from(from)?,
            i64::try_from(below)?,
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        },
    )?;
    let mut pins: Vec<Pin> = Vec::new();
    for row in rows {
        let (lamport, table_name, row_pk, identity_device, identity_lamport) = row?;
        let lamport = u64::try_from(lamport)?;
        let stated = match (identity_device, identity_lamport) {
            (Some(device_hex), Some(identity_lamport)) => Some(StatedRow {
                table_name: table_name.clone(),
                row_pk: row_pk.clone(),
                device_hex,
                lamport: u64::try_from(identity_lamport)?,
            }),
            _ => None,
        };
        match (pins.last_mut(), stated) {
            // One entry states many tombstones; one entry writes one row.
            (Some(Pin { lamport: last, kind: PinKind::Statements(rows) }), Some(row))
                if *last == lamport =>
                rows.push(row),
            (_, Some(row)) => {
                if pins.len() >= limit {
                    break;
                }
                pins.push(Pin { lamport, kind: PinKind::Statements(vec![row]) });
            },
            (_, None) => {
                if pins.len() >= limit {
                    break;
                }
                pins.push(Pin { lamport, kind: PinKind::LiveRow { table_name, row_pk } });
            },
        }
    }
    Ok(pins)
}

/// Drop every accepted entry on `device`'s chain in `stream` with `lamport < floor_lamport`.
///
/// The floor entry itself is RETAINED and must exist on the chain — an arbitrary lamport cannot
/// invent a floor that no peer could ever be pointed at. Entries retained for replay
/// (`pending_reason IS NOT NULL`) are never compacted: their payloads are owed to a later binary,
/// and dropping them would silently lose the forward-compat contract. A floor above a pin is
/// refused (#1277): only superseded entries are reclaimable. The check covers the window this
/// compaction reclaims — a store compacted before the pin rule may already hold pins below its
/// recorded floor, whose entries are gone; the driver re-authors those on the local chain.
pub(crate) fn compact_chain_prefix(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: DeviceFingerprint,
    floor_lamport: u64,
    now_ms: i64,
) -> anyhow::Result<CompactionReport> {
    let current = retained_floor(tx, stream, device)?;
    if let Some(current) = current
        && floor_lamport <= current
    {
        anyhow::bail!(
            "table-sync compaction floor {floor_lamport} does not advance the retained floor \
             {current} — a retreating compaction is a caller bug, not a no-op"
        );
    }
    let floor_entry: Option<(Vec<u8>,)> = tx
        .query_row(
            "SELECT entry_hash FROM table_sync_entries
             WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport = ?3",
            params![
                stream.to_bytes().as_slice(),
                device.to_bytes().as_slice(),
                i64::try_from(floor_lamport)?
            ],
            |row| Ok((row.get::<_, Vec<u8>>(0)?,)),
        )
        .optional()?;
    let Some((floor_hash,)) = floor_entry else {
        anyhow::bail!(
            "table-sync compaction floor {floor_lamport} names no entry on the chain — the floor \
             must be a retained entry a peer can be pointed at"
        );
    };
    if let Some(pin) =
        chain_pins(tx, stream, device, current.unwrap_or(0), floor_lamport, 1)?.first()
    {
        let (what, table_name, row_pk) = match &pin.kind {
            PinKind::LiveRow { table_name, row_pk } => ("carries", table_name, row_pk),
            PinKind::Statements(rows) =>
                ("states the delete of", &rows[0].table_name, &rows[0].row_pk),
        };
        anyhow::bail!(
            "table-sync compaction floor {floor_lamport} would drop the entry at lamport {} that \
             still {what} `{table_name}` row {row_pk} — only superseded entries are reclaimable",
            pin.lamport,
        );
    }
    let dropped_entries = tx.execute(
        "DELETE FROM table_sync_entries
         WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport < ?3
           AND pending_reason IS NULL",
        params![
            stream.to_bytes().as_slice(),
            device.to_bytes().as_slice(),
            i64::try_from(floor_lamport)?
        ],
    )?;
    // A gapped entry below the floor can never promote: its predecessor is part of the reclaimed
    // prefix and no honest sender re-offers it. Sweeping here is what keeps a stalled chain from
    // burning its per-chain gapped capacity forever.
    let swept_gapped = tx.execute(
        "DELETE FROM table_sync_gapped_entries
         WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport < ?3",
        params![
            stream.to_bytes().as_slice(),
            device.to_bytes().as_slice(),
            i64::try_from(floor_lamport)?
        ],
    )?;
    tx.execute(
        "INSERT INTO table_sync_retained_floors(
             stream_id, device_fingerprint, lamport, entry_hash, compacted_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(stream_id, device_fingerprint) DO UPDATE SET
             lamport = excluded.lamport,
             entry_hash = excluded.entry_hash,
             compacted_at_ms = excluded.compacted_at_ms
         WHERE excluded.lamport > table_sync_retained_floors.lamport",
        params![
            stream.to_bytes().as_slice(),
            device.to_bytes().as_slice(),
            i64::try_from(floor_lamport)?,
            floor_hash.as_slice(),
            now_ms,
        ],
    )?;
    Ok(CompactionReport { dropped_entries, swept_gapped })
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
