use rusqlite::params;

use super::*;
use crate::table_sync::registry::{ColumnSpec, ValueType};
use crate::table_sync::row_op::DecodedRowOp;
use crate::table_sync::{Cell, RowOp, TypedValue};

#[test]
fn malformed_chain_tail_hash_names_the_stored_field() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE table_sync_entries(
                stream_id BLOB, device_fingerprint BLOB, lamport INTEGER, entry_hash BLOB
            )",
    )
    .unwrap();
    let stream = [1_u8; 32];
    let device = [2_u8; 32];
    for len in [0, 31, 33] {
        conn.execute("DELETE FROM table_sync_entries", []).unwrap();
        conn.execute("INSERT INTO table_sync_entries VALUES (?1, ?2, 1, ?3)", params![
            stream.as_slice(),
            device.as_slice(),
            vec![0_u8; len]
        ])
        .unwrap();
        assert_eq!(
            accepted_chain_tail(&conn, stream, device).unwrap_err().to_string(),
            format!("stored entry_hash must be 32 bytes, got {len}"),
        );
    }
}

const INCARNATION: [u8; 32] = [0x24; 32];

fn account() -> AccountId {
    AccountId::from_bytes([0x42; 32])
}

const REPO_SPEC: TableSpec = TableSpec {
    name: "t_transport",
    scope_id: ScopeId::ANCHORS,
    spec_version: 1,
    pk: &[
        ColumnSpec::required("repo_id", ValueType::Text),
        ColumnSpec::required("id", ValueType::Text),
    ],
    columns: &[ColumnSpec::required("title", ValueType::Text)],
    local_columns: &[],
    repo_column: Some("repo_id"),
};
const GLOBAL_SPEC: TableSpec = TableSpec {
    name: "t_global",
    scope_id: ScopeId::new("global/1"),
    spec_version: 1,
    pk: &[ColumnSpec::required("id", ValueType::Text)],
    columns: &[ColumnSpec::required("title", ValueType::Text)],
    local_columns: &[],
    repo_column: None,
};

fn database() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
    crate::local_device(&conn, 0).unwrap();
    conn.execute(
        "INSERT INTO account_repo_incarnation_current(account_id, repository_id, incarnation_ref)
             VALUES (?1, 'repo-a', ?2), (?1, 'repo-b', ?2)",
        params![account().to_bytes().as_slice(), INCARNATION.as_slice()],
    )
    .unwrap();
    conn
}

#[test]
fn supported_streams_are_current_repo_scoped_and_account_global_specs_are_ignored() {
    let conn = database();
    let streams = supported_streams_against(&conn, account(), &[REPO_SPEC, GLOBAL_SPEC]).unwrap();
    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].repo_id, "repo-a");
    assert_eq!(streams[1].repo_id, "repo-b");
    assert!(streams.iter().all(|stream| stream.scope_id == "anchors/1"));
    assert!(streams.iter().all(|stream| {
        validate_stream_against(&conn, account(), stream, &[REPO_SPEC, GLOBAL_SPEC]).unwrap()
    }));
    assert!(supported_streams_against(&conn, account(), &[GLOBAL_SPEC]).unwrap().is_empty());
}

#[test]
fn local_authority_rejects_stale_unknown_and_forged_routes_without_writes() {
    let conn = database();
    let current = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
    let mut stale = current.clone();
    stale.incarnation_ref = [9; 32];
    let mut unknown = current.clone();
    unknown.scope_id = "unknown/1".into();
    let mut forged = current.clone();
    forged.stream_id = [8; 32];
    for route in [&stale, &unknown, &forged] {
        assert!(!validate_stream_against(&conn, account(), route, &[REPO_SPEC]).unwrap());
        assert_eq!(
            ingest_against(
                &conn,
                &IngestRoute { account_id: account(), stream: route, registry: &[REPO_SPEC] },
                crate::op::DeviceFingerprint::from_bytes([0; 32]),
                &[0],
                0,
                None,
                &Default::default(),
            )
            .unwrap(),
            TableSyncIngestOutcome::NoChange,
        );
    }
    assert!(
        !validate_stream_against(&conn, AccountId::from_bytes([0x43; 32]), &current, &[REPO_SPEC],)
            .unwrap(),
        "a sibling account cannot adopt this account's repo stream",
    );
    for table in
        ["table_sync_entries", "table_sync_gapped_entries", "sync_row_clocks", "table_sync_streams"]
    {
        let count: i64 =
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "invalid routes write no rows to {table}");
    }
}

#[test]
fn the_compaction_driver_is_idempotent_past_pending_entries() {
    let conn = database();
    let stream = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
    conn.execute(
        "INSERT INTO table_sync_streams(stream_id, repo_id, account_id, incarnation_ref, \
         scope_id) VALUES (?1, 'repo-a', ?2, ?3, 'anchors/1')",
        params![
            stream.stream_id.as_slice(),
            account().to_bytes().as_slice(),
            INCARNATION.as_slice()
        ],
    )
    .unwrap();
    for lamport in 0..5i64 {
        conn.execute(
            "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                     received_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, x'00', 0)",
            params![
                [(lamport + 1) as u8; 32].as_slice(),
                stream.stream_id.as_slice(),
                [2u8; 32].as_slice(),
                lamport
            ],
        )
        .unwrap();
    }
    // The tip carries a pending mark: reclaimable entries pin below it.
    conn.execute(
        "UPDATE table_sync_entries SET pending_reason = 'unknown_column' WHERE lamport = 4",
        [],
    )
    .unwrap();

    let keep_one = &|_scope: &str| Some(1);
    let first = table_sync_compact_overdue(&conn, account(), 1, keep_one).unwrap();
    assert_eq!(first, 3, "reclaimable entries 0..3 drop; the pending tip survives");
    let floor: Option<i64> =
        conn.query_row("SELECT lamport FROM table_sync_retained_floors", [], |row| row.get(0)).ok();
    assert_eq!(floor, Some(3));

    let second = table_sync_compact_overdue(&conn, account(), 2, keep_one).unwrap();
    assert_eq!(second, 0, "a chain pinned by its pending tip computes a non-advancing floor");
    let remaining: Vec<i64> = conn
        .prepare("SELECT lamport FROM table_sync_entries ORDER BY lamport")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(remaining, vec![3, 4], "nothing moves on the second pass");

    let zero = &|_scope: &str| Some(0);
    assert!(
        table_sync_compact_overdue(&conn, account(), 3, zero).is_err(),
        "a zero budget would drop the chain tail itself — refused"
    );
}

#[test]
fn scope_retention_budget_bounds_overlay_and_leaves_anchors_full() {
    assert_eq!(scope_retention_budget("overlay/1"), Some(OVERLAY_ACCEPTED_RETENTION));
    assert_eq!(scope_retention_budget("distill/1"), Some(DISTILL_ACCEPTED_RETENTION));
    assert_eq!(scope_retention_budget("anchors/1"), None);
    assert_eq!(scope_retention_budget("unknown/1"), None);
}

/// Seed `count` reclaimable (non-pending) accepted entries on a single-device chain, with
/// globally-unique entry hashes derived from `(tag, lamport)`. Records the stream's apply
/// context first — compaction needs it to restamp a dropped winner's published record.
fn seed_reclaimable_chain(conn: &Connection, stream: &TableSyncStream, tag: u8, count: i64) {
    conn.execute(
        "INSERT INTO table_sync_streams(stream_id, repo_id, account_id, incarnation_ref, \
         scope_id) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            stream.stream_id.as_slice(),
            stream.repo_id,
            account().to_bytes().as_slice(),
            stream.incarnation_ref.as_slice(),
            stream.scope_id,
        ],
    )
    .unwrap();
    for lamport in 0..count {
        let mut entry_hash = [0u8; 32];
        entry_hash[0] = tag;
        entry_hash[1..9].copy_from_slice(&(lamport as u64).to_le_bytes());
        conn.execute(
            "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                     received_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, x'00', 0)",
            params![
                entry_hash.as_slice(),
                stream.stream_id.as_slice(),
                [7u8; 32].as_slice(),
                lamport
            ],
        )
        .unwrap();
    }
}

