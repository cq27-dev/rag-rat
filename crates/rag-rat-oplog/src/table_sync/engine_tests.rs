use rusqlite::OptionalExtension;

use super::*;
use crate::table_sync::registry::{ColumnSpec, ValueType};

const SPEC: TableSpec = TableSpec {
    name: "t_demo",
    scope_id: ScopeId::new("demo/1"),
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
        conn.execute_batch("CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT) STRICT;").unwrap();
        let local = crate::local_device(&conn, 0).unwrap();
        // The device models a writer: what it edits locally is unsent work until it authors.
        enroll_writer(&conn, AccountId::from_bytes([42; 32]), local.fingerprint());
        Self { conn, local }
    }

    /// This device's one enrolment becomes read-only: it has never been a writer, so nothing
    /// it holds is unsent.
    fn make_read_only(&self) {
        self.conn
            .execute(
                "UPDATE account_roster_history SET role = 'read_only' WHERE device_fingerprint = \
                 ?1",
                [self.local.fingerprint().to_bytes().as_slice()],
            )
            .unwrap();
    }

    fn pubkey(&self) -> DevicePublic {
        self.local.secret().public()
    }

    fn set_title(&self, title: &str) {
        self.conn.execute("UPDATE t_demo SET title = ?1 WHERE id = 'r1'", [title]).unwrap();
    }

    fn title(&self) -> Option<String> {
        self.conn
            .query_row("SELECT title FROM t_demo WHERE id = 'r1'", [], |r| r.get::<_, String>(0))
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
            local_writer: Default::default(),
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
    fn ingest_reports(&mut self, entries: &[Vec<u8>], from: &DevicePublic) -> Vec<IngestReport> {
        enroll_writer(&self.conn, AccountId::from_bytes([42; 32]), from.fingerprint());
        let tx = self.conn.transaction().unwrap();
        let ctx = SyncCtx {
            repo_id: "repo",
            account_id: AccountId::from_bytes([42; 32]),
            incarnation_ref: [0x44; 32],
            device: &self.local,
            registry: REGISTRY,
            now_ms: 0,
            local_writer: Default::default(),
        };
        let out = entries
            .iter()
            .map(|bytes| ingest(&tx, &ctx, ScopeId::new("demo/1"), bytes, from, None).unwrap());
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
            local_writer: Default::default(),
        };
        let out = entries.iter().map(|bytes| {
            ingest(&tx, &ctx, ScopeId::new("demo/1"), bytes, from, None).unwrap().outcome
        });
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
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));
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
            local_writer: Default::default(),
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

