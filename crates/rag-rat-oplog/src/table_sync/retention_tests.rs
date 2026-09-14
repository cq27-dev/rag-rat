use super::*;
use crate::table_sync::registry::{ColumnSpec, TableSpec, ValueType};
use crate::table_sync::scope_stream::ScopeId;
use crate::table_sync::store::{self, record_stream_context};
use crate::table_sync::{Cell, RowOp, TypedValue, apply, row_op};
use crate::{AccountId, LocalDevice};

const SPEC: TableSpec = TableSpec {
    name: "t_demo",
    scope_id: ScopeId::new("demo/1"),
    spec_version: 1,
    pk: &[ColumnSpec::required("id", ValueType::Text)],
    columns: &[ColumnSpec::required("title", ValueType::Text)],
    local_columns: &[],
    repo_column: None,
};

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

fn remove(id: &str) -> RowOp {
    RowOp::Remove {
        spec_version: 1,
        table: "t_demo".to_string(),
        pk: vec![TypedValue::Text(id.to_string())],
    }
}

/// Author and self-apply `ops` on the device chain in order, recording the stream context.
fn author(conn: &mut rusqlite::Connection, device: &LocalDevice, ops: &[RowOp]) {
    enroll(conn, device);
    let tx = conn.transaction().unwrap();
    record_stream_context(&tx, stream(), "repo", account(), [0x44; 32], "demo/1").unwrap();
    for op in ops {
        let signed = store::author_row_entry(&tx, stream(), device.secret(), op, 0).unwrap();
        let meta = crate::op::OpMeta {
            lamport: signed.entry.lamport,
            device: signed.entry.device_fingerprint,
        };
        apply::apply_row_op_on_stream(&tx, &SPEC, "repo", stream(), op, meta).unwrap();
    }
    tx.commit().unwrap();
}

/// Author `n` entries on distinct rows (r{i} = v{i}): every entry carries a live row.
fn author_chain(conn: &mut rusqlite::Connection, device: &LocalDevice, n: u64) {
    let ops: Vec<_> = (0..n).map(|i| upsert(&format!("r{i}"), &format!("v{i}"))).collect();
    author(conn, device, &ops);
}

/// Author `n` successive writes to ONE row: every entry but the last is superseded.
fn author_rewrites(conn: &mut rusqlite::Connection, device: &LocalDevice, n: u64) {
    let ops: Vec<_> = (0..n).map(|i| upsert("hot", &format!("v{i}"))).collect();
    author(conn, device, &ops);
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
fn compaction_drops_only_superseded_entries() {
    let mut c = conn();
    let device = crate::local_device(&c, 0).unwrap();
    // 0: r0 (superseded by 3), 1: r1 (superseded by 2), 2: r1, 3: r0, 4: r2.
    author(&mut c, &device, &[
        upsert("r0", "v0"),
        upsert("r1", "v1"),
        upsert("r1", "v1b"),
        upsert("r0", "v0b"),
        upsert("r2", "v2"),
    ]);

    let report = {
        let tx = c.transaction().unwrap();
        let report = compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, 0).unwrap();
        tx.commit().unwrap();
        report
    };
    assert_eq!(report.dropped_entries, 2, "the two superseded entries are reclaimed");
    {
        let tx = c.transaction().unwrap();
        let err = compact_chain_prefix(&tx, stream(), device.fingerprint(), 3, 0).unwrap_err();
        assert!(err.to_string().contains("would drop"), "r1's winner at 2 pins: {err}");
    }
    assert_eq!(entry_lamports(&c, &device), vec![2, 3, 4]);
    let floor = {
        let tx = c.transaction().unwrap();
        retained_floor(&tx, stream(), device.fingerprint()).unwrap()
    };
    assert_eq!(floor, Some(2));

    // Every live row still has its winning entry, so a spec bump after compaction resolves each
    // stale row against it: the producer re-authors nothing.
    c.execute("UPDATE sync_published_rows SET spec_version = 99", []).unwrap();
    let tx = c.transaction().unwrap();
    let ops = super::super::produce::produce_row_ops(&tx, &SPEC, "repo", stream()).unwrap();
    assert!(ops.is_empty(), "no compacted winner reads as Unknown: {ops:?}");
    let signed =
        store::author_row_entry(&tx, stream(), device.secret(), &upsert("r9", "v9"), 0).unwrap();
    assert_eq!(signed.entry.lamport, 5, "the stream clock is based on the retained tail");
    tx.commit().unwrap();
}

