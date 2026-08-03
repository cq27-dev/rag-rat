//! Retention for the accepted table-sync log: compact a per-(stream, device) chain prefix below
//! a recorded floor (#1127).
//!
//! Anchors/1 launches fully retained; the first high-churn scope cannot. Compaction here drops
//! accepted entries with `lamport < floor` for ONE device chain, after refreshing every row whose
//! LWW winner is one of those entries — the hard constraint from #1020: without the refresh,
//! every such row's next producer pass takes the stale-version winner lookup, finds the entry
//! gone, reads `StaleRow::Unknown`, and the whole table re-authors at once.
//!
//! The floor is recorded durably (`table_sync_retained_floors`) and advertised on the wire: a
//! FRESH peer (no local chain) accepts the floor entry as its local root on exact
//! `(lamport, hash)` match and records the floor, so the signal propagates transitively. Adoption
//! is bounded by the chain-tip witness: a purged chain's retained tip is chain state, and a floor
//! at or below a different witness entry classifies as the equivocation it is. A re-offered entry
//! below the floor is idempotently ignored instead of classifying as a fork against the retained
//! tail. The driver (`table_sync_compact_overdue`) counts and floors only reclaimable entries,
//! clamps at the lowest pending lamport so forward-compat payloads stay offerable, treats a
//! non-advancing floor as the steady-state no-op, and stays inert for anchors/1 — its production
//! caller arrives with the first scope whose retention policy bounds it (overlay).
//!
//! A third state needs a follow-up slice of its own: a peer whose accepted TIP fell below the
//! sender's floor (offline while the scope churned past it) is neither fresh nor current. The
//! sender's entries cite a compacted predecessor the receiver never held, every entry parks, and
//! the chain stalls. Recovery — re-rooting such a chain with floor proof — is tracked in #1127
//! and deliberately not claimed here.
//!
//! Two accepted horizons, named rather than hidden:
//!
//! - **Spec bumps un-invisibilize refreshed rows.** The refresh restamps the published record but
//!   the row clock keeps pointing at the dropped winner, so the next table spec-version bump takes
//!   every refreshed row stale → the winner lookup finds the entry gone → `Unknown` → the producer
//!   re-authors it: one entry per refreshed row per spec bump. A clock can only point at a live
//!   entry, so this churn is the accepted cost of reclaiming the prefix — bounded, and far cheaper
//!   than the table-wide storm the refresh prevents on the uncompacted path. In the window between
//!   a spec bump and the next producer pass, the refold reads the same `Unknown` as a possible
//!   unsent edit and defers replay over those rows — the safe direction, and worth knowing when
//!   diagnosing a deferred entry after an upgrade.
//! - **Below-floor equivocations are undetectable on compacted peers.** The accept path answers
//!   `AlreadyPresent` for any entry below the floor, so a compacted peer discards fork evidence a
//!   non-compacted peer would still classify. Accepted: `Fork` is non-storing and a below-floor
//!   entry can never apply, so the divergence costs evidence, never convergence — and evidence loss
//!   is the price of reclaiming the region the evidence would describe.

use rusqlite::{OptionalExtension, Transaction, params};

use super::apply::{self, SyncedRow};
use super::registry::TableSpec;
use super::{row_op, store};
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;

/// What one prefix compaction did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactionReport {
    /// Rows whose published record was restamped to the current spec before their winner dropped.
    pub refreshed_rows: usize,
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
    entry_hash: [u8; 32],
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

/// Drop every accepted entry on `device`'s chain in `stream` with `lamport < floor_lamport`.
///
/// The floor entry itself is RETAINED and must exist on the chain — an arbitrary lamport cannot
/// invent a floor that no peer could ever be pointed at. Entries retained for replay
/// (`pending_reason IS NOT NULL`) are never compacted: their payloads are owed to a later binary,
/// and dropping them would silently lose the forward-compat contract.
pub(crate) fn compact_chain_prefix(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: DeviceFingerprint,
    floor_lamport: u64,
    registry: &[TableSpec],
    now_ms: i64,
) -> anyhow::Result<CompactionReport> {
    if let Some(current) = retained_floor(tx, stream, device)?
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

    let Some(context) = store::stream_context(tx, stream)? else {
        anyhow::bail!("table-sync compaction needs the stream's apply context");
    };
    let refreshed_rows =
        refresh_winners_below(tx, stream, &context.repo_id, registry, device, floor_lamport)?;
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
    Ok(CompactionReport { refreshed_rows, dropped_entries, swept_gapped })
}