/// An accepted entry this binary cannot apply yet, above the removed writer's winner, may be a
/// newer write to that row. A re-adoption at the stream tail would overwrite it on up-to-date
/// peers, which is why it used to wait. A live row is now restated at its own identity (#1488),
/// which beats nothing newer, so the removal drains at once and the parked write still wins
/// wherever it applies.
#[test]
fn readoption_restates_at_identity_under_a_parked_newer_write() {
    let mut a = Device::new();
    let mut c = Device::new();
    let account = AccountId::from_bytes([42; 32]);
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));
    a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'distilled')", []).unwrap();
    let entry = a.produce();
    enroll_writer(&c.conn, account, a.pubkey().fingerprint());
    enroll_writer(&c.conn, account, c.local.fingerprint());
    assert_eq!(c.ingest_all(&entry, &a.pubkey()), vec![IngestOutcome::Applied]);
    remove_writer(&c.conn, account, a.pubkey().fingerprint());
    c.conn
        .execute(
            "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                     received_at_ms, pending_reason
                 ) VALUES (x'77', ?1, ?2, 3, x'00', 0, 'newer_spec_version')",
            rusqlite::params![stream.to_bytes().as_slice(), [2u8; 32].as_slice()],
        )
        .unwrap();
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
    let drain = |c: &mut Device| {
        let tx = c.conn.transaction().unwrap();
        let ctx = SyncCtx {
            repo_id: "repo",
            account_id: account,
            incarnation_ref: [0x44; 32],
            device: &c.local,
            registry: REGISTRY,
            now_ms: 0,
            local_writer: Default::default(),
        };
        let processed = process_readoption_work_for_stream(&tx, &ctx, stream).unwrap();
        tx.commit().unwrap();
        processed
    };

    assert_eq!(drain(&mut c), Some(1), "the row is restated without waiting");
    let own: Vec<u8> = c
        .conn
        .query_row(
            "SELECT signed_bytes FROM table_sync_entries WHERE device_fingerprint = ?1",
            [c.local.fingerprint().to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    let signed = crate::entry::decode_signed(&own).unwrap();
    let Ok(row_op::DecodedRowOp::Known(RowOp::RestateRows { rows, .. })) =
        row_op::decode(&signed.entry.op_bytes)
    else {
        panic!("re-adoption restates the row");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].device, a.pubkey().fingerprint(), "at the removed writer's identity");
    let clock: String = c
        .conn
        .query_row("SELECT device_fingerprint FROM sync_row_clocks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(clock, a.pubkey().fingerprint().to_string(), "the row keeps its writer's clock");
}

/// Enqueue re-adoption of `removed` on `stream` (at removal epoch `epoch`) and drain it.
fn drain_readoption(
    d: &mut Device,
    removed: crate::op::DeviceFingerprint,
    stream: crate::stream::StreamId,
    epoch: u64,
) -> Option<usize> {
    let account = AccountId::from_bytes([42; 32]);
    let tx = d.conn.transaction().unwrap();
    store::enqueue_readoption_work(&tx, account, removed, stream, [7; 32], epoch, 10).unwrap();
    let ctx = SyncCtx {
        repo_id: "repo",
        account_id: account,
        incarnation_ref: [0x44; 32],
        device: &d.local,
        registry: REGISTRY,
        now_ms: 0,
        local_writer: Default::default(),
    };
    let processed = process_readoption_work_for_stream(&tx, &ctx, stream).unwrap();
    tx.commit().unwrap();
    processed
}

/// Every entry `d`'s own chain holds, in lamport order.
fn own_entries(d: &Device) -> Vec<Vec<u8>> {
    d.conn
        .prepare(
            "SELECT signed_bytes FROM table_sync_entries WHERE device_fingerprint = ?1
              ORDER BY lamport",
        )
        .unwrap()
        .query_map([d.local.fingerprint().to_bytes().as_slice()], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// #1488. A removed writer's newer write reached only some stores. Every adopter re-adopts the row
/// from its own view — a lagging one from an older copy — and whichever order the restatements
/// arrive in, every store ends at the newest copy any adopter held. A tail re-author would have
/// let the lagging adopter's older copy win wherever it was re-authored last. (#1479 is the same
/// shape: a store restored from an older backup is a lagging adopter of its retired identity.)
#[test]
fn readoption_at_identity_keeps_the_newest_copy_whatever_the_order() {
    let account = AccountId::from_bytes([42; 32]);
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));
    let mut a = Device::new(); // the writer, later removed
    let mut lagging = Device::new();
    let mut current = Device::new();
    a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'older')", []).unwrap();
    let first = a.produce();
    a.set_title("newer");
    let second = a.produce();
    lagging.ingest_all(&first, &a.pubkey());
    current.ingest_all(&first, &a.pubkey());
    current.ingest_all(&second, &a.pubkey());
    assert_eq!(lagging.title().as_deref(), Some("older"));
    // The lagging adopter has seen MORE of the stream's clock than the current one (its own
    // writes), so anything it signs at the tail outranks what the current adopter signs.
    for id in ["x1", "x2", "x3"] {
        lagging.conn.execute("INSERT INTO t_demo(id, title) VALUES (?1, 'own')", [id]).unwrap();
        lagging.produce();
    }
    for store in [&lagging, &current] {
        enroll_writer(&store.conn, account, lagging.local.fingerprint());
        enroll_writer(&store.conn, account, current.local.fingerprint());
        remove_writer(&store.conn, account, a.pubkey().fingerprint());
    }

    assert_eq!(drain_readoption(&mut lagging, a.pubkey().fingerprint(), stream, 9), Some(1));
    assert_eq!(drain_readoption(&mut current, a.pubkey().fingerprint(), stream, 9), Some(1));
    let (from_lagging, from_current) = (own_entries(&lagging), own_entries(&current));

    // The adopters exchange restatements: the older copy never overwrites the newer one.
    current.ingest_all(&from_lagging, &lagging.pubkey());
    assert_eq!(current.title().as_deref(), Some("newer"));
    lagging.ingest_all(&from_current, &current.pubkey());
    assert_eq!(lagging.title().as_deref(), Some("newer"), "the lagging adopter catches up");

    // Fresh replicas, which refuse A's own entries, reach the newest copy in either order.
    for older_first in [true, false] {
        let mut fresh = Device::new();
        let batches = [(&from_lagging, lagging.pubkey()), (&from_current, current.pubkey())];
        let order: Vec<usize> = if older_first { vec![0, 1] } else { vec![1, 0] };
        for index in order {
            let (entries, from) = &batches[index];
            fresh.ingest_all(entries, from);
        }
        assert_eq!(fresh.title().as_deref(), Some("newer"), "older_first = {older_first}");
        // The row's winning entry is never held here; it resolves through its carrier.
        let tx = fresh.conn.transaction().unwrap();
        let pk = [row_op::TypedValue::Text("r1".into())];
        let apply::SyncedRow::Cells(cells) = apply::read_synced_cells(&tx, &SPEC, &pk).unwrap()
        else {
            panic!("the row is readable")
        };
        assert_eq!(
            apply::stale_row_disposition(&tx, &SPEC, "repo", stream, &pk, &cells).unwrap(),
            apply::StaleRow::Unchanged,
            "the winner resolves through the restating entry",
        );
    }

    // Draining the removal again owes nothing: this chain already carries the row.
    assert_eq!(drain_readoption(&mut current, a.pubkey().fingerprint(), stream, 11), Some(0));
}

