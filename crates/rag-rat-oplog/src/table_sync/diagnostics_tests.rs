use super::*;
use crate::stream::StreamId;
use crate::table_sync::diagnostics::{self, TableSyncDiagnosticQuery, TableSyncRowCause as Cause};
use crate::table_sync::{produce, transport};

fn stream() -> StreamId {
    scope_stream_id("repo", account(), [0x44; 32], OLD.scope_id)
}

pub(super) fn causes(conn: &rusqlite::Connection) -> Vec<String> {
    diagnostics::table_sync_row_diagnostics(conn, &TableSyncDiagnosticQuery {
        stream_id: stream().to_bytes(),
        repo_id: "repo",
        after: None,
        limit: 100,
    })
    .unwrap()
    .into_iter()
    .map(|r| r.cause)
    .collect()
}

fn published() -> Device {
    let mut d = Device::new();
    d.conn.execute("INSERT INTO t_demo(id,title) VALUES ('r1','value')", []).unwrap();
    d.produce(OLD_REGISTRY, "repo");
    d
}

fn compare(d: &mut Device) -> apply::StaleRow {
    let tx = d.conn.transaction().unwrap();
    let pk = [TypedValue::Text("r1".into())];
    let apply::SyncedRow::Cells(cells) = apply::read_synced_cells(&tx, &NEW, &pk).unwrap() else {
        panic!("readable row")
    };
    let result = apply::stale_row_disposition(&tx, &NEW, "repo", stream(), &pk, &cells).unwrap();
    tx.commit().unwrap();
    result
}

#[test]
fn unresolved_winner_causes_are_distinct_and_clear_when_repaired() {
    for (sql, cause) in [
        ("DELETE FROM sync_row_clocks", Cause::MissingClock),
        ("UPDATE sync_row_clocks SET device_fingerprint = 'invalid'", Cause::InvalidClockDevice),
        ("DELETE FROM table_sync_entries", Cause::MissingEntry),
        ("UPDATE table_sync_entries SET signed_bytes = X'00'", Cause::UndecodableEntry),
    ] {
        let mut d = published();
        d.conn
            .execute_batch(
                "CREATE TEMP TABLE entries_backup AS SELECT * FROM table_sync_entries; CREATE \
                 TEMP TABLE clocks_backup AS SELECT * FROM sync_row_clocks;",
            )
            .unwrap();
        d.conn.execute_batch(sql).unwrap();
        assert_eq!(compare(&mut d), apply::StaleRow::Unknown(cause));
        assert_eq!(causes(&d.conn), [cause.as_db_str()]);
        // Successful re-authoring is the producer's repair of an unresolved winner.
        // A missing clock device cannot poison the next valid self-apply.
        d.conn
            .execute_batch(
                "DELETE FROM table_sync_entries; INSERT INTO table_sync_entries SELECT * FROM \
                 entries_backup; DELETE FROM sync_row_clocks; INSERT INTO sync_row_clocks SELECT \
                 * FROM clocks_backup; UPDATE t_demo SET title = 'edited';",
            )
            .unwrap();
        d.produce(NEW_REGISTRY, "repo");
        assert!(causes(&d.conn).is_empty());
    }
}

#[test]
fn winner_identity_and_projection_causes_are_distinct() {
    for (op, cause) in [
        (
            RowOp::Remove {
                table: "t_demo".into(),
                spec_version: 1,
                pk: vec![TypedValue::Text("r1".into())],
            },
            Cause::WrongOperation,
        ),
        (
            RowOp::Upsert {
                table: "other".into(),
                spec_version: 1,
                pk: vec![TypedValue::Text("r1".into())],
                cells: vec![],
            },
            Cause::WrongTable,
        ),
        (
            RowOp::Upsert {
                table: "t_demo".into(),
                spec_version: 1,
                pk: vec![TypedValue::Text("other".into())],
                cells: vec![],
            },
            Cause::WrongKey,
        ),
        (
            RowOp::Upsert {
                table: "t_demo".into(),
                spec_version: 2,
                pk: vec![TypedValue::Text("r1".into())],
                cells: vec![],
            },
            Cause::UnprojectableWinner,
        ),
    ] {
        let mut d = published();
        let tx = d.conn.transaction().unwrap();
        let signed = store::author_row_entry(&tx, stream(), d.local.secret(), &op, 1).unwrap();
        tx.execute("UPDATE sync_row_clocks SET lamport = ?1", [signed.entry.lamport as i64])
            .unwrap();
        tx.commit().unwrap();
        assert_eq!(compare(&mut d), apply::StaleRow::Unknown(cause));
        assert_eq!(causes(&d.conn), [cause.as_db_str()]);
    }
}