/// A live winner is a pin: a floor above it is refused, so a raw local edit on that row keeps
/// its published record and the producer still authors it.
#[test]
fn a_live_winner_pins_its_entry_and_an_unsent_edit_is_still_authored() {
    let mut c = conn();
    let device = crate::local_device(&c, 0).unwrap();
    author_chain(&mut c, &device, 4);
    c.execute("UPDATE t_demo SET title = 'local-edit' WHERE id = 'r0'", []).unwrap();

    {
        let tx = c.transaction().unwrap();
        let err = compact_chain_prefix(&tx, stream(), device.fingerprint(), 1, 0).unwrap_err();
        assert!(err.to_string().contains("would drop"), "r0's winner pins lamport 0: {err}");
    }
    assert_eq!(entry_lamports(&c, &device), vec![0, 1, 2, 3], "the refusal drops nothing");

    let tx = c.transaction().unwrap();
    let ops = super::super::produce::produce_row_ops(&tx, &SPEC, "repo", stream()).unwrap();
    assert_eq!(ops.len(), 1, "the unsent edit is produced, not disowned");
    assert!(
        matches!(&ops[0], RowOp::Upsert { pk, .. } if pk == &vec![TypedValue::Text("r0".to_string())]),
        "and it is r0's local edit",
    );
    tx.commit().unwrap();
}

/// A tombstone whose pk has no live row is a pin — a peer that re-roots past it would keep the
/// deleted row — until a later write makes the row live again.
#[test]
fn an_orphan_tombstone_pins_until_its_row_is_live_again() {
    let mut c = conn();
    let device = crate::local_device(&c, 0).unwrap();
    // 0: r0, 1: remove r0 (orphan tombstone), 2: hot (superseded by 3), 3: hot.
    author(&mut c, &device, &[
        upsert("r0", "v0"),
        remove("r0"),
        upsert("hot", "v0"),
        upsert("hot", "v1"),
    ]);
    {
        let tx = c.transaction().unwrap();
        let err = compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, 0).unwrap_err();
        assert!(err.to_string().contains("would drop"), "the tombstone at 1 pins: {err}");
    }

    // r0 is live again at 4: anything the tombstone would suppress loses to that clock.
    author(&mut c, &device, &[upsert("r0", "v0b")]);
    let tx = c.transaction().unwrap();
    let report = compact_chain_prefix(&tx, stream(), device.fingerprint(), 3, 0).unwrap();
    tx.commit().unwrap();
    assert_eq!(report.dropped_entries, 3);
    assert_eq!(entry_lamports(&c, &device), vec![3, 4]);
}