/// A row this chain carries at another device's identity pins its restating entry, and compaction
/// carries that pin forward by restating the row at the same identity — never as a tail write of
/// this device's own, which would beat a newer write it has not seen (#1488).
#[test]
fn compaction_moves_a_restated_row_by_restating_it() {
    let account = AccountId::from_bytes([42; 32]);
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));
    let mut a = Device::new();
    let mut c = Device::new();
    a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'kept')", []).unwrap();
    let entries = a.produce();
    c.ingest_all(&entries, &a.pubkey());
    enroll_writer(&c.conn, account, c.local.fingerprint());
    remove_writer(&c.conn, account, a.pubkey().fingerprint());
    assert_eq!(drain_readoption(&mut c, a.pubkey().fingerprint(), stream, 9), Some(1));

    let tx = c.conn.transaction().unwrap();
    let pins = crate::table_sync::retention::chain_pins(
        &tx,
        stream,
        c.local.fingerprint(),
        0,
        1 << 40,
        16,
    )
    .unwrap();
    assert_eq!(pins.len(), 1, "the restating entry carries the row: {pins:?}");
    let ctx = SyncCtx {
        repo_id: "repo",
        account_id: account,
        incarnation_ref: [0x44; 32],
        device: &c.local,
        registry: REGISTRY,
        now_ms: 0,
        local_writer: Default::default(),
    };
    assert_eq!(reauthor_chain_pins(&tx, &ctx, "demo/1", stream, &pins, 4, false).unwrap(), 1);
    tx.commit().unwrap();
    let newest = own_entries(&c).pop().unwrap();
    let signed = crate::entry::decode_signed(&newest).unwrap();
    let Ok(row_op::DecodedRowOp::Known(RowOp::RestateRows { rows, .. })) =
        row_op::decode(&signed.entry.op_bytes)
    else {
        panic!("the pin moves as a restatement");
    };
    assert_eq!(rows[0].device, a.pubkey().fingerprint(), "at the same identity");
    let clock: String = c
        .conn
        .query_row("SELECT device_fingerprint FROM sync_row_clocks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(clock, a.pubkey().fingerprint().to_string());
}