#[test]
fn producer_rollback_keeps_diagnostics_but_no_failed_signed_entry() {
    let d = published();
    d.conn.execute("UPDATE sync_row_clocks SET lamport = 9999", []).unwrap();
    d.conn.execute("UPDATE t_demo SET title = 'unsent'", []).unwrap();
    let before: i64 =
        d.conn.query_row("SELECT COUNT(*) FROM table_sync_entries", [], |r| r.get(0)).unwrap();
    let ctx = SyncCtx {
        repo_id: "repo",
        account_id: account(),
        incarnation_ref: [0x44; 32],
        device: &d.local,
        registry: NEW_REGISTRY,
        now_ms: 1,
        local_writer: Default::default(),
    };
    for _ in 0..2 {
        let err = transport::author_repo_pending(&d.conn, &ctx).unwrap_err();
        assert!(err.to_string().contains("lost its own self-apply"));
        assert_eq!(causes(&d.conn), ["missing_entry"]);
        let count: i64 =
            d.conn.query_row("SELECT COUNT(*) FROM table_sync_entries", [], |r| r.get(0)).unwrap();
        assert_eq!(count, before);
        assert_eq!(d.row().unwrap().0, "unsent");
    }
    d.conn.execute("UPDATE sync_row_clocks SET lamport = 0", []).unwrap();
    assert_eq!(transport::author_repo_pending(&d.conn, &ctx).unwrap(), 1);
    assert!(causes(&d.conn).is_empty());
}

#[test]
fn scan_only_unreadable_rows_survive_restart_and_clear_after_repair() {
    const FLAG: TableSpec = TableSpec {
        name: "t_flag",
        scope_id: OLD.scope_id,
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("flag", ValueType::Bool)],
        local_columns: &[],
        repo_column: None,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn.execute_batch(
            "CREATE TABLE t_flag(id TEXT PRIMARY KEY,flag INTEGER) STRICT; INSERT INTO t_flag \
             VALUES ('bad',2);",
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        assert!(produce::produce_row_ops(&tx, &FLAG, "repo", stream()).unwrap().is_empty());
        tx.commit().unwrap();
    }
    let mut conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(causes(&conn), ["unreadable_row"]);
    conn.execute("UPDATE t_flag SET flag = 1", []).unwrap();
    let tx = conn.transaction().unwrap();
    assert_eq!(produce::produce_row_ops(&tx, &FLAG, "repo", stream()).unwrap().len(), 1);
    tx.commit().unwrap();
    assert!(causes(&conn).is_empty());
}

#[test]
fn diagnostic_query_is_bounded_scoped_and_tolerates_future_causes() {
    let mut d = Device::new();
    let tx = d.conn.transaction().unwrap();
    for repo in ["repo", "sibling"] {
        for n in 0..1002 {
            diagnostics::record(
                &tx,
                &apply::RowKey {
                    stream: stream(),
                    repo_id: repo,
                    table: "t_demo",
                    row_pk: &format!("{n:04}"),
                },
                Cause::MissingClock,
            )
            .unwrap();
        }
    }
    diagnostics::record(
        &tx,
        &apply::RowKey {
            stream: StreamId::from_bytes([88; 32]),
            repo_id: "repo",
            table: "t_demo",
            row_pk: "foreign",
        },
        Cause::MissingClock,
    )
    .unwrap();
    tx.execute(
        "UPDATE table_sync_row_diagnostics SET cause = 'future_cause' WHERE row_pk = '0000'",
        [],
    )
    .unwrap();
    tx.commit().unwrap();
    let mut query = TableSyncDiagnosticQuery {
        stream_id: stream().to_bytes(),
        repo_id: "repo",
        after: None,
        limit: usize::MAX,
    };
    let page = diagnostics::table_sync_row_diagnostics(&d.conn, &query).unwrap();
    assert_eq!(page.len(), 1000);
    assert_eq!(page[0].cause, "future_cause");
    query.after = Some((&page[999].table_name, &page[999].row_pk));
    let rest = diagnostics::table_sync_row_diagnostics(&d.conn, &query).unwrap();
    assert_eq!(rest.iter().map(|r| r.row_pk.as_str()).collect::<Vec<_>>(), ["1000", "1001"]);
    query.limit = 0;
    assert!(diagnostics::table_sync_row_diagnostics(&d.conn, &query).unwrap().is_empty());
}