#[test]
fn the_production_policy_compacts_overlay_chains_and_leaves_anchors_full() {
    let conn = database();
    let streams = table_sync_supported_streams(&conn, account()).unwrap();
    let overlay = streams
        .iter()
        .find(|s| s.repo_id == "repo-a" && s.scope_id == "overlay/1")
        .expect("repo-a advertises an overlay stream");
    let anchors = streams
        .iter()
        .find(|s| s.repo_id == "repo-a" && s.scope_id == "anchors/1")
        .expect("repo-a advertises an anchors stream");

    // Overlay is bounded; give it more than its budget. Anchors is full-retention; a chain the
    // same length must be left untouched.
    let overlay_len = i64::try_from(OVERLAY_ACCEPTED_RETENTION).unwrap() + 4;
    seed_reclaimable_chain(&conn, overlay, 0xAA, overlay_len);
    seed_reclaimable_chain(&conn, anchors, 0xBB, overlay_len);

    let dropped = table_sync_compact_overdue(&conn, account(), 1, &scope_retention_budget).unwrap();
    assert_eq!(dropped, 4, "overlay compacts to its 256-entry budget; 4 oldest are reclaimed");

    let overlay_remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM table_sync_entries WHERE stream_id = ?1",
            [overlay.stream_id.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(overlay_remaining, i64::try_from(OVERLAY_ACCEPTED_RETENTION).unwrap());

    let anchors_remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM table_sync_entries WHERE stream_id = ?1",
            [anchors.stream_id.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(anchors_remaining, overlay_len, "anchors is full-retention — nothing reclaimed");
}

/// A pending entry below the computed floor would survive locally yet be unreachable to
/// floor-adopting peers; the driver clamps the floor at the lowest pending lamport.
#[test]
fn the_compaction_driver_clamps_the_floor_above_pending_entries() {
    let conn = database();
    let stream = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
    conn.execute(
        "INSERT INTO table_sync_streams(stream_id, repo_id, account_id, incarnation_ref, \
         scope_id) VALUES (?1, 'repo-a', ?2, ?3, 'anchors/1')",
        params![
            stream.stream_id.as_slice(),
            account().to_bytes().as_slice(),
            INCARNATION.as_slice()
        ],
    )
    .unwrap();
    for lamport in 0..5i64 {
        conn.execute(
            "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                     received_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, x'00', 0)",
            params![
                [(lamport + 1) as u8; 32].as_slice(),
                stream.stream_id.as_slice(),
                [2u8; 32].as_slice(),
                lamport
            ],
        )
        .unwrap();
    }
    conn.execute(
        "UPDATE table_sync_entries SET pending_reason = 'unknown_column' WHERE lamport = 1",
        [],
    )
    .unwrap();

    let keep_two = &|_scope: &str| Some(2);
    let compacted = table_sync_compact_overdue(&conn, account(), 1, keep_two).unwrap();
    assert_eq!(compacted, 1, "only the genesis drops: the floor clamps at the pending entry");
    let floor: Option<i64> =
        conn.query_row("SELECT lamport FROM table_sync_retained_floors", [], |row| row.get(0)).ok();
    assert_eq!(floor, Some(1), "the pending entry sits AT the floor and stays offerable");
    let remaining: Vec<i64> = conn
        .prepare("SELECT lamport FROM table_sync_entries ORDER BY lamport")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(remaining, vec![1, 2, 3, 4]);
}

#[test]
fn the_gapped_horizon_sweeps_expired_entries_with_their_descendants() {
    let conn = database();
    let stream = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
    let horizon = store::GAPPED_ENTRY_MAX_AGE_MS;
    let seed = |hash: [u8; 32], prev: [u8; 32], lamport: i64, gapped_at: i64| {
        conn.execute(
            "INSERT INTO table_sync_gapped_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                     gapped_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, x'00', ?6)",
            params![
                hash.as_slice(),
                stream.stream_id.as_slice(),
                [2u8; 32].as_slice(),
                lamport,
                prev.as_slice(),
                gapped_at
            ],
        )
        .unwrap();
    };
    seed([0x11; 32], [0x00; 32], 5, 0); // expired root
    seed([0x12; 32], [0x11; 32], 6, 0); // its expired descendant
    seed([0x14; 32], [0x11; 32], 8, horizon + 500); // a RECENT descendant of the expired root
    seed([0x13; 32], [0x00; 32], 7, horizon + 500); // recent, unrelated, survives

    let swept = table_sync_sweep_expired_gapped(&conn, horizon + 1_000).unwrap();
    assert_eq!(
        swept, 3,
        "the expired root takes its whole parked subtree — a child of an expired root can never \
         promote, however recently it was parked",
    );
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 1, "only the unrelated recent entry survives");

    // Expiry is recovery-safe: the dropped root can be parked again when it really arrives.
    seed([0x11; 32], [0x00; 32], 5, horizon + 1_000);
    let reparked: i64 = conn
        .query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |row| row.get(0))
        .unwrap();
    assert_eq!(reparked, 2, "redelivery re-retains an expired entry");
}

#[test]
fn accepted_snapshot_excludes_gapped_rows() {
    let conn = database();
    let stream = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
    conn.execute(
        "INSERT INTO table_sync_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, signed_bytes, received_at_ms)
             VALUES (?1, ?2, ?3, 0, x'01', 0)",
        params![[1u8; 32].as_slice(), stream.stream_id.as_slice(), [2u8; 32].as_slice()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO table_sync_gapped_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                 gapped_at_ms)
             VALUES (?1, ?2, ?3, 1, ?4, x'02', 0)",
        params![
            [3u8; 32].as_slice(),
            stream.stream_id.as_slice(),
            [2u8; 32].as_slice(),
            [1u8; 32].as_slice(),
        ],
    )
    .unwrap();
    let accepted = accepted_chain_entries(
        &conn,
        stream.stream_id,
        [2; 32],
        TableSyncEntryStart::Beginning,
        10,
    )
    .unwrap();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].signed_bytes, vec![1]);
}

#[test]
fn accepted_device_chains_page_canonically_and_resume_from_exact_frontiers() {
    let conn = database();
    let stream = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
    for (device, lamport, hash) in [(2u8, 1i64, 11u8), (2, 3, 13), (4, 2, 22)] {
        conn.execute(
            "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                     received_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![
                [hash; 32].as_slice(),
                stream.stream_id.as_slice(),
                [device; 32].as_slice(),
                lamport,
                vec![hash],
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO table_sync_chain_tips(
                     stream_id, device_fingerprint, lamport, entry_hash)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(stream_id, device_fingerprint) DO UPDATE SET
                     lamport = excluded.lamport, entry_hash = excluded.entry_hash
                 WHERE excluded.lamport > table_sync_chain_tips.lamport",
            params![
                stream.stream_id.as_slice(),
                [device; 32].as_slice(),
                lamport,
                [hash; 32].as_slice(),
            ],
        )
        .unwrap();
    }

    let first = accepted_chain_page(&conn, stream.stream_id, None, 1).unwrap();
    assert_eq!(first, vec![TableSyncChainHead {
        device_fingerprint: [2; 32],
        lamport: 3,
        entry_hash: [13; 32],
        floor: None,
    }]);
    let second = accepted_chain_page(&conn, stream.stream_id, Some([2; 32]), 1).unwrap();
    assert_eq!(second[0].device_fingerprint, [4; 32]);
    assert_eq!(
        chain_frontier(&conn, stream.stream_id, [2; 32]).unwrap(),
        TableSyncFrontier::Accepted(TableSyncChainCursor { lamport: 3, entry_hash: [13; 32] })
    );

    let page =
        accepted_chain_entries(&conn, stream.stream_id, [2; 32], TableSyncEntryStart::Beginning, 1)
            .unwrap();
    assert_eq!(page[0].signed_bytes, vec![11]);
    let suffix = accepted_chain_entries(
        &conn,
        stream.stream_id,
        [2; 32],
        TableSyncEntryStart::After(TableSyncChainCursor { lamport: 1, entry_hash: [11; 32] }),
        10,
    )
    .unwrap();
    assert_eq!(suffix.iter().map(|entry| entry.signed_bytes[0]).collect::<Vec<_>>(), [13]);
    assert!(
        accepted_chain_entries(
            &conn,
            stream.stream_id,
            [2; 32],
            TableSyncEntryStart::After(TableSyncChainCursor { lamport: 1, entry_hash: [99; 32] }),
            10,
        )
        .unwrap_err()
        .to_string()
        .contains("cursor hash conflicts")
    );
}