#[test]
fn readoption_never_authors_a_remove_while_the_physical_row_is_live() {
    let mut a = Device::new(); // creates AND deletes r1, then leaves the roster
    let mut c = Device::new();
    let account = AccountId::from_bytes([42; 32]);
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));

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
            local_writer: Default::default(),
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
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));

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
            local_writer: Default::default(),
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
            local_writer: Default::default(),
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
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));

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
            local_writer: Default::default(),
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
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));

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
            local_writer: Default::default(),
        };
        assert_eq!(process_readoption_work_for_stream(&tx, &ctx, stream).unwrap(), Some(1));
        tx.commit().unwrap();
    }
    let original_lamport: i64 = c
        .conn
        .query_row("SELECT original_lamport FROM table_sync_readoption_audit", [], |row| row.get(0))
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
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));

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
            local_writer: Default::default(),
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
        scope_id: ScopeId::new("demo/1"),
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
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));
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
            local_writer: Default::default(),
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
            local_writer: Default::default(),
        };
        for bytes in &entry {
            ingest(&tx, &ctx, ScopeId::new("demo/1"), bytes, &a.pubkey(), None).unwrap();
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
            local_writer: Default::default(),
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
            local_writer: Default::default(),
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
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));

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
        local_writer: Default::default(),
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
    let r2: String =
        c.conn.query_row("SELECT title FROM t_demo WHERE id = 'r2'", [], |row| row.get(0)).unwrap();
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
        "B's own genesis takes lamport 0 — the held entry's lamport 1 is invisible to the clock",
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
    let stream = scope_stream_id(
        "repo",
        AccountId::from_bytes([42; 32]),
        [0x44; 32],
        ScopeId::new("demo/1"),
    );
    let genesis_hash: [u8; 32] = a
        .conn
        .query_row("SELECT entry_hash FROM table_sync_entries ORDER BY lamport LIMIT 1", [], |r| {
            r.get::<_, Vec<u8>>(0)
        })
        .unwrap()
        .try_into()
        .unwrap();
    let sibling = entry::sign_entry_from_op_bytes(
        a.local.secret(),
        stream,
        Some(EntryHash::from_bytes(genesis_hash)),
        9,
        row_op::encode(&row_op::RowOp::Remove {
            table: "t_demo".into(),
            pk: vec![row_op::TypedValue::Text("r9".into())],
            spec_version: 1,
        }),
    );

    // Both successors arrive before the genesis and are held.
    let held = b.ingest_reports(&[second[0].clone(), sibling.signed_bytes.clone()], &a.pubkey());
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
        vec![IngestOutcome::Retained(crate::table_sync::store::PendingReason::DeferredUnsentEdit)],
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

    let stream = scope_stream_id(
        "repo",
        AccountId::from_bytes([42; 32]),
        [0x44; 32],
        ScopeId::new("demo/1"),
    );
    let genesis_hash: [u8; 32] = a
        .conn
        .query_row("SELECT entry_hash FROM table_sync_entries ORDER BY lamport LIMIT 1", [], |r| {
            r.get::<_, Vec<u8>>(0)
        })
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
        Some(EntryHash::from_bytes(genesis_hash)),
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
        "and its held child is abandoned with it, not left keyed to a hash that never lands: {:?}",
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

    let stream = scope_stream_id(
        "repo",
        AccountId::from_bytes([42; 32]),
        [0x44; 32],
        ScopeId::new("demo/1"),
    );
    let genesis_hash: [u8; 32] = a
        .conn
        .query_row("SELECT entry_hash FROM table_sync_entries ORDER BY lamport LIMIT 1", [], |r| {
            r.get::<_, Vec<u8>>(0)
        })
        .unwrap()
        .try_into()
        .unwrap();
    // Lamport 0 ties the genesis's own lamport, so this classifies at/below the tail — a
    // conflict — and sorts BEFORE the real successor at lamport 1.
    let invalid = entry::sign_entry_from_op_bytes(
        a.local.secret(),
        stream,
        Some(EntryHash::from_bytes(genesis_hash)),
        0,
        row_op::encode(&row_op::RowOp::Remove {
            table: "t_demo".into(),
            pk: vec![row_op::TypedValue::Text("r_bogus".into())],
            spec_version: 1,
        }),
    );

    let held = b.ingest_reports(&[invalid.signed_bytes.clone(), successor[0].clone()], &a.pubkey());
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
            .query_row("SELECT title FROM t_demo WHERE id = 'r2'", [], |r| r.get::<_, String>(0))
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
    let stream = scope_stream_id(
        "repo",
        AccountId::from_bytes([42; 32]),
        [0x44; 32],
        ScopeId::new("demo/1"),
    );
    let cross = entry::sign_entry_from_op_bytes(
        c.local.secret(),
        stream,
        Some(EntryHash::from_bytes(a_genesis_hash)),
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

    let stream = scope_stream_id(
        "repo",
        AccountId::from_bytes([42; 32]),
        [0x44; 32],
        ScopeId::new("demo/1"),
    );
    let genesis_hash: [u8; 32] = a
        .conn
        .query_row("SELECT entry_hash FROM table_sync_entries ORDER BY lamport LIMIT 1", [], |r| {
            r.get::<_, Vec<u8>>(0)
        })
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
        Some(EntryHash::from_bytes(genesis_hash)),
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
        "the loser is judged and the OTHER device's citation of it goes too — nothing else could \
         retire that row, since its predecessor is never accepted",
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
        local_writer: Default::default(),
    };
    let error =
        ingest(&tx, &ctx, ScopeId::new("demo/1"), &entries[0], &old.pubkey(), None).unwrap_err();
    assert!(error.to_string().contains("different stream"));
    for table in ["table_sync_entries", "table_sync_gapped_entries", "sync_row_clocks"] {
        let count: i64 =
            tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "old history must not enter {table}");
    }
    tx.rollback().unwrap();
}