#[test]
fn diagnostic_tokens_are_exact_and_stable() {
    use strum::IntoEnumIterator;
    let tokens: Vec<_> = Cause::iter().map(Cause::as_db_str).collect();
    assert_eq!(tokens, [
        "missing_clock",
        "invalid_clock_device",
        "missing_entry",
        "undecodable_entry",
        "unknown_operation",
        "wrong_operation",
        "wrong_table",
        "wrong_key",
        "unprojectable_winner",
        "unreadable_row",
        "self_apply_superseded"
    ]);
    for token in tokens {
        assert_eq!(Cause::from_db_str(token).unwrap().as_db_str(), token);
    }
    assert_eq!(Cause::from_db_str("MISSING_CLOCK"), None);
    assert_eq!(Cause::from_db_str("future_cause"), None);
}

#[test]
fn opaque_and_malformed_winner_payloads_have_separate_causes() {
    use crate::cbor::VecEncoderExt;
    let mut bytes = Vec::new();
    let mut enc = minicbor::Encoder::new(&mut bytes);
    enc.put_array(3);
    enc.put_str("rag-rat/table-op/1");
    enc.put_str("future");
    enc.put_array(0);
    for (payload, expected) in
        [(bytes, Cause::UnknownOperation), (vec![0], Cause::UndecodableEntry)]
    {
        let mut d = published();
        let signed =
            crate::entry::sign_entry_from_op_bytes(d.local.secret(), stream(), None, 0, payload);
        d.conn
            .execute("UPDATE table_sync_entries SET signed_bytes = ?1", [signed.signed_bytes])
            .unwrap();
        assert_eq!(compare(&mut d), apply::StaleRow::Unknown(expected));
    }
}