#[test]
fn compaction_never_drops_a_pending_entry_or_invents_a_floor() {
    let mut c = conn();
    let device = crate::local_device(&c, 0).unwrap();
    author_rewrites(&mut c, &device, 3);
    c.execute(
        "UPDATE table_sync_entries SET pending_reason = 'unknown_column' WHERE lamport = 1",
        [],
    )
    .unwrap();

    {
        let tx = c.transaction().unwrap();
        let err = compact_chain_prefix(&tx, stream(), device.fingerprint(), 9, 0).unwrap_err();
        assert!(err.to_string().contains("names no entry"), "a floor must be a retained entry");
        tx.commit().unwrap();
    }

    let report = {
        let tx = c.transaction().unwrap();
        let report = compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, 0).unwrap();
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
    author_rewrites(&mut c, &device, 6);

    {
        let tx = c.transaction().unwrap();
        compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, 0).unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = c.transaction().unwrap();
        let report = compact_chain_prefix(&tx, stream(), device.fingerprint(), 4, 0).unwrap();
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
        let err = compact_chain_prefix(&tx, stream(), device.fingerprint(), 3, 0).unwrap_err();
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
    author_rewrites(&mut c, &device, 4);
    let tail_hash: Vec<u8> = c
        .query_row("SELECT entry_hash FROM table_sync_entries WHERE lamport = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    {
        let tx = c.transaction().unwrap();
        compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, 0).unwrap();
        tx.commit().unwrap();
    }

    // A DIFFERENT entry at lamport 2 — the floor's own slot, but not the floor entry.
    let forged = crate::entry::sign_entry_from_op_bytes(
        device.secret(),
        stream(),
        Some(EntryHash::from_bytes(<[u8; 32]>::try_from(tail_hash.as_slice()).unwrap())),
        2,
        super::super::row_op::encode(&upsert("rx", "forged")),
    );
    let tx = c.transaction().unwrap();
    let outcome = store::accept_row_entry(
        &tx,
        &store::AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t_demo"],
            pubkey: &device.secret().public(),
            now_ms: 0,
        },
        &forged.signed_bytes,
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