#[test]
fn a_witness_without_an_accepted_tail_requests_the_tip_inclusively() {
    let source = database();
    let destination = database();
    let source_stream =
        supported_streams_against(&source, account(), &[REPO_SPEC]).unwrap().remove(0);
    let destination_stream =
        supported_streams_against(&destination, account(), &[REPO_SPEC]).unwrap().remove(0);
    let device = [2; 32];
    let tip = [9; 32];
    source
        .execute(
            "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                     received_at_ms)
                 VALUES (?1, ?2, ?3, 7, x'09', 0)",
            params![tip.as_slice(), source_stream.stream_id.as_slice(), device.as_slice()],
        )
        .unwrap();
    for (conn, stream) in [(&source, &source_stream), (&destination, &destination_stream)] {
        conn.execute(
            "INSERT INTO table_sync_chain_tips(
                     stream_id, device_fingerprint, lamport, entry_hash)
                 VALUES (?1, ?2, 7, ?3)",
            params![stream.stream_id.as_slice(), device.as_slice(), tip.as_slice()],
        )
        .unwrap();
    }

    let frontier = chain_frontier(&destination, destination_stream.stream_id, device).unwrap();
    assert_eq!(
        frontier,
        TableSyncFrontier::Restore(TableSyncChainCursor { lamport: 7, entry_hash: tip })
    );
    let TableSyncFrontier::Restore(witness) = frontier else { unreachable!() };
    let restored = accepted_chain_entries(
        &source,
        source_stream.stream_id,
        device,
        TableSyncEntryStart::At(witness),
        1,
    )
    .unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].cursor.entry_hash, tip);

    let successor_source = database();
    let successor_stream =
        supported_streams_against(&successor_source, account(), &[REPO_SPEC]).unwrap().remove(0);
    successor_source
        .execute(
            "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                     received_at_ms)
                 VALUES (?1, ?2, ?3, 8, ?4, x'0a', 0)",
            params![
                [10u8; 32].as_slice(),
                successor_stream.stream_id.as_slice(),
                device.as_slice(),
                tip.as_slice(),
            ],
        )
        .unwrap();
    let successor = accepted_chain_entries(
        &successor_source,
        successor_stream.stream_id,
        device,
        TableSyncEntryStart::At(witness),
        1,
    )
    .unwrap();
    assert_eq!(successor.len(), 1);
    assert_eq!(successor[0].cursor.entry_hash, [10; 32]);
}

#[test]
fn production_registry_advertises_every_scope_per_current_repo() {
    let conn = database();
    let streams = table_sync_supported_streams(&conn, account()).unwrap();
    // Each current repo advertises one stream per registered scope: anchors/1, overlay/1,
    // distill/1.
    let mut pairs: Vec<(String, String)> =
        streams.iter().map(|stream| (stream.repo_id.clone(), stream.scope_id.clone())).collect();
    pairs.sort();
    assert_eq!(pairs, vec![
        ("repo-a".to_string(), "anchors/1".to_string()),
        ("repo-a".to_string(), "distill/1".to_string()),
        ("repo-a".to_string(), "overlay/1".to_string()),
        ("repo-b".to_string(), "anchors/1".to_string()),
        ("repo-b".to_string(), "distill/1".to_string()),
        ("repo-b".to_string(), "overlay/1".to_string()),
    ]);
}

#[test]
fn production_anchors_create_rebind_and_delete_preserve_local_resolution() {
    let source = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&source, &crate::test_hooks()).unwrap();
    source
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
                 VALUES ('repo-a', 'repo-a', 0)",
            [],
        )
        .unwrap();
    let account = crate::local_account(&source, 0).unwrap();
    crate::ensure_repo_incarnation(&source, "repo-a", 1).unwrap().unwrap();
    source
        .execute(
            "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                     logical_symbol_id, symbol_id, chunk_id, edge_id, anchor_status, created_at_ms)
                 VALUES ('repo-a', 'memory-a', 'path', 'src/lib.rs', 'src/lib.rs', 4, 5,
                         11, 12, 13, 14, 'current', 2)",
            [],
        )
        .unwrap();
    assert_eq!(table_sync_author_pending(&source, account, 2).unwrap(), 1);
    let route = table_sync_supported_streams(&source, account).unwrap().remove(0);

    let destination = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&destination, &crate::test_hooks()).unwrap();
    crate::local_device(&destination, 0).unwrap();
    for entry in crate::account_entries_for_sync(&source, account).unwrap() {
        crate::account_ingest(&destination, &entry.signed_bytes, 0).unwrap();
    }
    let lens_revisions = |conn: &Connection| {
        (
            rag_rat_db::meta::repo_meta(
                conn,
                "repo-a",
                rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
            )
            .unwrap(),
            rag_rat_db::meta::repo_meta(
                conn,
                "repo-a",
                rag_rat_db::meta::LENS_MEMORIES_REVISION_META,
            )
            .unwrap(),
        )
    };
    let before_create = lens_revisions(&destination);
    let sync_all = |destination: &Connection| {
        let heads = table_sync_chain_page_after(&source, account, &route, None, 10).unwrap();
        assert_eq!(heads.len(), 1);
        for entry in table_sync_chain_entries(
            &source,
            account,
            &route,
            heads[0].device_fingerprint,
            TableSyncEntryStart::Beginning,
            20,
        )
        .unwrap()
        {
            table_sync_ingest(
                destination,
                account,
                &route,
                &TableSyncReceived {
                    expected_device: heads[0].device_fingerprint,
                    signed_bytes: &entry.signed_bytes,
                    advertised_floor: None,
                    advertised_tip: None,
                },
                3,
                &Default::default(),
            )
            .unwrap();
        }
    };
    sync_all(&destination);
    assert_eq!(lens_revisions(&destination), before_create);
    let replicated_before_registration: bool = destination
        .query_row(
            "SELECT EXISTS(
                     SELECT 1 FROM repo_memory_bindings
                     WHERE repo_id = 'repo-a' AND memory_id = 'memory-a')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(replicated_before_registration);
    destination
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
                 VALUES ('repo-a', 'repo-a', 0)",
            [],
        )
        .unwrap();
    destination
        .execute(
            "UPDATE repo_memory_bindings
                 SET logical_symbol_id = 71, symbol_id = 72, chunk_id = 73, edge_id = 74,
                     anchor_status = 'relocated', resolved = 1, resolved_path = 'src/here.rs',
                     resolved_start_line = 40, resolved_end_line = 41,
                     resolved_binding_id = 'src/here.rs'
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
            [],
        )
        .unwrap();
    assert_eq!(
        table_sync_author_pending(&destination, account, 3).unwrap(),
        0,
        "checkout-local resolution — the rowids, the status, where relocation landed — does not \
         become a replicated edit",
    );
    source
        .execute(
            "UPDATE repo_memory_bindings SET path = 'src/renamed.rs', start_line = 6
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
            [],
        )
        .unwrap();
    assert_eq!(table_sync_author_pending(&source, account, 3).unwrap(), 1);
    let before_update = lens_revisions(&destination);
    sync_all(&destination);
    let after_update = lens_revisions(&destination);
    assert_ne!(after_update.0, before_update.0);
    assert_ne!(after_update.1, before_update.1);
    // A winning upsert that CHANGES the authored row resets the checkout-local resolution
    // that described the old one — where relocation had landed (`registry::reset_on_upsert`);
    // the handles and the status stay.
    let row: (String, i64, Option<i64>, Option<i64>, i64, String, Option<String>) = destination
        .query_row(
            "SELECT path, logical_symbol_id, symbol_id, chunk_id, edge_id, anchor_status,
                        COALESCE(resolved_path, resolved_binding_id)
                 FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        row,
        ("src/renamed.rs".into(), 71, Some(72), Some(73), 74, "relocated".into(), None)
    );

    source
        .execute_batch(
            "DELETE FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a';
                 INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                     anchor_status, created_at_ms)
                 VALUES ('repo-a', 'memory-a', 'path', 'src/moved.rs', 'src/moved.rs', 8, 9,
                         'current', 4)",
        )
        .unwrap();
    assert_eq!(table_sync_author_pending(&source, account, 4).unwrap(), 2);
    let before_rebind = lens_revisions(&destination);
    sync_all(&destination);
    let after_rebind = lens_revisions(&destination);
    assert_ne!(after_rebind.0, before_rebind.0);
    assert_ne!(after_rebind.1, before_rebind.1);
    let rebound: (String, String) = destination
        .query_row(
            "SELECT path, anchor_status FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(rebound, ("src/moved.rs".into(), "unverified".into()));

    source
        .execute(
            "DELETE FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
            [],
        )
        .unwrap();
    assert_eq!(table_sync_author_pending(&source, account, 5).unwrap(), 1);
    let before_delete = lens_revisions(&destination);
    sync_all(&destination);
    let after_delete = lens_revisions(&destination);
    assert_ne!(after_delete.0, before_delete.0);
    assert_ne!(after_delete.1, before_delete.1);
    let exists: bool = destination
        .query_row(
            "SELECT EXISTS(
                     SELECT 1 FROM repo_memory_bindings
                     WHERE repo_id = 'repo-a' AND memory_id = 'memory-a')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!exists);
    assert_eq!(table_sync_author_pending(&source, account, 6).unwrap(), 0);
}