/// Restamp the published record of every UNTOUCHED row whose whole-row LWW winner is one of the
/// entries about to drop. After the refresh such a row compares hashes at the CURRENT spec
/// version and never consults the winner lookup, so the dropped entry is invisible to it.
///
/// The `registry` MUST be the full production registry, not the scope's subset: a row whose table
/// is missing here keeps its stale published record while its winner drops anyway — the
/// conservative direction (a later pass re-authors it), but a silent one.
///
/// The refresh is deliberately verdict-gated on [`apply::StaleRow::Unchanged`]: a raw local write
/// does not advance the row clock, so restamping to the CURRENT row contents would record an
/// unsent edit as published — the producer would then see no delta and never author it (the
/// `StaleRow::Unknown` path it would otherwise take authors, the safe direction), and the replay
/// guard that keys off the published record would let a remote upsert clobber the edit. A
/// `LocallyChanged` or `Unknown` row is left alone: the producer authors it at a retained
/// lamport, exactly as it would without compaction. An unreadable row is skipped for the same
/// reason it always was — no comparable hash exists to restamp.
fn refresh_winners_below(
    tx: &Transaction<'_>,
    stream: StreamId,
    repo_id: &str,
    registry: &[TableSpec],
    device: DeviceFingerprint,
    floor_lamport: u64,
) -> anyhow::Result<usize> {
    let device_hex = device.to_string();
    let mut stmt = tx.prepare(
        "SELECT table_name, row_pk FROM sync_row_clocks
         WHERE stream_id = ?1 AND repo_id = ?2 AND device_fingerprint = ?3 AND lamport < ?4",
    )?;
    let rows = stmt
        .query_map(
            params![
                stream.to_bytes().as_slice(),
                repo_id,
                device_hex,
                i64::try_from(floor_lamport)?
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut refreshed = 0;
    for (table_name, row_pk) in rows {
        let Some(spec) = registry.iter().find(|spec| spec.name == table_name) else {
            continue;
        };
        let pk = row_op::row_pk_values(&row_pk)?;
        let SyncedRow::Cells(cells) = apply::read_synced_cells(tx, spec, &pk)? else {
            continue;
        };
        // The winner still exists at this point (the refresh runs before the drop), so the
        // disposition is settled against the entry itself — not against the published record,
        // which is exactly what may be lying about the row being sent.
        if apply::stale_row_disposition(tx, spec, repo_id, stream, &pk, &cells)?
            != apply::StaleRow::Unchanged
        {
            continue;
        }
        apply::record_published(
            tx,
            stream,
            repo_id,
            spec.name,
            &row_pk,
            &row_op::cells_hash(&cells),
            spec.spec_version,
        )?;
        refreshed += 1;
    }
    Ok(refreshed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table_sync::registry::{ColumnSpec, ValueType};
    use crate::table_sync::store::record_stream_context;
    use crate::table_sync::{Cell, RowOp, TypedValue};
    use crate::{AccountId, LocalDevice};

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

    fn conn() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch("CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT) STRICT;").unwrap();
        c
    }

    fn account() -> AccountId {
        AccountId::from_bytes([42; 32])
    }

    fn stream() -> StreamId {
        crate::stream::StreamId::from_bytes([7; 32])
    }

    fn enroll(conn: &rusqlite::Connection, device: &LocalDevice) {
        conn.execute(
            "INSERT OR IGNORE INTO account_roster_history
                 (roster_ref, account_id, device_fingerprint, role, effective_at, closed_at)
             VALUES (?1, ?2, ?3, 'owner', 0, NULL)",
            params![
                device.fingerprint().to_bytes().as_slice(),
                account().to_bytes().as_slice(),
                device.fingerprint().to_bytes().as_slice()
            ],
        )
        .unwrap();
    }

    fn upsert(id: &str, title: &str) -> RowOp {
        RowOp::Upsert {
            spec_version: 1,
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text(id.to_string())],
            cells: vec![Cell {
                column: "title".to_string(),
                value: TypedValue::Text(title.to_string()),
            }],
        }
    }

    /// Author `n` entries on the device chain (r{i} = v{i}), recording the stream context.
    fn author_chain(conn: &mut rusqlite::Connection, device: &LocalDevice, n: u64) {
        enroll(conn, device);
        let tx = conn.transaction().unwrap();
        record_stream_context(&tx, stream(), "repo", account(), [0x44; 32], "demo/1").unwrap();
        for i in 0..n {
            let op = upsert(&format!("r{i}"), &format!("v{i}"));
            let signed = store::author_row_entry(&tx, stream(), device.secret(), &op, 0).unwrap();
            let meta = crate::op::OpMeta {
                lamport: signed.entry.lamport,
                device: signed.entry.device_fingerprint,
            };
            apply::apply_row_op_on_stream(&tx, &SPEC, "repo", stream(), &op, meta).unwrap();
        }
        tx.commit().unwrap();
    }

    fn entry_lamports(conn: &rusqlite::Connection, device: &LocalDevice) -> Vec<i64> {
        let mut stmt = conn
            .prepare(
                "SELECT lamport FROM table_sync_entries
                 WHERE stream_id = ?1 AND device_fingerprint = ?2 ORDER BY lamport",
            )
            .unwrap();
        stmt.query_map(
            params![stream().to_bytes().as_slice(), device.fingerprint().to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    #[test]
    fn compaction_drops_the_prefix_and_keeps_the_rows_authorable() {
        let mut c = conn();
        let device = crate::local_device(&c, 0).unwrap();
        author_chain(&mut c, &device, 6);
        // A second write to r1: its winner is lamport 6 (retained), r0's winner is lamport 0
        // (about to drop below the floor).
        {
            let tx = c.transaction().unwrap();
            let op = upsert("r1", "v1b");
            let signed = store::author_row_entry(&tx, stream(), device.secret(), &op, 0).unwrap();
            let meta = crate::op::OpMeta {
                lamport: signed.entry.lamport,
                device: signed.entry.device_fingerprint,
            };
            apply::apply_row_op_on_stream(&tx, &SPEC, "repo", stream(), &op, meta).unwrap();
            tx.commit().unwrap();
        }
        // Age the published record so the producer would consult the winner lookup without the
        // refresh: a differing spec version is exactly the stale path compaction must close.
        c.execute("UPDATE sync_published_rows SET spec_version = 99", []).unwrap();

        let report = {
            let tx = c.transaction().unwrap();
            let report =
                compact_chain_prefix(&tx, stream(), device.fingerprint(), 4, REGISTRY, 0).unwrap();
            tx.commit().unwrap();
            report
        };
        assert_eq!(report.dropped_entries, 4, "entries 0..4 are reclaimed");
        assert_eq!(
            report.refreshed_rows, 3,
            "r0/r2/r3's winners dropped; r1 rewrote at 6 and r4/r5 are retained"
        );
        assert_eq!(entry_lamports(&c, &device), vec![4, 5, 6]);

        let floor = {
            let tx = c.transaction().unwrap();
            retained_floor(&tx, stream(), device.fingerprint()).unwrap()
        };
        assert_eq!(floor, Some(4));

        // The producer sees no delta: refreshed rows compare at the current spec version, so the
        // dropped winners never surface as a table-wide re-authoring storm.
        let tx = c.transaction().unwrap();
        let ops = super::super::produce::produce_row_ops(&tx, &SPEC, "repo", stream()).unwrap();
        assert!(ops.is_empty(), "refresh-before-drop leaves nothing to re-author");
        tx.commit().unwrap();

        // Authoring continues on the retained tail.
        author_chain(&mut c, &device, 0);
        let tx = c.transaction().unwrap();
        let op = upsert("r9", "v9");
        let signed = store::author_row_entry(&tx, stream(), device.secret(), &op, 0).unwrap();
        assert_eq!(signed.entry.lamport, 7, "the stream clock is based on the retained tail");
        tx.commit().unwrap();
    }

    /// A raw local edit on a row whose winner drops below the floor must NOT be restamped as
    /// published: that would disown the edit (the producer would see no delta) and disarm the
    /// replay guard that keys off the published record.
    #[test]
    fn an_unsent_local_edit_survives_compaction_and_is_authored() {
        let mut c = conn();
        let device = crate::local_device(&c, 0).unwrap();
        author_chain(&mut c, &device, 4);
        // Raw local write: no entry, no clock advance, published record still says v0.
        c.execute("UPDATE t_demo SET title = 'local-edit' WHERE id = 'r0'", []).unwrap();

        let report = {
            let tx = c.transaction().unwrap();
            let report =
                compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, REGISTRY, 0).unwrap();
            tx.commit().unwrap();
            report
        };
        assert_eq!(report.dropped_entries, 2);
        assert_eq!(
            report.refreshed_rows, 1,
            "only the untouched r1 is restamped; r0 is left for the producer"
        );

        // The edit is authored: the published record still names v0, so the producer sees the
        // delta it must carry — the safe direction the refresh must never close.
        let tx = c.transaction().unwrap();
        let ops = super::super::produce::produce_row_ops(&tx, &SPEC, "repo", stream()).unwrap();
        assert_eq!(ops.len(), 1, "the unsent edit is produced, not disowned");
        assert!(
            matches!(&ops[0], RowOp::Upsert { pk, .. } if pk == &vec![TypedValue::Text("r0".to_string())]),
            "and it is r0's local edit",
        );
        tx.commit().unwrap();
    }

    #[test]
    fn compaction_never_drops_a_pending_entry_or_invents_a_floor() {
        let mut c = conn();
        let device = crate::local_device(&c, 0).unwrap();
        author_chain(&mut c, &device, 3);
        c.execute(
            "UPDATE table_sync_entries SET pending_reason = 'unknown_column' WHERE lamport = 1",
            [],
        )
        .unwrap();

        {
            let tx = c.transaction().unwrap();
            let err = compact_chain_prefix(&tx, stream(), device.fingerprint(), 9, REGISTRY, 0)
                .unwrap_err();
            assert!(err.to_string().contains("names no entry"), "a floor must be a retained entry");
            tx.commit().unwrap();
        }

        let report = {
            let tx = c.transaction().unwrap();
            let report =
                compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, REGISTRY, 0).unwrap();
            tx.commit().unwrap();
            report
        };
        assert_eq!(report.dropped_entries, 1, "only the settled genesis drops");
        assert_eq!(entry_lamports(&c, &device), vec![1, 2], "the pending entry is retained");
    }

    #[test]
    fn a_second_compaction_advances_the_floor_and_a_retreat_is_refused() {
        let mut c = conn();
        let device = crate::local_device(&c, 0).unwrap();
        author_chain(&mut c, &device, 6);

        {
            let tx = c.transaction().unwrap();
            compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, REGISTRY, 0).unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = c.transaction().unwrap();
            let report =
                compact_chain_prefix(&tx, stream(), device.fingerprint(), 4, REGISTRY, 0).unwrap();
            tx.commit().unwrap();
            assert_eq!(report.dropped_entries, 2, "entries 2 and 3 drop in the second pass");
        }
        let floor = {
            let tx = c.transaction().unwrap();
            retained_floor(&tx, stream(), device.fingerprint()).unwrap()
        };
        assert_eq!(floor, Some(4), "the floor advances monotonically");
        assert_eq!(entry_lamports(&c, &device), vec![4, 5]);

        {
            let tx = c.transaction().unwrap();
            let err = compact_chain_prefix(&tx, stream(), device.fingerprint(), 3, REGISTRY, 0)
                .unwrap_err();
            assert!(
                err.to_string().contains("does not advance"),
                "a retreating floor is a caller bug, not a silent re-compaction: {err}",
            );
            tx.commit().unwrap();
        }
        assert_eq!(entry_lamports(&c, &device), vec![4, 5], "the refused retreat drops nothing");
    }

    /// The below-floor idempotence is strict: an equivocation AT the floor lamport is beyond the
    /// reclaimed region and still classifies as a fork.
    #[test]
    fn an_equivocation_at_the_floor_lamport_is_still_a_fork() {
        let mut c = conn();
        let device = crate::local_device(&c, 0).unwrap();
        author_chain(&mut c, &device, 4);
        let tail_hash: Vec<u8> = c
            .query_row("SELECT entry_hash FROM table_sync_entries WHERE lamport = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        {
            let tx = c.transaction().unwrap();
            compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, REGISTRY, 0).unwrap();
            tx.commit().unwrap();
        }

        // A DIFFERENT entry at lamport 2 — the floor's own slot, but not the floor entry.
        let forged = crate::entry::sign_entry_from_op_bytes(
            device.secret(),
            stream(),
            Some(<[u8; 32]>::try_from(tail_hash.as_slice()).unwrap()),
            2,
            super::super::row_op::encode(&upsert("rx", "forged")),
        );
        let tx = c.transaction().unwrap();
        let outcome = store::accept_row_entry(
            &tx,
            account(),
            stream(),
            &["t_demo"],
            &forged.signed_bytes,
            &device.secret().public(),
            0,
            None,
        )
        .unwrap();
        assert_eq!(
            outcome,
            store::AcceptOutcome::Fork,
            "lamport == floor is outside the reclaimed prefix, so equivocation still reports",
        );
        tx.commit().unwrap();
    }

    /// The re-adoption candidate set derives from the merge state, which survives compaction:
    /// compact a writer's prefix, THEN remove the writer, and the drain still repairs the row —
    /// with the audit naming the slot by lamport alone (the winning entry is gone).
    #[test]
    fn compaction_before_a_removal_does_not_shrink_the_repair_set() {
        let mut c = conn();
        let device = crate::local_device(&c, 0).unwrap();
        author_chain(&mut c, &device, 4);
        {
            let tx = c.transaction().unwrap();
            compact_chain_prefix(&tx, stream(), device.fingerprint(), 3, REGISTRY, 0).unwrap();
            tx.commit().unwrap();
        }
        let removed_fp = device.fingerprint();

        // The writer is removed after its prefix was compacted: entries 0..3 named the winners of
        // r0..r2, and those winners are GONE from the entry log now.
        let work = {
            let tx = c.transaction().unwrap();
            store::enqueue_readoption_work(&tx, account(), removed_fp, stream(), [9; 32], 9, 10)
                .unwrap();
            let work =
                store::readoption_work_for_stream(&tx, account(), stream()).unwrap().unwrap();
            tx.commit().unwrap();
            work
        };
        let candidates = {
            let tx = c.transaction().unwrap();
            let out = store::readoption_candidates(&tx, stream(), work.device_fingerprint).unwrap();
            tx.commit().unwrap();
            out
        };
        assert_eq!(candidates.len(), 4, "merge state names every surviving winner");
        assert!(
            candidates.iter().filter(|c| c.original_lamport < 3).all(|c| c.entry_hash.is_none()),
            "compacted winners carry no hash",
        );
        assert!(
            candidates.iter().find(|c| c.original_lamport == 3).unwrap().entry_hash.is_some(),
            "the retained winner keeps its hash",
        );
    }

    /// A gapped entry below the floor can never promote — its predecessor is part of the
    /// reclaimed prefix — so compaction sweeps it instead of letting it burn the per-chain cap.
    #[test]
    fn compaction_sweeps_gapped_entries_below_the_floor() {
        let mut c = conn();
        let device = crate::local_device(&c, 0).unwrap();
        author_chain(&mut c, &device, 4);
        c.execute(
            "INSERT INTO table_sync_gapped_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                 gapped_at_ms
             ) VALUES (x'99', ?1, ?2, 1, x'98', x'00', 0),
                      (x'9a', ?1, ?2, 9, x'97', x'00', 0)",
            params![stream().to_bytes().as_slice(), device.fingerprint().to_bytes().as_slice()],
        )
        .unwrap();

        let report = {
            let tx = c.transaction().unwrap();
            let report =
                compact_chain_prefix(&tx, stream(), device.fingerprint(), 3, REGISTRY, 0).unwrap();
            tx.commit().unwrap();
            report
        };
        assert_eq!(report.swept_gapped, 1, "the gapped entry below the floor is swept");
        let remaining: i64 = c
            .query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "the one above the floor survives");
    }

    /// Adopting an advertised floor sweeps gapped entries below it: a below-floor entry parked by
    /// reordering can never promote once the floor is adopted (its predecessor reports
    /// AlreadyPresent, so nothing ever probes it), and must not burn the per-chain cap forever.
    #[test]
    fn adopting_a_floor_sweeps_gapped_entries_below_it() {
        let mut a = conn();
        let device = crate::local_device(&a, 0).unwrap();
        author_chain(&mut a, &device, 4);
        let (floor_hash, floor_bytes): (Vec<u8>, Vec<u8>) = a
            .query_row(
                "SELECT entry_hash, signed_bytes FROM table_sync_entries WHERE lamport = 2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        // The fresh peer parked a below-floor entry before the floor entry arrived (reordered
        // delivery), then the floor arrives and roots the chain.
        let mut b = conn();
        enroll(&b, &device);
        b.execute(
            "INSERT INTO table_sync_gapped_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                 gapped_at_ms
             ) VALUES (x'99', ?1, ?2, 1, x'98', x'00', 0)",
            params![stream().to_bytes().as_slice(), device.fingerprint().to_bytes().as_slice()],
        )
        .unwrap();
        let tx = b.transaction().unwrap();
        let outcome = store::accept_row_entry(
            &tx,
            account(),
            stream(),
            &["t_demo"],
            &floor_bytes,
            &device.secret().public(),
            0,
            Some(store::AdvertisedFloor {
                lamport: 2,
                entry_hash: <[u8; 32]>::try_from(floor_hash.as_slice()).unwrap(),
            }),
        )
        .unwrap();
        assert!(matches!(outcome, store::AcceptOutcome::Stored { .. }));
        assert_eq!(
            retained_floor(&tx, stream(), device.fingerprint()).unwrap(),
            Some(2),
            "the adopted floor is recorded",
        );
        let gapped: i64 = tx
            .query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(gapped, 0, "the below-floor parked entry is swept on adoption");
        tx.commit().unwrap();
    }

    /// A retained chain-tip witness is chain state: adopting a floor BELOW it would regress the
    /// purge boundary, resurrect the purged prefix, and wedge the chain behind it.
    #[test]
    fn floor_adoption_never_regresses_a_witnessed_tip() {
        let mut a = conn();
        let device = crate::local_device(&a, 0).unwrap();
        author_chain(&mut a, &device, 4);
        let (floor_hash, floor_bytes): (Vec<u8>, Vec<u8>) = a
            .query_row(
                "SELECT entry_hash, signed_bytes FROM table_sync_entries WHERE lamport = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        // The receiver purged its accepted log; the chain-tip witness at lamport 3 survives.
        let mut b = conn();
        enroll(&b, &device);
        let tip_hash: Vec<u8> = a
            .query_row("SELECT entry_hash FROM table_sync_entries WHERE lamport = 3", [], |row| {
                row.get(0)
            })
            .unwrap();
        b.execute(
            "INSERT INTO table_sync_chain_tips(stream_id, device_fingerprint, lamport, entry_hash)
             VALUES (?1, ?2, 3, ?3)",
            params![
                stream().to_bytes().as_slice(),
                device.fingerprint().to_bytes().as_slice(),
                tip_hash.as_slice()
            ],
        )
        .unwrap();

        let tx = b.transaction().unwrap();
        let outcome = store::accept_row_entry(
            &tx,
            account(),
            stream(),
            &["t_demo"],
            &floor_bytes,
            &device.secret().public(),
            0,
            Some(store::AdvertisedFloor {
                lamport: 1,
                entry_hash: <[u8; 32]>::try_from(floor_hash.as_slice()).unwrap(),
            }),
        )
        .unwrap();
        assert_eq!(
            outcome,
            store::AcceptOutcome::Fork,
            "a floor below the witnessed tip classifies as the equivocation it is, not a root",
        );
        assert!(
            retained_floor(&tx, stream(), device.fingerprint()).unwrap().is_none(),
            "and no floor is adopted",
        );
        tx.commit().unwrap();

        // The same-lamport boundary: an equivocation AT the witnessed lamport (different hash)
        // is equally not adoptable — the witness branch reports it as the fork it is.
        let equivocation = crate::entry::sign_entry_from_op_bytes(
            device.secret(),
            stream(),
            None,
            3,
            super::super::row_op::encode(&upsert("rx", "fork")),
        );
        let tx = b.transaction().unwrap();
        let outcome = store::accept_row_entry(
            &tx,
            account(),
            stream(),
            &["t_demo"],
            &equivocation.signed_bytes,
            &device.secret().public(),
            0,
            Some(store::AdvertisedFloor { lamport: 3, entry_hash: equivocation.entry.entry_hash }),
        )
        .unwrap();
        assert_eq!(
            outcome,
            store::AcceptOutcome::Fork,
            "lamport == witness with a different hash is the equivocation, not a root",
        );
        assert!(retained_floor(&tx, stream(), device.fingerprint()).unwrap().is_none());
        tx.commit().unwrap();
    }

    #[test]
    fn a_reoffered_entry_below_the_floor_is_idempotent_not_a_fork() {
        let mut c = conn();
        let device = crate::local_device(&c, 0).unwrap();
        author_chain(&mut c, &device, 3);
        let dropped: Vec<u8> = c
            .query_row("SELECT signed_bytes FROM table_sync_entries WHERE lamport = 0", [], |row| {
                row.get(0)
            })
            .unwrap();
        {
            let tx = c.transaction().unwrap();
            compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, REGISTRY, 0).unwrap();
            tx.commit().unwrap();
        }

        // Redelivery of a compacted entry must not classify as a fork against the retained tail:
        // the floor says the prefix is intentionally gone.
        let tx = c.transaction().unwrap();
        let outcome = store::accept_row_entry(
            &tx,
            account(),
            stream(),
            &["t_demo"],
            &dropped,
            &device.secret().public(),
            0,
            None,
        )
        .unwrap();
        assert_eq!(outcome, store::AcceptOutcome::AlreadyPresent);
        tx.commit().unwrap();
    }
}