/// A store compacted before the pin rule dropped entries that still carried live rows. The
/// re-adoption candidate set derives from the merge state, which survived: remove the writer
/// and the drain still repairs every row — the audit naming the slot by lamport alone.
#[test]
fn a_winner_whose_entry_is_gone_is_still_a_repair_candidate() {
    let mut c = conn();
    let device = crate::local_device(&c, 0).unwrap();
    author_chain(&mut c, &device, 4);
    c.execute("DELETE FROM table_sync_entries WHERE lamport < 3", []).unwrap();
    let removed_fp = device.fingerprint();

    // Entries 0..3 named the winners of r0..r2, and those winners are GONE from the entry log.
    let work = {
        let tx = c.transaction().unwrap();
        store::enqueue_readoption_work(&tx, account(), removed_fp, stream(), [9; 32], 9, 10)
            .unwrap();
        let work = store::readoption_work_for_stream(&tx, account(), stream()).unwrap().unwrap();
        tx.commit().unwrap();
        work
    };
    let candidates = {
        let tx = c.transaction().unwrap();
        let out =
            store::readoption_candidates(&tx, stream(), work.device_fingerprint, "local").unwrap();
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
    author_rewrites(&mut c, &device, 4);
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
        let report = compact_chain_prefix(&tx, stream(), device.fingerprint(), 3, 0).unwrap();
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
        &store::AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t_demo"],
            pubkey: &device.secret().public(),
            now_ms: 0,
        },
        &floor_bytes,
        Some(store::ChainCursor {
            lamport: 2,
            entry_hash: EntryHash::from_bytes(<[u8; 32]>::try_from(floor_hash.as_slice()).unwrap()),
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
        &store::AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t_demo"],
            pubkey: &device.secret().public(),
            now_ms: 0,
        },
        &floor_bytes,
        Some(store::ChainCursor {
            lamport: 1,
            entry_hash: EntryHash::from_bytes(<[u8; 32]>::try_from(floor_hash.as_slice()).unwrap()),
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
        &store::AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t_demo"],
            pubkey: &device.secret().public(),
            now_ms: 0,
        },
        &equivocation.signed_bytes,
        Some(store::ChainCursor { lamport: 3, entry_hash: equivocation.entry.entry_hash }),
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

/// The rejoin case the floor exists for: a peer whose accepted tip fell below the sender's
/// floor re-roots onto it instead of parking forever on a compacted predecessor.
#[test]
fn a_peer_whose_tip_fell_below_the_floor_re_roots_and_converges() {
    let mut a = conn();
    let device = crate::local_device(&a, 0).unwrap();
    author_chain(&mut a, &device, 6);
    let bytes = |lamport: i64| -> (Vec<u8>, Vec<u8>) {
        a.query_row(
            "SELECT entry_hash, signed_bytes FROM table_sync_entries WHERE lamport = ?1",
            [lamport],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    let floor_hash = bytes(4).0;

    // The peer holds entries 0..2 (its tip is 2), then rejoins after the sender compacted
    // below floor 4. Entry 3 is NOT delivered — its hash is the predecessor entry 4 cites.
    let mut b = conn();
    enroll(&b, &device);
    for lamport in 0..3 {
        let (_, signed_bytes) = bytes(lamport);
        let tx = b.transaction().unwrap();
        store::accept_row_entry(
            &tx,
            &store::AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t_demo"],
                pubkey: &device.secret().public(),
                now_ms: 0,
            },
            &signed_bytes,
            None,
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // The floor entry arrives with its advertisement: adopted as the new root, not parked on
    // the compacted predecessor it cites.
    let tx = b.transaction().unwrap();
    let outcome = store::accept_row_entry(
        &tx,
        &store::AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t_demo"],
            pubkey: &device.secret().public(),
            now_ms: 0,
        },
        &bytes(4).1,
        Some(store::ChainCursor {
            lamport: 4,
            entry_hash: EntryHash::from_bytes(<[u8; 32]>::try_from(floor_hash.as_slice()).unwrap()),
        }),
    )
    .unwrap();
    assert!(
        matches!(outcome, store::AcceptOutcome::Stored { .. }),
        "the floor re-roots the stalled chain: {outcome:?}",
    );
    assert_eq!(retained_floor(&tx, stream(), device.fingerprint()).unwrap(), Some(4));

    // The chain continues from the new root, and the skipped prefix is idempotent, not fork.
    let outcome = store::accept_row_entry(
        &tx,
        &store::AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t_demo"],
            pubkey: &device.secret().public(),
            now_ms: 0,
        },
        &bytes(5).1,
        None,
    )
    .unwrap();
    assert!(matches!(outcome, store::AcceptOutcome::Stored { .. }), "entry 5 chains: {outcome:?}");
    let outcome = store::accept_row_entry(
        &tx,
        &store::AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t_demo"],
            pubkey: &device.secret().public(),
            now_ms: 0,
        },
        &bytes(3).1,
        None,
    )
    .unwrap();
    assert_eq!(outcome, store::AcceptOutcome::AlreadyPresent, "below the floor is idempotent");
    tx.commit().unwrap();
}

/// A parked gapped copy of the floor entry can never promote (its predecessor was compacted),
/// so AlreadyGapped must not deadlock the re-root: the copy is taken out and the entry
/// adopted as root.
#[test]
fn a_parked_floor_entry_does_not_deadlock_the_reroot() {
    let mut a = conn();
    let device = crate::local_device(&a, 0).unwrap();
    author_chain(&mut a, &device, 6);
    let (floor_hash, floor_bytes): (Vec<u8>, Vec<u8>) = a
        .query_row(
            "SELECT entry_hash, signed_bytes FROM table_sync_entries WHERE lamport = 4",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    // The peer holds entries 0..2, and the floor entry ITSELF is parked in its gapped table
    // — the rolling-upgrade shape: a new sender offered At(floor) to a pre-PR receiver,
    // which parked it on the compacted predecessor.
    let mut b = conn();
    enroll(&b, &device);
    for lamport in 0..3 {
        let (_, signed_bytes): (Vec<u8>, Vec<u8>) = a
            .query_row(
                "SELECT entry_hash, signed_bytes FROM table_sync_entries WHERE lamport = ?1",
                [lamport],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let tx = b.transaction().unwrap();
        store::accept_row_entry(
            &tx,
            &store::AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t_demo"],
                pubkey: &device.secret().public(),
                now_ms: 0,
            },
            &signed_bytes,
            None,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    b.execute(
        "INSERT INTO table_sync_gapped_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                 gapped_at_ms
             ) VALUES (?1, ?2, ?3, 4, x'00', ?4, 0)",
        params![
            floor_hash.as_slice(),
            stream().to_bytes().as_slice(),
            device.fingerprint().to_bytes().as_slice(),
            floor_bytes.as_slice()
        ],
    )
    .unwrap();

    let tx = b.transaction().unwrap();
    let outcome = store::accept_row_entry(
        &tx,
        &store::AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t_demo"],
            pubkey: &device.secret().public(),
            now_ms: 0,
        },
        &floor_bytes,
        Some(store::ChainCursor {
            lamport: 4,
            entry_hash: EntryHash::from_bytes(<[u8; 32]>::try_from(floor_hash.as_slice()).unwrap()),
        }),
    )
    .unwrap();
    assert!(
        matches!(outcome, store::AcceptOutcome::Stored { .. }),
        "the parked floor entry is adopted, not short-circuited: {outcome:?}",
    );
    assert_eq!(retained_floor(&tx, stream(), device.fingerprint()).unwrap(), Some(4));
    let gapped: i64 = tx
        .query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |row| row.get(0))
        .unwrap();
    assert_eq!(gapped, 0, "the parked copy is gone");
    tx.commit().unwrap();
}

#[test]
fn a_reoffered_entry_below_the_floor_is_idempotent_not_a_fork() {
    let mut c = conn();
    let device = crate::local_device(&c, 0).unwrap();
    author_rewrites(&mut c, &device, 3);
    let dropped: Vec<u8> = c
        .query_row("SELECT signed_bytes FROM table_sync_entries WHERE lamport = 0", [], |row| {
            row.get(0)
        })
        .unwrap();
    {
        let tx = c.transaction().unwrap();
        compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, 0).unwrap();
        tx.commit().unwrap();
    }

    // Redelivery of a compacted entry must not classify as a fork against the retained tail:
    // the floor says the prefix is intentionally gone.
    let tx = c.transaction().unwrap();
    let outcome = store::accept_row_entry(
        &tx,
        &store::AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t_demo"],
            pubkey: &device.secret().public(),
            now_ms: 0,
        },
        &dropped,
        None,
    )
    .unwrap();
    assert_eq!(outcome, store::AcceptOutcome::AlreadyPresent);
    tx.commit().unwrap();
}

// ── statements as pins (#1295) ───────────────────────────────────────────────────────────────

fn restate_of(id: &str, device: &LocalDevice, lamport: u64) -> RowOp {
    RowOp::Restate {
        spec_version: 1,
        table: "t_demo".to_string(),
        deletes: vec![crate::table_sync::StatedDelete {
            pk: vec![TypedValue::Text(id.to_string())],
            device: device.fingerprint(),
            lamport,
        }],
    }
}

fn pins(conn: &mut rusqlite::Connection, device: &LocalDevice) -> Vec<Pin> {
    let tx = conn.transaction().unwrap();
    chain_pins(&tx, stream(), device.fingerprint(), 0, 1 << 40, usize::MAX).unwrap()
}

/// An orphan tombstone pins the entry at this chain's STATEMENT of it: the original `Remove`
/// until the writer restates it at its tail, then the restatement — and the entry that first
/// stated it can go. A row live again stops the statement pinning, and the live write pins.
#[test]
fn a_restated_tombstone_frees_the_original_entry() {
    let mut c = conn();
    let device = crate::local_device(&c, 0).unwrap();
    author(&mut c, &device, &[upsert("r1", "v1"), remove("r1")]); // 0, 1
    assert_eq!(pins(&mut c, &device), vec![Pin {
        lamport: 1,
        kind: PinKind::Statements(vec![StatedRow {
            table_name: "t_demo".to_string(),
            row_pk: row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]),
            device_hex: device.fingerprint().to_string(),
            lamport: 1,
        }]),
    }]);
    author(&mut c, &device, &[upsert("r2", "x"), restate_of("r1", &device, 1)]); // 2, 3
    let after = pins(&mut c, &device);
    assert_eq!(after.iter().map(|pin| pin.lamport).collect::<Vec<_>>(), vec![2, 3]);
    assert!(matches!(after[1].kind, PinKind::Statements(ref rows) if rows.len() == 1));
    // The entry at 1 is superseded now: compaction past it is allowed.
    let tx = c.transaction().unwrap();
    compact_chain_prefix(&tx, stream(), device.fingerprint(), 2, 0).unwrap();
    tx.commit().unwrap();
    // Live again: the statement stops pinning and the live write pins instead.
    author(&mut c, &device, &[upsert("r1", "back")]); // 4
    assert_eq!(pins(&mut c, &device).iter().map(|pin| pin.lamport).collect::<Vec<_>>(), vec![2, 4]);
}