#[test]
fn accepted_transfer_is_idempotent_and_obeys_the_current_roster_write_gate() {
    let mut source = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&source, &crate::test_hooks()).unwrap();
    let account = crate::local_account(&source, 0).unwrap();
    add_repo_state(&source, account);
    source
        .execute("INSERT INTO t_transport(repo_id, id, title) VALUES ('repo-a', 'r1', 'one')", [])
        .unwrap();
    let local = crate::load_local_device(&source).unwrap().unwrap();
    let author = local.public().fingerprint().to_bytes();
    let tx = source.transaction().unwrap();
    let ctx = SyncCtx {
        repo_id: "repo-a",
        account_id: account,
        incarnation_ref: INCARNATION,
        device: &local,
        registry: &[REPO_SPEC],
        now_ms: 0,
        local_writer: Default::default(),
    };
    let authored = engine::produce_and_author(&tx, &ctx).unwrap();
    tx.commit().unwrap();
    assert_eq!(authored.len(), 1);
    let route = supported_streams_against(&source, account, &[REPO_SPEC]).unwrap().remove(0);

    let restore = |remove_writer: bool| {
        let dest = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&dest, &crate::test_hooks()).unwrap();
        crate::local_device(&dest, 0).unwrap();
        for entry in crate::account_entries_for_sync(&source, account).unwrap() {
            crate::account_ingest(&dest, &entry.signed_bytes, 0).unwrap();
        }
        add_repo_state(&dest, account);
        if remove_writer {
            dest.execute(
                "UPDATE account_roster_history SET closed_at = 1 WHERE account_id = ?1",
                [account.to_bytes().as_slice()],
            )
            .unwrap();
        }
        dest
    };

    let destination = restore(false);
    assert_eq!(
        ingest_against(
            &destination,
            &IngestRoute { account_id: account, stream: &route, registry: &[REPO_SPEC] },
            crate::op::DeviceFingerprint::from_bytes([0; 32]),
            &authored[0],
            1,
            None,
            &Default::default(),
        )
        .unwrap(),
        TableSyncIngestOutcome::NoChange,
    );
    assert_eq!(
        ingest_against(
            &destination,
            &IngestRoute { account_id: account, stream: &route, registry: &[REPO_SPEC] },
            crate::op::DeviceFingerprint::from_bytes(author),
            &authored[0],
            1,
            None,
            &Default::default(),
        )
        .unwrap(),
        TableSyncIngestOutcome::Stored,
    );
    assert_eq!(
        ingest_against(
            &destination,
            &IngestRoute { account_id: account, stream: &route, registry: &[REPO_SPEC] },
            crate::op::DeviceFingerprint::from_bytes(author),
            &authored[0],
            2,
            None,
            &Default::default(),
        )
        .unwrap(),
        TableSyncIngestOutcome::NoChange,
    );
    let title: String = destination
        .query_row(
            "SELECT title FROM t_transport WHERE repo_id = 'repo-a' AND id = 'r1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(title, "one");

    let removed = restore(true);
    assert_eq!(
        ingest_against(
            &removed,
            &IngestRoute { account_id: account, stream: &route, registry: &[REPO_SPEC] },
            crate::op::DeviceFingerprint::from_bytes(author),
            &authored[0],
            1,
            None,
            &Default::default(),
        )
        .unwrap(),
        TableSyncIngestOutcome::NoChange,
    );
    let accepted: i64 =
        removed.query_row("SELECT COUNT(*) FROM table_sync_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(accepted, 0, "a removed writer reaches no accepted table history");
}

/// The synthetic repo table plus a current incarnation for `repo-a`.
fn add_repo_state(conn: &Connection, account: AccountId) {
    conn.execute_batch(
        "CREATE TABLE t_transport(
                 repo_id TEXT NOT NULL,
                 id TEXT NOT NULL,
                 title TEXT NOT NULL,
                 PRIMARY KEY(repo_id, id)
             ) STRICT;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO account_repo_incarnation_current(
                 account_id, repository_id, incarnation_ref
             ) VALUES (?1, 'repo-a', ?2)",
        params![account.to_bytes().as_slice(), INCARNATION.as_slice()],
    )
    .unwrap();
}

/// A store whose local device owns a real account, so it is a roster-effective writer and a
/// peer can verify its entries.
fn writer_store() -> (Connection, AccountId) {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
    let account = crate::local_account(&conn, 0).unwrap();
    add_repo_state(&conn, account);
    (conn, account)
}

/// A fresh device that has folded `source`'s account log but holds no table history, and is
/// not a writer of that account.
fn peer_of(source: &Connection, account: AccountId) -> Connection {
    let peer = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&peer, &crate::test_hooks()).unwrap();
    crate::local_device(&peer, 0).unwrap();
    for entry in crate::account_entries_for_sync(source, account).unwrap() {
        crate::account_ingest(&peer, &entry.signed_bytes, 0).unwrap();
    }
    add_repo_state(&peer, account);
    peer
}

fn write(conn: &Connection, id: &str, title: &str) {
    conn.execute(
        "INSERT OR REPLACE INTO t_transport(repo_id, id, title) VALUES ('repo-a', ?1, ?2)",
        params![id, title],
    )
    .unwrap();
}

fn author(conn: &Connection, account: AccountId) {
    let local = crate::load_local_device(conn).unwrap().unwrap();
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
    let ctx = SyncCtx {
        repo_id: "repo-a",
        account_id: account,
        incarnation_ref: INCARNATION,
        device: &local,
        registry: &[REPO_SPEC],
        now_ms: 0,
        local_writer: Default::default(),
    };
    engine::produce_and_author(&tx, &ctx).unwrap();
    tx.commit().unwrap();
}

/// `write` then `author`, once per title: successive writes to one row.
fn rewrite(conn: &Connection, account: AccountId, id: &str, times: usize) {
    for i in 0..times {
        write(conn, id, &format!("v{i}"));
        author(conn, account);
    }
}

fn compact(conn: &Connection, account: AccountId, keep: u64) -> usize {
    compact_overdue_against(conn, account, 9, &|_| Some(keep), &[REPO_SPEC]).unwrap()
}