/// A read-only replica can never author, so a raw local row of its own — one a migration seeded,
/// or a summary it regenerated for itself — is not unsent work and must not hold a received row
/// back: the deferral's only redeemer is the producer, and this device has none. The received
/// row applies on the merits.
#[test]
fn a_read_only_replica_never_holds_a_received_row_back_for_its_own_edits() {
    let mut a = Device::new();
    let mut b = Device::new();
    b.make_read_only();
    a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
    b.ingest_all(&a.produce(), &a.pubkey());
    b.set_title("local-only");
    a.set_title("from-A");
    assert_eq!(b.ingest_all(&a.produce(), &a.pubkey()), vec![IngestOutcome::Applied]);
    assert_eq!(b.title().as_deref(), Some("from-A"), "the writer's row overrides the local one");
    assert!(b.produce().is_empty(), "and a read-only device authors nothing");
}

/// A REMOVED writer is not a read-only one: its enrolment can come back (a re-invite, or a
/// contested roster fold resolving), and what it edited is then publishable — so the guard holds
/// and the edit survives, exactly as on an effective writer.
#[test]
fn a_removed_writer_keeps_its_unpublished_edit_behind_the_guard() {
    let mut a = Device::new();
    let mut b = Device::new();
    a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
    b.ingest_all(&a.produce(), &a.pubkey());
    b.set_title("unsent-B");
    remove_writer(&b.conn, AccountId::from_bytes([42; 32]), b.local.fingerprint());
    a.set_title("from-A");
    assert_eq!(b.ingest_all(&a.produce(), &a.pubkey()), vec![IngestOutcome::Retained(
        crate::table_sync::store::PendingReason::DeferredUnsentEdit
    )]);
    assert_eq!(b.title().as_deref(), Some("unsent-B"), "the unsent local edit survives");
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
        crate::table_sync::store::PendingReason::DeferredUnsentEdit
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
        scope_id: ScopeId::new("multi/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("v", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    const TB: TableSpec = TableSpec {
        name: "t_b",
        scope_id: ScopeId::new("multi/1"),
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
            local_writer: Default::default(),
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
            local_writer: Default::default(),
        };
        for bytes in &entries {
            assert_eq!(
                ingest(&tx, &ctx, ScopeId::new("multi/1"), bytes, &a_dev.secret().public(), None)
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

// ── re-adoption restates deletes in bounded scopes (#1295) ───────────────────────────────────

const OVERLAY_SPEC: TableSpec = TableSpec { scope_id: ScopeId::OVERLAY, ..SPEC };
const OVERLAY: &[TableSpec] = &[OVERLAY_SPEC];

fn ctx_on<'a>(device: &'a Device, registry: &'a [TableSpec]) -> SyncCtx<'a> {
    SyncCtx {
        repo_id: "repo",
        account_id: AccountId::from_bytes([42; 32]),
        incarnation_ref: [0x44; 32],
        device: &device.local,
        registry,
        now_ms: 0,
        local_writer: Default::default(),
    }
}

fn produce_on(device: &Device, registry: &[TableSpec]) -> Vec<Vec<u8>> {
    let tx = device.conn.unchecked_transaction().unwrap();
    let out = produce_and_author(&tx, &ctx_on(device, registry)).unwrap();
    tx.commit().unwrap();
    out
}

fn ingest_on(
    device: &Device,
    registry: &[TableSpec],
    entries: &[Vec<u8>],
    from: &DevicePublic,
) -> Vec<IngestOutcome> {
    enroll_writer(&device.conn, AccountId::from_bytes([42; 32]), from.fingerprint());
    let scope = registry[0].scope_id;
    let tx = device.conn.unchecked_transaction().unwrap();
    let ctx = ctx_on(device, registry);
    let out = entries
        .iter()
        .map(|bytes| ingest(&tx, &ctx, scope, bytes, from, None).unwrap().outcome)
        .collect();
    tx.commit().unwrap();
    out
}

/// Enqueue and drain the removal of `removed` on `stream`, returning the drain's answer.
fn drain_removal(
    device: &Device,
    registry: &[TableSpec],
    removed: crate::op::DeviceFingerprint,
    stream: crate::stream::StreamId,
) -> Option<usize> {
    let account = AccountId::from_bytes([42; 32]);
    remove_writer(&device.conn, account, removed);
    let tx = device.conn.unchecked_transaction().unwrap();
    store::enqueue_readoption_work(&tx, account, removed, stream, [8; 32], 11, 12).unwrap();
    let out = process_readoption_work_for_stream(&tx, &ctx_on(device, registry), stream).unwrap();
    tx.commit().unwrap();
    out
}

/// This device's own chain, oldest first, as signed bytes.
fn own_chain(device: &Device) -> Vec<Vec<u8>> {
    device
        .conn
        .prepare(
            "SELECT signed_bytes FROM table_sync_entries WHERE device_fingerprint = ?1
             ORDER BY lamport",
        )
        .unwrap()
        .query_map([device.local.fingerprint().to_bytes().as_slice()], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn decoded(bytes: &[u8]) -> RowOp {
    let signed = crate::entry::decode_signed(bytes).unwrap();
    match crate::table_sync::row_op::decode(&signed.entry.op_bytes).unwrap() {
        crate::table_sync::row_op::DecodedRowOp::Known(op) => op,
        other => panic!("known op, got {other:?}"),
    }
}

fn tombstone_identity(device: &Device) -> Option<String> {
    device
        .conn
        .query_row("SELECT device_fingerprint FROM sync_row_tombstones", [], |row| row.get(0))
        .optional()
        .unwrap()
}

fn statements(device: &Device) -> Vec<(String, i64)> {
    device
        .conn
        .prepare(
            "SELECT device_fingerprint, lamport FROM sync_tombstone_statements ORDER BY lamport",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// In a bounded scope, re-adopting a removed writer's delete restates it at its identity: the
/// adopter's chain now states the tombstone (so its floor can move past it later), the identity
/// stays the removed writer's, and a fresh replica folding the adopter's chain alone holds the
/// delete.
#[test]
fn removing_a_writer_adds_the_adopters_statement_and_keeps_the_identity() {
    let a = Device::new();
    let c = Device::new();
    let account = AccountId::from_bytes([42; 32]);
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::OVERLAY);

    a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'created')", []).unwrap();
    let create = produce_on(&a, OVERLAY);
    ingest_on(&c, OVERLAY, &create, &a.pubkey());
    a.delete_row();
    let delete = produce_on(&a, OVERLAY);
    ingest_on(&c, OVERLAY, &delete, &a.pubkey());
    let a_hex = a.pubkey().fingerprint().to_string();
    assert_eq!(statements(&c), vec![(a_hex.clone(), 1)], "A's remove states its delete");

    assert_eq!(drain_removal(&c, OVERLAY, a.pubkey().fingerprint(), stream), Some(1));
    let chain = own_chain(&c);
    assert_eq!(chain.len(), 1, "one restatement");
    let RowOp::Restate { deletes, .. } = decoded(&chain[0]) else { panic!("a restate") };
    assert_eq!(deletes.len(), 1);
    assert_eq!((deletes[0].device, deletes[0].lamport), (a.pubkey().fingerprint(), 1));
    assert_eq!(tombstone_identity(&c).as_deref(), Some(a_hex.as_str()), "identity unchanged");
    let c_hex = c.local.fingerprint().to_string();
    assert_eq!(statements(&c), vec![(a_hex.clone(), 1), (c_hex, 2)], "and C states it too");
    let audits: i64 = c
        .conn
        .query_row("SELECT COUNT(*) FROM table_sync_readoption_audit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(audits, 1);

    // A fresh replica that only ever sees C's chain holds the delete.
    let d = Device::new();
    assert_eq!(ingest_on(&d, OVERLAY, &chain, &c.pubkey()), vec![IngestOutcome::Applied]);
    assert_eq!(d.title(), None);
    assert_eq!(tombstone_identity(&d).as_deref(), Some(a_hex.as_str()));

    // Draining the same removal again re-signs nothing: C already states it.
    let tx = c.conn.unchecked_transaction().unwrap();
    store::enqueue_readoption_work(&tx, account, a.pubkey().fingerprint(), stream, [8; 32], 12, 13)
        .unwrap();
    assert_eq!(
        process_readoption_work_for_stream(&tx, &ctx_on(&c, OVERLAY), stream).unwrap(),
        Some(0)
    );
    tx.commit().unwrap();
}

/// A fully retained scope keeps today's tail-signed `Remove` for a re-adopted delete: nothing
/// there is ever compacted, and a restatement would only park on a pre-restate device.
#[test]
fn anchors_re_adoption_keeps_the_tail_signed_remove() {
    let a = Device::new();
    let c = Device::new();
    let account = AccountId::from_bytes([42; 32]);
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));
    a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'created')", []).unwrap();
    let create = produce_on(&a, REGISTRY);
    ingest_on(&c, REGISTRY, &create, &a.pubkey());
    a.delete_row();
    let delete = produce_on(&a, REGISTRY);
    ingest_on(&c, REGISTRY, &delete, &a.pubkey());

    assert_eq!(drain_removal(&c, REGISTRY, a.pubkey().fingerprint(), stream), Some(1));
    let chain = own_chain(&c);
    assert!(matches!(decoded(&chain[0]), RowOp::Remove { .. }), "a tail-signed remove");
    let c_hex = c.local.fingerprint().to_string();
    assert_eq!(tombstone_identity(&c).as_deref(), Some(c_hex.as_str()), "the identity moved");
}

/// A delete parked while its author was still a writer, and replayed after that author's
/// removal already drained, re-arms the removal: the drain that follows carries the delete under
/// a live chain. Without this the tombstone would sit under a removed identity no chain states.
#[test]
fn replay_after_removal_re_arms_re_adoption() {
    const BOOL_SPEC: TableSpec = TableSpec {
        name: "t_typed",
        scope_id: ScopeId::OVERLAY,
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("flag", ValueType::Bool)],
        local_columns: &[],
        repo_column: None,
    };
    const BOOL: &[TableSpec] = &[BOOL_SPEC];
    let a = Device::new();
    let c = Device::new();
    let account = AccountId::from_bytes([42; 32]);
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::OVERLAY);
    for device in [&a, &c] {
        device
            .conn
            .execute_batch("CREATE TABLE t_typed(id TEXT PRIMARY KEY, flag INTEGER) STRICT;")
            .unwrap();
    }
    // C owns r1; A deletes it. On C an unreadable cell holds A's delete back.
    c.conn.execute("INSERT INTO t_typed(id, flag) VALUES ('r1', 1)", []).unwrap();
    let create = produce_on(&c, BOOL);
    ingest_on(&a, BOOL, &create, &c.pubkey());
    a.conn.execute("DELETE FROM t_typed WHERE id = 'r1'", []).unwrap();
    let delete = produce_on(&a, BOOL);
    c.conn.execute("UPDATE t_typed SET flag = 2 WHERE id = 'r1'", []).unwrap();
    assert_eq!(ingest_on(&c, BOOL, &delete, &a.pubkey()), vec![IngestOutcome::Retained(
        store::PendingReason::DeferredUnreadableRow
    )]);
    // A is removed while its delete is parked: A owns nothing on C, so the drain completes.
    assert_eq!(drain_removal(&c, BOOL, a.pubkey().fingerprint(), stream), Some(0));
    // The cell is repaired and the next producer pass replays the delete: it beats C's clock,
    // and the merge state it leaves is a tombstone under A — a removed writer no chain states.
    c.conn.execute("UPDATE t_typed SET flag = 1 WHERE id = 'r1'", []).unwrap();
    assert!(produce_on(&c, BOOL).is_empty(), "nothing local to author; the pass is the replay");
    let a_hex = a.pubkey().fingerprint().to_string();
    assert_eq!(tombstone_identity(&c).as_deref(), Some(a_hex.as_str()));
    let tx = c.conn.unchecked_transaction().unwrap();
    assert!(
        store::has_pending_readoption_work(&tx, account, stream).unwrap(),
        "the completed removal is re-armed by the replay"
    );
    assert_eq!(
        process_readoption_work_for_stream(&tx, &ctx_on(&c, BOOL), stream).unwrap(),
        Some(1)
    );
    tx.commit().unwrap();
    let c_hex = c.local.fingerprint().to_string();
    assert!(statements(&c).iter().any(|(device, _)| *device == c_hex), "C states A's delete");
    let last = own_chain(&c).pop().unwrap();
    assert!(matches!(decoded(&last), RowOp::Restate { .. }));
    assert_eq!(tombstone_identity(&c).as_deref(), Some(a_hex.as_str()), "identity unchanged");
}

/// In a fully retained scope a re-adopted delete is a tail-signed `Remove`, a new identity above
/// every accepted entry — so, like a re-authored row, it waits while a parked newer write sits
/// above the tombstone: peers that understand that write have applied it, and the remove would
/// suppress it there. Only a restatement, which settles at the original identity, is never held.
#[test]
fn anchors_re_adoption_holds_a_tail_signed_remove_below_a_parked_newer_write() {
    let a = Device::new();
    let c = Device::new();
    let account = AccountId::from_bytes([42; 32]);
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::new("demo/1"));
    a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'created')", []).unwrap();
    let create = produce_on(&a, REGISTRY);
    ingest_on(&c, REGISTRY, &create, &a.pubkey());
    a.delete_row();
    let delete = produce_on(&a, REGISTRY);
    ingest_on(&c, REGISTRY, &delete, &a.pubkey()); // tombstone (A, 1)
    // A newer write to r1 this binary cannot apply yet, parked above the tombstone.
    c.conn
        .execute(
            "INSERT INTO table_sync_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                 received_at_ms, pending_reason
             ) VALUES (x'77', ?1, ?2, 2, x'00', 0, 'newer_spec_version')",
            rusqlite::params![stream.to_bytes().as_slice(), [2u8; 32].as_slice()],
        )
        .unwrap();
    assert_eq!(
        drain_removal(&c, REGISTRY, a.pubkey().fingerprint(), stream),
        None,
        "the tail-signed remove waits for the parked write"
    );
    assert!(own_chain(&c).is_empty(), "nothing was signed over it");
    c.conn
        .execute("UPDATE table_sync_entries SET pending_reason = NULL WHERE entry_hash = x'77'", [])
        .unwrap();
    let tx = c.conn.unchecked_transaction().unwrap();
    assert_eq!(
        process_readoption_work_for_stream(&tx, &ctx_on(&c, REGISTRY), stream).unwrap(),
        Some(1)
    );
    tx.commit().unwrap();
    assert!(matches!(decoded(&own_chain(&c)[0]), RowOp::Remove { .. }));
}