/// One restatement of several deletes is ONE pin holding them all, and a chain's statements are
/// its own: a second chain stating the same tombstone pins on that chain, not this one.
#[test]
fn shared_statements_count_as_one_entry_per_chain() {
    let mut c = conn();
    let device = crate::local_device(&c, 0).unwrap();
    author(&mut c, &device, &[upsert("r1", "a"), upsert("r2", "b"), remove("r1"), remove("r2")]); // 0..3
    let both = RowOp::Restate {
        spec_version: 1,
        table: "t_demo".to_string(),
        deletes: ["r1", "r2"]
            .into_iter()
            .zip([2u64, 3])
            .map(|(id, lamport)| crate::table_sync::StatedDelete {
                pk: vec![TypedValue::Text(id.to_string())],
                device: device.fingerprint(),
                lamport,
            })
            .collect(),
    };
    author(&mut c, &device, &[both]); // 4
    let after = pins(&mut c, &device);
    assert_eq!(after.len(), 1, "one entry states both");
    assert_eq!(after[0].lamport, 4);
    assert_eq!(after[0].held(), 2);
    // Another chain's statement of r1 (identity unchanged) is that chain's pin, not this one's.
    let other = crate::op::DeviceFingerprint::from_bytes([0xff; 32]);
    let tx = c.transaction().unwrap();
    tx.execute(
        "INSERT INTO sync_tombstone_statements(
             stream_id, repo_id, table_name, row_pk, device_fingerprint, lamport)
         VALUES (?1, 'repo', 't_demo', ?2, ?3, 40)",
        params![
            stream().to_bytes().as_slice(),
            row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]),
            other.to_string(),
        ],
    )
    .unwrap();
    let theirs = chain_pins(&tx, stream(), other, 0, 1 << 40, usize::MAX).unwrap();
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].lamport, 40);
    let ours = chain_pins(&tx, stream(), device.fingerprint(), 0, 1 << 40, usize::MAX).unwrap();
    assert_eq!(ours[0].held(), 2, "unchanged by the other chain's statement");
}

/// The migration's backfill and a live `Remove` agree on what pins: one statement per tombstone
/// at its own identity.
#[test]
fn statement_backfill_keeps_chain_pins_parity() {
    let mut c = conn();
    let device = crate::local_device(&c, 0).unwrap();
    author(&mut c, &device, &[upsert("r1", "v1"), remove("r1"), upsert("r2", "v2"), remove("r2")]);
    let live = pins(&mut c, &device);
    c.execute("DELETE FROM sync_tombstone_statements", []).unwrap();
    rag_rat_db::schema::migrations::apply_tombstone_statements(&c).unwrap();
    assert_eq!(pins(&mut c, &device), live);
}