fn chain_lamports(conn: &Connection) -> Vec<i64> {
    conn.prepare("SELECT lamport FROM table_sync_entries ORDER BY lamport")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn live_rows(conn: &Connection) -> Vec<(String, String)> {
    conn.prepare("SELECT id, title FROM t_transport ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Live rows and orphan tombstones whose carrying entry is gone: what a peer folding only the
/// retained log can never learn.
/// Pins whose carrying entry is gone from the log: a live clock's entry, or a statement's —
/// a tombstone is delivered by whichever entry its chain last stated it at, never by its
/// identity's entry once that has been restated.
fn stranded_rows(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM (
                 SELECT stream_id, device_fingerprint, lamport FROM sync_row_clocks
                 UNION ALL
                 SELECT s.stream_id, s.device_fingerprint, s.lamport FROM \
         sync_tombstone_statements s
                 WHERE NOT EXISTS (
                     SELECT 1 FROM sync_row_clocks c
                     WHERE c.stream_id = s.stream_id AND c.table_name = s.table_name
                       AND c.row_pk = s.row_pk
                 )
             ) p
             WHERE NOT EXISTS (
                 SELECT 1 FROM table_sync_entries e
                 WHERE e.stream_id = p.stream_id AND e.lamport = p.lamport
                   AND lower(hex(e.device_fingerprint)) = p.device_fingerprint
             )",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

/// Offer every chain of `source` to `peer` as a session planning a fresh or below-floor peer
/// does: from the recorded floor (advertised on the floor entry), else from the beginning.
/// Re-offers of entries the peer holds read idempotent.
fn sync_chains(source: &Connection, peer: &Connection, account: AccountId) {
    let route = supported_streams_against(source, account, &[REPO_SPEC]).unwrap().remove(0);
    for head in accepted_chain_page(source, route.stream_id, None, 16).unwrap() {
        let start = head.floor.map_or(TableSyncEntryStart::Beginning, TableSyncEntryStart::At);
        for entry in
            accepted_chain_entries(source, route.stream_id, head.device_fingerprint, start, 1024)
                .unwrap()
        {
            let floor = head.floor.filter(|floor| floor.lamport == entry.cursor.lamport);
            ingest_received_against(
                peer,
                &IngestRoute { account_id: account, stream: &route, registry: &[REPO_SPEC] },
                &TableSyncReceived {
                    expected_device: head.device_fingerprint,
                    signed_bytes: &entry.signed_bytes,
                    advertised_floor: floor,
                    advertised_tip: Some(TableSyncChainCursor {
                        lamport: head.lamport,
                        entry_hash: head.entry_hash,
                    }),
                },
                1,
                &Default::default(),
            )
            .unwrap();
        }
    }
}

/// The #1277 probe: more live rows than the budget. Every entry carries a row, so nothing is
/// reclaimable and nothing is worth re-authoring — the chain stays honestly over budget.
#[test]
fn live_rows_past_the_budget_are_kept_and_nothing_is_reauthored() {
    let (a, account) = writer_store();
    for i in 0..5 {
        write(&a, &format!("r{i}"), "v");
    }
    author(&a, account);
    assert_eq!(compact(&a, account, 4), 0);
    assert_eq!(compact(&a, account, 4), 0, "a second pass authors nothing either");
    assert_eq!(chain_lamports(&a), vec![0, 1, 2, 3, 4]);
    let floors: i64 = a
        .query_row("SELECT COUNT(*) FROM table_sync_retained_floors", [], |row| row.get(0))
        .unwrap();
    assert_eq!(floors, 0, "no floor is recorded for a chain that cannot shrink");

    let peer = peer_of(&a, account);
    sync_chains(&a, &peer, account);
    assert_eq!(live_rows(&peer), live_rows(&a));
}

/// The writer's own chain moves its oldest pins to the tail when that pays — here two entries
/// authored free eight — and settles once the chain is only its pins.
#[test]
fn the_writer_reauthors_its_oldest_pins_when_that_frees_twice_as_much() {
    let (a, account) = writer_store();
    write(&a, "a", "a");
    write(&a, "b", "b");
    author(&a, account); // a@0, b@1
    rewrite(&a, account, "c", 8); // c@2..9, only c@9 live

    assert_eq!(compact(&a, account, 2), 8, "moving a and b clears the way to the budget floor");
    assert_eq!(chain_lamports(&a), vec![8, 9, 10, 11]);
    assert_eq!(stranded_rows(&a), 0);

    assert_eq!(compact(&a, account, 2), 2, "moving c frees c@8 and c@9");
    assert_eq!(compact(&a, account, 2), 0, "moving a to free one entry does not pay");
    assert_eq!(chain_lamports(&a), vec![10, 11, 12], "three live rows, three entries");
    assert_eq!(stranded_rows(&a), 0);

    let peer = peer_of(&a, account);
    sync_chains(&a, &peer, account);
    assert_eq!(live_rows(&peer), live_rows(&a), "a fresh peer folds every live row");
}

/// Moving pins is worth it only when every entry authored reclaims at least one more: two
/// moved to free three is refused, and the floor clamps at the oldest pin instead.
#[test]
fn moving_pins_that_free_less_than_twice_their_cost_does_not_pay() {
    let (a, account) = writer_store();
    write(&a, "a", "a");
    write(&a, "b", "b");
    author(&a, account); // a@0, b@1
    rewrite(&a, account, "c", 2); // c@2, c@3

    assert_eq!(compact(&a, account, 1), 0);
    assert_eq!(chain_lamports(&a), vec![0, 1, 2, 3], "nothing authored or dropped");
}

/// A peer compacted before the pin rule may hold a foreign chain whose pins sit below its
/// floor, with their entries gone. Nothing can carry those forward, and they must not wedge
/// compaction of the region above the floor.
#[test]
fn a_foreign_chain_stranded_below_an_earlier_floor_still_compacts() {
    let (a, account) = writer_store();
    write(&a, "a", "a");
    author(&a, account); // a@0
    rewrite(&a, account, "c", 8); // c@1..8, only c@8 live
    let peer = peer_of(&a, account);
    sync_chains(&a, &peer, account);
    let route = supported_streams_against(&peer, account, &[REPO_SPEC]).unwrap().remove(0);
    let writer = crate::load_local_device(&a).unwrap().unwrap().fingerprint();
    {
        let tx = Transaction::new_unchecked(&peer, TransactionBehavior::Immediate).unwrap();
        let floor_hash: Vec<u8> = tx
            .query_row("SELECT entry_hash FROM table_sync_entries WHERE lamport = 4", [], |row| {
                row.get(0)
            })
            .unwrap();
        tx.execute("DELETE FROM table_sync_entries WHERE lamport < 4", []).unwrap();
        retention::record_adopted_floor(
            &tx,
            StreamId::from_bytes(route.stream_id),
            writer,
            4,
            EntryHash::try_from_sql(floor_hash).unwrap(),
            0,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(stranded_rows(&peer), 1, "a@0 is gone below the floor");

    assert_eq!(compact(&peer, account, 2), 3, "entries 4, 5 and 6 above the floor drop");
    assert_eq!(chain_lamports(&peer), vec![7, 8]);
}

/// More cold pins than one pass may carry, under a churning row: the paying move spans several
/// passes instead of stalling behind the cap, and the chain reaches its budget.
#[test]
fn a_run_of_pins_longer_than_the_pass_cap_still_reaches_the_budget() {
    let (a, account) = writer_store();
    for i in 0..=COMPACTION_REAUTHOR_MAX {
        write(&a, &format!("cold{i:03}"), "v");
    }
    author(&a, account); // 65 cold rows at 0..=64
    rewrite(&a, account, "hot", 200); // 65..=264, only the last live

    let mut passes = Vec::new();
    loop {
        let reclaimed = compact(&a, account, 100);
        if reclaimed == 0 {
            break;
        }
        passes.push(reclaimed);
        assert!(passes.len() < 8, "compaction settles: {passes:?}");
    }
    assert_eq!(passes, vec![64, 165, 1], "carry 64, then the last cold pin, then the budget");
    assert_eq!(chain_lamports(&a).len(), 100, "the chain is at its budget");
    assert_eq!(stranded_rows(&a), 0);

    let peer = peer_of(&a, account);
    sync_chains(&a, &peer, account);
    assert_eq!(live_rows(&peer), live_rows(&a));
}

/// An accepted entry this binary cannot apply yet may be a newer write to a pinned row, on any
/// chain. Re-authoring the pin above it would overwrite that write on up-to-date peers, so the
/// pin stays until the entry replays.
#[test]
fn a_parked_newer_write_holds_the_pins_below_it() {
    let (a, account) = writer_store();
    write(&a, "a", "a");
    write(&a, "b", "b");
    author(&a, account); // a@0, b@1
    rewrite(&a, account, "c", 8); // c@2..9
    let route = supported_streams_against(&a, account, &[REPO_SPEC]).unwrap().remove(0);
    a.execute(
        "INSERT INTO table_sync_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, signed_bytes, received_at_ms,
                 pending_reason
             ) VALUES (x'77', ?1, ?2, 5, x'00', 0, 'newer_spec_version')",
        params![route.stream_id.as_slice(), [2u8; 32].as_slice()],
    )
    .unwrap();

    assert_eq!(compact(&a, account, 2), 0, "a and b stay below the parked write");
    assert_eq!(chain_lamports(&a).len(), 11, "ten local entries and the parked one");

    a.execute("UPDATE table_sync_entries SET pending_reason = NULL WHERE entry_hash = x'77'", [])
        .unwrap();
    assert_eq!(compact(&a, account, 2), 8, "once it replays, the pins move");
}

/// A retained entry at the transport limit grows when re-signed under a predecessor hash. The
/// pin it carries cannot move, and must clamp the floor rather than fail every session.
#[test]
fn a_pin_too_large_to_resign_stays_pinned() {
    let (a, account) = writer_store();
    let route = supported_streams_against(&a, account, &[REPO_SPEC]).unwrap().remove(0);
    let local = crate::load_local_device(&a).unwrap().unwrap();
    let genesis_len = |title: &str| {
        let op = RowOp::Upsert {
            table: "t_transport".to_string(),
            spec_version: 1,
            pk: vec![TypedValue::Text("repo-a".to_string()), TypedValue::Text("big".to_string())],
            cells: vec![Cell {
                column: "title".to_string(),
                value: TypedValue::Text(title.to_string()),
            }],
        };
        crate::entry::sign_entry_from_op_bytes(
            local.secret(),
            StreamId::from_bytes(route.stream_id),
            None,
            0,
            super::super::row_op::encode(&op),
        )
        .signed_bytes
        .len()
    };
    let probe = super::super::TABLE_SYNC_ENTRY_MAX_BYTES - 256;
    let title = "x"
        .repeat(probe + super::super::TABLE_SYNC_ENTRY_MAX_BYTES - genesis_len(&"x".repeat(probe)));
    write(&a, "big", &title);
    author(&a, account); // big@0, exactly at the limit
    let stored: i64 = a
        .query_row(
            "SELECT length(signed_bytes) FROM table_sync_entries WHERE lamport = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, super::super::TABLE_SYNC_ENTRY_MAX_BYTES as i64);
    rewrite(&a, account, "c", 8); // c@1..8

    assert_eq!(compact(&a, account, 2), 0, "big cannot move, so it clamps the floor");
    assert_eq!(chain_lamports(&a), (0..9).collect::<Vec<_>>(), "nothing authored or dropped");
}

/// Only a chain's writer can carry its rows forward: a peer compacting a foreign chain drops
/// the superseded prefix below the oldest pin and authors nothing.
#[test]
fn a_foreign_chain_clamps_at_its_oldest_pin_and_is_never_reauthored() {
    let (a, account) = writer_store();
    rewrite(&a, account, "c", 8); // c@0..7, only c@7 live
    write(&a, "a", "a");
    write(&a, "b", "b");
    author(&a, account); // a@8, b@9
    let peer = peer_of(&a, account);
    sync_chains(&a, &peer, account);

    assert_eq!(compact(&peer, account, 2), 7, "the prefix below c's winner drops");
    assert_eq!(chain_lamports(&peer), vec![7, 8, 9], "and the peer authored nothing");
    assert_eq!(stranded_rows(&peer), 0);

    assert_eq!(compact(&a, account, 2), 8, "the writer moves c instead and reaches the budget");
    assert_eq!(chain_lamports(&a), vec![8, 9, 10]);
}

/// A pin whose physical row disagrees with its merge state is the producer's to settle, not
/// compaction's: re-authoring stops there and the floor clamps at it.
#[test]
fn a_pin_that_cannot_be_carried_halts_reauthoring_and_clamps_there() {
    let (a, account) = writer_store();
    write(&a, "a", "a");
    write(&a, "b", "b");
    author(&a, account);
    rewrite(&a, account, "c", 8);
    // Deleted but not yet authored: a's clock still names this chain, the row is gone.
    a.execute("DELETE FROM t_transport WHERE id = 'a'", []).unwrap();

    assert_eq!(compact(&a, account, 2), 0);
    assert_eq!(chain_lamports(&a), (0..10).collect::<Vec<_>>(), "nothing authored or dropped");
}

/// The re-root case the tombstone pin exists for: a peer offline since before a delete
/// rejoins below the writer's floor, and the delete still reaches it.
#[test]
fn a_peer_rejoining_below_the_floor_still_receives_a_delete() {
    let (a, account) = writer_store();
    for id in ["r0", "r1", "r2"] {
        write(&a, id, id);
    }
    author(&a, account); // r0@0, r1@1, r2@2
    let rejoining = peer_of(&a, account);
    sync_chains(&a, &rejoining, account);

    a.execute("DELETE FROM t_transport WHERE id = 'r0'", []).unwrap();
    author(&a, account); // remove r0 @3
    rewrite(&a, account, "h", 8); // h@4..11, only h@11 live
    assert_eq!(compact(&a, account, 2), 10);
    assert_eq!(chain_lamports(&a), vec![10, 11, 12, 13, 14], "r1, r2 and the delete moved");
    assert_eq!(stranded_rows(&a), 0);

    sync_chains(&a, &rejoining, account);
    assert_eq!(live_rows(&rejoining), live_rows(&a), "the rejoining peer dropped r0");
    let fresh = peer_of(&a, account);
    sync_chains(&a, &fresh, account);
    assert_eq!(live_rows(&fresh), live_rows(&a));
}

/// A store compacted before the pin rule dropped entries that still carried live rows; the
/// driver re-authors those once, whatever the budget, so fresh peers receive them again.
#[test]
fn rows_stranded_below_an_earlier_floor_are_reauthored_once() {
    let (a, account) = writer_store();
    write(&a, "a", "a");
    write(&a, "b", "b");
    author(&a, account);
    rewrite(&a, account, "c", 8);
    let route = supported_streams_against(&a, account, &[REPO_SPEC]).unwrap().remove(0);
    let local = crate::load_local_device(&a).unwrap().unwrap().fingerprint();
    {
        // What the earlier compaction to floor 8 left behind.
        let tx = Transaction::new_unchecked(&a, TransactionBehavior::Immediate).unwrap();
        let floor_hash: Vec<u8> = tx
            .query_row("SELECT entry_hash FROM table_sync_entries WHERE lamport = 8", [], |row| {
                row.get(0)
            })
            .unwrap();
        tx.execute("DELETE FROM table_sync_entries WHERE lamport < 8", []).unwrap();
        retention::record_adopted_floor(
            &tx,
            StreamId::from_bytes(route.stream_id),
            local,
            8,
            EntryHash::try_from_sql(floor_hash).unwrap(),
            0,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(stranded_rows(&a), 2, "a and b are unreachable from the retained log");
    // Deleted but not yet authored: a cannot be carried until the producer settles it, and
    // must not hold b back.
    a.execute("DELETE FROM t_transport WHERE id = 'a'", []).unwrap();

    assert_eq!(compact(&a, account, 100), 0, "the chain is within budget");
    assert_eq!(stranded_rows(&a), 1, "b moved to the tail; a waits for its delete");
    author(&a, account);
    assert_eq!(stranded_rows(&a), 0, "the producer's delete carries a");
    assert_eq!(chain_lamports(&a), vec![8, 9, 10, 11]);
    assert_eq!(compact(&a, account, 100), 0);
    assert_eq!(chain_lamports(&a), vec![8, 9, 10, 11], "the repair runs once");

    let peer = peer_of(&a, account);
    sync_chains(&a, &peer, account);
    assert_eq!(live_rows(&peer), live_rows(&a));
}

/// A writer's own orphan tombstones are restated into one entry at the tail before the floor
/// moves, so the entries that first stated them are reclaimed and a fresh peer folding only
/// the retained suffix still receives every delete. A chain that is all statements packed at
/// the tail is its irreducible footprint: a further pass authors nothing.
#[test]
fn compaction_restates_own_orphan_tombstones_before_taking_the_floor() {
    let (a, account) = writer_store();
    for id in ["r0", "r1", "r2", "r3", "r4"] {
        write(&a, id, id);
    }
    author(&a, account); // 0..=4
    a.execute("DELETE FROM t_transport", []).unwrap();
    author(&a, account); // removes 5..=9
    rewrite(&a, account, "h", 8); // 10..=17, only h@17 live

    assert_eq!(compact(&a, account, 2), 16, "everything below the target is reclaimed");
    assert_eq!(chain_lamports(&a), vec![16, 17, 18], "one restatement carries five deletes");
    assert_eq!(stranded_rows(&a), 0);
    let restated: Vec<u8> = a
        .query_row("SELECT signed_bytes FROM table_sync_entries WHERE lamport = 18", [], |r| {
            r.get(0)
        })
        .unwrap();
    let signed = crate::entry::decode_signed(&restated).unwrap();
    let Ok(DecodedRowOp::Known(RowOp::Restate { deletes, .. })) =
        crate::table_sync::row_op::decode(&signed.entry.op_bytes)
    else {
        panic!("the tail entry is a restatement");
    };
    assert_eq!(deletes.len(), 5);
    assert!(deletes.iter().all(|d| (5..=9).contains(&d.lamport)), "at their identities");

    let fresh = peer_of(&a, account);
    sync_chains(&a, &fresh, account);
    assert_eq!(live_rows(&fresh), live_rows(&a), "no phantom row on a fresh peer");
    let tombstones: i64 =
        fresh.query_row("SELECT COUNT(*) FROM sync_row_tombstones", [], |row| row.get(0)).unwrap();
    assert_eq!(tombstones, 5, "every delete reached it");

    // Steady state: the superseded h@16 goes, then nothing pays and nothing is authored.
    assert_eq!(compact(&a, account, 2), 1);
    assert_eq!(chain_lamports(&a), vec![17, 18]);
    assert_eq!(compact(&a, account, 1), 0, "a chain that is all statements authors nothing");
    assert_eq!(chain_lamports(&a), vec![17, 18]);
}

/// The mandatory phase: statements a store compacted under an older rule left below its
/// floor are restated once, whatever the budget, so fresh peers receive the deletes again.
#[test]
fn mandatory_below_floor_repair_runs_before_the_budget_loop_for_both_pin_kinds() {
    let (a, account) = writer_store();
    write(&a, "a", "a");
    write(&a, "b", "b");
    author(&a, account); // 0, 1
    a.execute("DELETE FROM t_transport WHERE id = 'b'", []).unwrap();
    author(&a, account); // remove b @2
    rewrite(&a, account, "c", 8); // 3..=10
    let route = supported_streams_against(&a, account, &[REPO_SPEC]).unwrap().remove(0);
    let local = crate::load_local_device(&a).unwrap().unwrap().fingerprint();
    {
        // What an older compaction to floor 8 left behind: a live row and a statement gone.
        let tx = Transaction::new_unchecked(&a, TransactionBehavior::Immediate).unwrap();
        let floor_hash: Vec<u8> = tx
            .query_row("SELECT entry_hash FROM table_sync_entries WHERE lamport = 8", [], |row| {
                row.get(0)
            })
            .unwrap();
        tx.execute("DELETE FROM table_sync_entries WHERE lamport < 8", []).unwrap();
        retention::record_adopted_floor(
            &tx,
            StreamId::from_bytes(route.stream_id),
            local,
            8,
            EntryHash::try_from_sql(floor_hash).unwrap(),
            0,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(stranded_rows(&a), 2, "a's row and b's delete are unreachable");
    assert_eq!(compact(&a, account, 100), 0, "within budget, so only the repair runs");
    assert_eq!(stranded_rows(&a), 0);
    assert_eq!(chain_lamports(&a), vec![8, 9, 10, 11, 12], "an upsert and a restatement");
    let peer = peer_of(&a, account);
    sync_chains(&a, &peer, account);
    assert_eq!(live_rows(&peer), live_rows(&a));
}

/// A chain's statements move only when its own writer restates: a foreign chain clamps at
/// its oldest statement, whatever its budget.
#[test]
fn a_foreign_chain_clamps_at_its_orphan_tombstones_until_its_writer_restates() {
    let (a, account) = writer_store();
    write(&a, "r0", "r0");
    author(&a, account); // 0
    a.execute("DELETE FROM t_transport WHERE id = 'r0'", []).unwrap();
    author(&a, account); // remove @1
    rewrite(&a, account, "h", 6); // 2..=7
    let peer = peer_of(&a, account);
    sync_chains(&a, &peer, account);
    assert_eq!(compact(&peer, account, 2), 1, "only the superseded entry below the statement");
    let held: Vec<i64> = peer
        .prepare("SELECT lamport FROM table_sync_entries ORDER BY lamport")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(held, (1..=7).collect::<Vec<_>>(), "clamped at the statement");
    // The writer restates and compacts; the peer then reclaims past the old statement.
    assert_eq!(compact(&a, account, 2), 6);
    sync_chains(&a, &peer, account);
    assert!(compact(&peer, account, 2) >= 5, "the restatement freed the foreign prefix");
}

/// The economics charge stated deletes as the batches they pack into. Three rows written by
/// a peer and deleted here leave this chain with three statements and nothing else below
/// the target: per-row costing would refuse the move (3 freed < 2 × 3), batch costing moves
/// all three (3 freed ≥ 2 × 1) and the chain compacts to its tail.
#[test]
fn restating_obeys_the_twice_the_cost_rule_per_batch() {
    let (a, account) = writer_store();
    let route = supported_streams_against(&a, account, &[REPO_SPEC]).unwrap().remove(0);
    // Rows that arrived from a peer: physically present, published, clocked under a foreign
    // device — so this chain never wrote them, and their deletes are its first entries. The
    // deletes take lamport 0.. (the stream clock counts entries, and there are none), so
    // they tie the foreign clock on lamport and win on fingerprint: all-`ff` sorts above any
    // local device.
    for id in ["r0", "r1", "r2"] {
        write(&a, id, id);
        let pk = [TypedValue::Text("repo-a".to_string()), TypedValue::Text(id.to_string())];
        let row_pk = crate::table_sync::row_op::row_pk_string(&pk);
        let tx = Transaction::new_unchecked(&a, TransactionBehavior::Immediate).unwrap();
        let hash =
            crate::table_sync::apply::synced_row_hash(&tx, &REPO_SPEC, &pk).unwrap().unwrap();
        let stream = StreamId::from_bytes(route.stream_id);
        crate::table_sync::apply::record_published(
            &tx,
            &crate::table_sync::apply::RowKey {
                stream,
                repo_id: "repo-a",
                table: REPO_SPEC.name,
                row_pk: &row_pk,
            },
            &hash,
            REPO_SPEC.spec_version,
        )
        .unwrap();
        tx.execute(
            "INSERT INTO sync_row_clocks(
                     stream_id, repo_id, table_name, row_pk, lamport, device_fingerprint)
                 VALUES (?1, 'repo-a', ?2, ?3, 0, ?4)",
            params![
                route.stream_id.as_slice(),
                REPO_SPEC.name,
                row_pk,
                crate::op::DeviceFingerprint::from_bytes([0xff; 32]).to_string()
            ],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(chain_lamports(&a), Vec::<i64>::new(), "nothing of this chain's yet");
    a.execute("DELETE FROM t_transport", []).unwrap();
    author(&a, account); // removes 0..=2
    write(&a, "live", "live");
    author(&a, account); // 3
    assert_eq!(compact(&a, account, 1), 3, "the three statements moved into one entry");
    assert_eq!(chain_lamports(&a), vec![3, 4]);
    assert_eq!(stranded_rows(&a), 0);
    let fresh = peer_of(&a, account);
    sync_chains(&a, &fresh, account);
    assert_eq!(live_rows(&fresh), vec![("live".to_string(), "live".to_string())]);
}

#[test]
fn interrupted_floor_delivery_needs_the_promised_suffix_before_serving_a_fresh_peer() {
    let (source, account) = writer_store();
    write(&source, "deleted", "old");
    author(&source, account);
    let intermediary = peer_of(&source, account);
    let fresh = peer_of(&source, account);
    // A fresh peer can learn a stale upsert from another source before it sees the floor.
    sync_chains(&source, &fresh, account);
    source.execute("DELETE FROM t_transport WHERE id = 'deleted'", []).unwrap();
    author(&source, account);
    rewrite(&source, account, "hot", 8);
    assert!(compact(&source, account, 3) > 0);
    let route = supported_streams_against(&source, account, &[REPO_SPEC]).unwrap().remove(0);
    let offered = accepted_chain_page(&source, route.stream_id, None, 16).unwrap().remove(0);
    let floor = offered.floor.unwrap();
    assert!(floor.lamport < offered.lamport, "the delete is carried later in the suffix");
    let first = accepted_chain_entries(
        &source,
        route.stream_id,
        offered.device_fingerprint,
        TableSyncEntryStart::At(floor),
        1,
    )
    .unwrap()
    .remove(0);
    ingest_received_against(
        &intermediary,
        &IngestRoute { account_id: account, stream: &route, registry: &[REPO_SPEC] },
        &TableSyncReceived {
            expected_device: offered.device_fingerprint,
            signed_bytes: &first.signed_bytes,
            advertised_floor: Some(floor),
            advertised_tip: None,
        },
        1,
        &Default::default(),
    )
    .unwrap();
    assert!(
        chain_lamports(&intermediary).is_empty(),
        "a floor without its inventory tip cannot be adopted"
    );
    ingest_received_against(
        &intermediary,
        &IngestRoute { account_id: account, stream: &route, registry: &[REPO_SPEC] },
        &TableSyncReceived {
            expected_device: offered.device_fingerprint,
            signed_bytes: &first.signed_bytes,
            advertised_floor: Some(floor),
            advertised_tip: Some(TableSyncChainCursor {
                lamport: offered.lamport,
                entry_hash: offered.entry_hash,
            }),
        },
        1,
        &Default::default(),
    )
    .unwrap();
    // The connection ends here, before the restatement reaches the intermediary.
    let relayed = accepted_chain_page(&intermediary, route.stream_id, None, 16).unwrap().remove(0);
    assert_eq!(relayed.floor, Some(floor));
    assert_eq!(relayed.lamport, offered.lamport);
    assert_eq!(relayed.entry_hash, offered.entry_hash);
    let stream_id = StreamId::from_bytes(route.stream_id);
    assert!(coverage::stream_pending(&intermediary, stream_id).unwrap());
    assert_eq!(compact(&intermediary, account, 1), 0);
    write(&intermediary, "unsent", "local");
    author(&intermediary, account);
    assert_eq!(
        chain_lamports(&intermediary),
        [floor.lamport as i64],
        "incomplete stream authors nothing"
    );
    intermediary.execute("DELETE FROM t_transport WHERE id = 'unsent'", []).unwrap();
    let suffix = accepted_chain_entries(
        &source,
        route.stream_id,
        offered.device_fingerprint,
        TableSyncEntryStart::At(floor),
        16,
    )
    .unwrap();
    let middle = suffix[1].cursor;
    for start in [TableSyncEntryStart::After(middle), TableSyncEntryStart::At(middle)] {
        assert!(
            accepted_chain_entries(
                &intermediary,
                route.stream_id,
                offered.device_fingerprint,
                start,
                16
            )
            .unwrap()
            .is_empty()
        );
    }
    let wrong = TableSyncChainCursor { lamport: floor.lamport, entry_hash: [9; 32] };
    assert!(
        accepted_chain_entries(
            &intermediary,
            route.stream_id,
            offered.device_fingerprint,
            TableSyncEntryStart::After(wrong),
            16
        )
        .is_err()
    );
    let tip = suffix.last().unwrap();
    // Another source offers the target itself as a new root. It must stay gapped: a higher
    // floor cannot bypass the original, still-missing predecessor and erase the obligation.
    ingest_received_against(
        &intermediary,
        &IngestRoute { account_id: account, stream: &route, registry: &[REPO_SPEC] },
        &TableSyncReceived {
            expected_device: offered.device_fingerprint,
            signed_bytes: &tip.signed_bytes,
            advertised_floor: Some(tip.cursor),
            advertised_tip: Some(tip.cursor),
        },
        2,
        &Default::default(),
    )
    .unwrap();
    assert!(coverage::stream_pending(&intermediary, stream_id).unwrap());
    assert_eq!(chain_lamports(&intermediary), [floor.lamport as i64]);
    let restart = tempfile::tempdir().unwrap();
    let database = restart.path().join("intermediary.db");
    intermediary.execute("VACUUM INTO ?1", [database.to_str().unwrap()]).unwrap();
    drop(intermediary);
    let intermediary = Connection::open(&database).unwrap();
    rag_rat_db::schema::apply(&intermediary, &crate::test_hooks()).unwrap();
    assert!(coverage::stream_pending(&intermediary, stream_id).unwrap());
    sync_chains(&intermediary, &fresh, account);
    assert!(live_rows(&fresh).iter().any(|(id, _)| id == "deleted"));
    assert!(coverage::stream_pending(&fresh, stream_id).unwrap());
    assert!(!live_rows(&source).iter().any(|(id, _)| id == "deleted"));
    ingest_received_against(
        &intermediary,
        &IngestRoute { account_id: account, stream: &route, registry: &[REPO_SPEC] },
        &TableSyncReceived {
            expected_device: offered.device_fingerprint,
            signed_bytes: &suffix[1].signed_bytes,
            advertised_floor: None,
            advertised_tip: Some(TableSyncChainCursor {
                lamport: offered.lamport,
                entry_hash: offered.entry_hash,
            }),
        },
        2,
        &Default::default(),
    )
    .unwrap();
    assert!(middle.lamport > floor.lamport && middle.lamport < offered.lamport);
    assert!(coverage::stream_pending(&intermediary, stream_id).unwrap());
    rag_rat_db::schema::purge_repo_rows(&intermediary, "repo-a").unwrap();
    assert!(chain_lamports(&intermediary).is_empty());
    assert_eq!(
        intermediary
            .query_row("SELECT COUNT(*) FROM table_sync_streams", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        intermediary
            .query_row("SELECT COUNT(*) FROM table_sync_retained_floors", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(
        accepted_chain_page(&intermediary, route.stream_id, None, 16).unwrap().is_empty(),
        "debt alone advertises no signed history"
    );
    assert!(coverage::stream_pending(&intermediary, stream_id).unwrap());
    let different = scope_stream_id("repo-a", account, [0x99; 32], REPO_SPEC.scope_id);
    assert!(!coverage::stream_pending(&intermediary, different).unwrap());
    // Same-incarnation rejoin restores the witness but still owes the original tip.
    ingest_received_against(
        &intermediary,
        &IngestRoute { account_id: account, stream: &route, registry: &[REPO_SPEC] },
        &TableSyncReceived {
            expected_device: offered.device_fingerprint,
            signed_bytes: &suffix[1].signed_bytes,
            advertised_floor: Some(floor),
            advertised_tip: Some(TableSyncChainCursor {
                lamport: offered.lamport,
                entry_hash: offered.entry_hash,
            }),
        },
        3,
        &Default::default(),
    )
    .unwrap();
    assert_eq!(chain_lamports(&intermediary), [middle.lamport as i64]);
    assert!(coverage::stream_pending(&intermediary, stream_id).unwrap());
    // Reaching the full source is sufficient to repair the stale projection.
    sync_chains(&source, &intermediary, account);
    sync_chains(&intermediary, &fresh, account);
    assert_eq!(live_rows(&fresh), live_rows(&source));
    assert!(!coverage::stream_pending(&intermediary, stream_id).unwrap());
    assert!(!coverage::stream_pending(&fresh, stream_id).unwrap());
}