#[test]
fn readopting_a_statement_carrier_preserves_actual_provenance() {
    let a = Device::new();
    let b = Device::new();
    let c = Device::new();
    let account = AccountId::from_bytes([42; 32]);
    let stream = scope_stream_id("repo", account, [0x44; 32], ScopeId::OVERLAY);
    a.conn.execute("INSERT INTO t_demo(id,title) VALUES ('r1','created')", []).unwrap();
    ingest_on(&b, OVERLAY, &produce_on(&a, OVERLAY), &a.pubkey());
    a.delete_row();
    ingest_on(&b, OVERLAY, &produce_on(&a, OVERLAY), &a.pubkey());
    assert_eq!(drain_removal(&b, OVERLAY, a.local.fingerprint(), stream), Some(1));
    let chain = own_chain(&b);
    let actual = crate::entry::decode_signed(&chain[0]).unwrap();
    assert_eq!(actual.entry.lamport, 2);
    ingest_on(&c, OVERLAY, &chain, &b.pubkey());
    assert_eq!(drain_removal(&c, OVERLAY, b.local.fingerprint(), stream), Some(1));
    let recorded: (i64, Option<Vec<u8>>) = c
        .conn
        .query_row(
            "SELECT original_lamport, original_entry_hash FROM table_sync_readoption_audit",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        recorded.0 as u64, actual.entry.lamport,
        "provenance must name B's actual statement, not A's delete lamport"
    );
    assert_eq!(recorded.1.as_deref(), Some(actual.entry.entry_hash.as_slice()));
    assert_eq!(tombstone_identity(&c), Some(a.local.fingerprint().to_string()));
    let RowOp::Restate { deletes, .. } = decoded(&own_chain(&c)[0]) else {
        panic!("the adopter must restate the original delete");
    };
    assert_eq!(deletes[0].device, a.local.fingerprint());
    assert_eq!(deletes[0].lamport, 1);
}