#[test]
fn superseded_self_apply_is_reported_when_the_winner_itself_resolves() {
    let mut d = published();
    d.conn
        .execute(
            "INSERT INTO \
             sync_row_tombstones(stream_id,repo_id,table_name,row_pk,lamport,device_fingerprint) \
             SELECT stream_id,repo_id,table_name,row_pk,9999,device_fingerprint FROM \
             sync_row_clocks",
            [],
        )
        .unwrap();
    d.conn.execute("UPDATE t_demo SET title = 'unsent'", []).unwrap();
    let ctx = SyncCtx {
        repo_id: "repo",
        account_id: account(),
        incarnation_ref: [0x44; 32],
        device: &d.local,
        registry: OLD_REGISTRY,
        now_ms: 1,
        local_writer: Default::default(),
    };
    let err = transport::author_repo_pending(&d.conn, &ctx).unwrap_err();
    assert!(err.to_string().contains("lost its own self-apply"));
    assert_eq!(causes(&d.conn), ["self_apply_superseded"]);
    assert_eq!(d.row().unwrap().0, "unsent");
    assert_eq!(
        d.conn
            .query_row("SELECT COUNT(*) FROM table_sync_entries", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let peer = Device::new();
    d.enroll(peer.local.fingerprint());
    let incoming = RowOp::Upsert {
        table: "t_demo".into(),
        spec_version: 1,
        pk: vec![TypedValue::Text("r1".into())],
        cells: vec![Cell { column: "title".into(), value: TypedValue::Text("incoming".into()) }],
    };
    let signed = crate::entry::sign_entry_from_op_bytes(
        peer.local.secret(),
        stream(),
        None,
        10_000,
        row_op::encode(&incoming),
    );
    assert_eq!(d.ingest(OLD_REGISTRY, "repo", &[signed.signed_bytes], &peer.pubkey()), [
        IngestOutcome::Retained(PendingReason::DeferredUnsentEdit)
    ]);
    assert_eq!(
        causes(&d.conn),
        ["self_apply_superseded"],
        "comparable-hash deferral proves no repair"
    );
    let tx = d.conn.transaction().unwrap();
    assert_eq!(
        apply::unsent_work_blocking_replay(&tx, &NEW, "repo", stream(), &incoming).unwrap(),
        Some(PendingReason::DeferredUnsentEdit)
    );
    tx.commit().unwrap();
    assert_eq!(
        causes(&d.conn),
        ["self_apply_superseded"],
        "resolving the winning entry does not resolve the blocking tombstone"
    );
}

#[test]
fn readoption_failure_rolls_back_entries_and_retains_scan_only_diagnostics() {
    const SCOPED: TableSpec = TableSpec {
        name: "t_scoped",
        scope_id: OLD.scope_id,
        spec_version: 1,
        pk: &[
            ColumnSpec::required("repo_id", ValueType::Text),
            ColumnSpec::required("id", ValueType::Text),
        ],
        columns: &[ColumnSpec::required("title", ValueType::Text)],
        local_columns: &[],
        repo_column: Some("repo_id"),
    };
    const FLAG: TableSpec = TableSpec {
        name: "t_flag",
        columns: &[ColumnSpec::required("flag", ValueType::Bool)],
        ..SCOPED
    };
    const REGISTRY: &[TableSpec] = &[SCOPED, FLAG];
    let mut a = Device::new();
    let mut b = Device::new();
    for d in [&a, &b] {
        d.conn
            .execute_batch(
                "CREATE TABLE t_scoped(repo_id TEXT NOT NULL,id TEXT NOT NULL,title TEXT,PRIMARY \
                 KEY(repo_id,id)) STRICT; CREATE TABLE t_flag(repo_id TEXT NOT NULL,id TEXT NOT \
                 NULL,flag INTEGER,PRIMARY KEY(repo_id,id)) STRICT;",
            )
            .unwrap();
    }
    a.conn.execute("INSERT INTO t_scoped VALUES ('repo','r1','unchanged')", []).unwrap();
    let entries = a.produce(REGISTRY, "repo");
    b.enroll(a.local.fingerprint());
    b.ingest(REGISTRY, "repo", &entries, &a.pubkey());
    b.conn
        .execute("UPDATE account_roster_history SET closed_at = 1 WHERE device_fingerprint = ?1", [
            a.local.fingerprint().to_bytes().as_slice(),
        ])
        .unwrap();
    b.conn
        .execute(
            "INSERT INTO \
             sync_row_tombstones(stream_id,repo_id,table_name,row_pk,lamport,device_fingerprint) \
             SELECT stream_id,repo_id,table_name,row_pk,9999,device_fingerprint FROM \
             sync_row_clocks",
            [],
        )
        .unwrap();
    b.conn.execute("INSERT INTO t_flag VALUES ('repo','bad',2)", []).unwrap();
    let tx = b.conn.transaction().unwrap();
    store::enqueue_readoption_work(&tx, account(), a.local.fingerprint(), stream(), [7; 32], 9, 10)
        .unwrap();
    tx.commit().unwrap();
    let ctx = SyncCtx {
        repo_id: "repo",
        account_id: account(),
        incarnation_ref: [0x44; 32],
        device: &b.local,
        registry: REGISTRY,
        now_ms: 11,
        local_writer: Default::default(),
    };
    let error = transport::author_repo_pending(&b.conn, &ctx).unwrap_err();
    assert!(error.to_string().contains("lost its own self-apply"));
    assert_eq!(causes(&b.conn), ["unreadable_row", "self_apply_superseded"]);
    assert_eq!(
        b.conn
            .query_row("SELECT COUNT(*) FROM table_sync_entries", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        entries.len() as i64
    );
    assert_eq!(
        b.conn.query_row("SELECT title FROM t_scoped", [], |r| r.get::<_, String>(0)).unwrap(),
        "unchanged"
    );
}
