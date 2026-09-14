use super::*;
use crate::table_sync::row_op::TypedValue;

/// The account every table-sync test scopes to. `accept_row_entry` gates on the signing device
/// being a roster-effective writer of THIS account.
fn account() -> AccountId {
    AccountId::from_bytes([9; 32])
}

/// Insert a roster-effective row so `device_is_effective_writer(account(), fp)` sees `fp` at
/// `role`. `roster_ref` is the PRIMARY KEY, so it must be unique per row — the fingerprint is a
/// fine per-device key for a test, and `INSERT OR IGNORE` keeps re-enrollment idempotent.
fn enroll(c: &rusqlite::Connection, account: AccountId, fp: DeviceFingerprint, role: &str) {
    c.execute(
        "INSERT OR IGNORE INTO account_roster_history
                 (roster_ref, account_id, device_fingerprint, role, effective_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, 0, NULL)",
        params![
            fp.to_bytes().as_slice(),
            account.to_bytes().as_slice(),
            fp.to_bytes().as_slice(),
            role
        ],
    )
    .unwrap();
}

/// Mark `fp`'s roster row removed (`closed_at` set) — an off-roster device after removal.
fn remove_from_roster(c: &rusqlite::Connection, account: AccountId, fp: DeviceFingerprint) {
    c.execute(
        "UPDATE account_roster_history SET closed_at = 1
             WHERE account_id = ?1 AND device_fingerprint = ?2",
        params![account.to_bytes().as_slice(), fp.to_bytes().as_slice()],
    )
    .unwrap();
}

fn conn() -> rusqlite::Connection {
    let c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    // The behavior tests (chain / lamport / fork / payload) use the `[1; 32]` device; enroll it
    // as an effective writer of account() so the #935 authority gate admits it and they reach
    // the logic under test. The authority tests below use OTHER devices and set their
    // own state.
    enroll(&c, account(), DeviceSecret::from_seed(&[1; 32]).public().fingerprint(), "owner");
    c
}

fn stream() -> StreamId {
    StreamId::from_bytes([5; 32])
}

fn op(id: &str) -> RowOp {
    RowOp::Remove {
        spec_version: 1,
        table: "t".to_string(),
        pk: vec![TypedValue::Text(id.to_string())],
    }
}

/// What a restatement may pack is the transport limit less this bound, so the bound must
/// cover the real envelope at its widest: a linked entry, a full-width lamport, the domains,
/// the fingerprint, the signature and every CBOR header. A bound that fell short would not
/// error — `author_row_entry_if_it_fits` would decline the batch and the pin would never move.
#[test]
fn an_envelope_never_adds_more_than_the_overhead_bound() {
    let secret = crate::device::DeviceSecret::from_seed(&[0x51; 32]);
    let op_bytes = vec![0xAB; super::super::TABLE_SYNC_ENTRY_MAX_BYTES - 1];
    let signed = entry::sign_entry_from_op_bytes(
        &secret,
        StreamId::from_bytes([0x33; 32]),
        Some(EntryHash::from_bytes([0x44; 32])),
        MAX_ENTRY_LAMPORT - 1,
        op_bytes.clone(),
    );
    let overhead = signed.signed_bytes.len() - op_bytes.len();
    assert!(
        overhead <= super::super::TABLE_SYNC_ENTRY_OVERHEAD_MAX,
        "the envelope adds {overhead} bytes, over the {} bound",
        super::super::TABLE_SYNC_ENTRY_OVERHEAD_MAX
    );
}

#[test]
fn readoption_work_enqueues_reads_rearms_and_completes() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let device = DeviceFingerprint::from_bytes([7; 32]);

    assert!(!has_pending_readoption_work(&tx, account(), stream()).unwrap());
    assert!(readoption_work_for_stream(&tx, account(), stream()).unwrap().is_none());

    enqueue_readoption_work(&tx, account(), device, stream(), [3; 32], 11, 12).unwrap();
    let work = readoption_work_for_stream(&tx, account(), stream()).unwrap().unwrap();
    assert_eq!(work.device_fingerprint, device);
    assert_eq!(work.roster_ref, [3; 32]);
    assert_eq!(work.removed_at_epoch, 11);
    assert!(has_pending_readoption_work(&tx, account(), stream()).unwrap());

    // A same-roster_ref re-enqueue (an idempotent re-fold) changes nothing.
    enqueue_readoption_work(&tx, account(), device, stream(), [3; 32], 11, 99).unwrap();
    let work = readoption_work_for_stream(&tx, account(), stream()).unwrap().unwrap();
    assert_eq!(work.removed_at_epoch, 11, "a re-fold of the same removal is inert");

    // A new removal (re-invite, newer epoch) re-arms the row even before completion.
    enqueue_readoption_work(&tx, account(), device, stream(), [4; 32], 21, 22).unwrap();
    let work = readoption_work_for_stream(&tx, account(), stream()).unwrap().unwrap();
    assert_eq!(work.roster_ref, [4; 32]);
    assert_eq!(work.removed_at_epoch, 21, "the new removal replaces the stale record");

    complete_readoption_work(&tx, account(), device, stream(), 30).unwrap();
    assert!(readoption_work_for_stream(&tx, account(), stream()).unwrap().is_none());
    assert!(!has_pending_readoption_work(&tx, account(), stream()).unwrap());

    // A STALE removal (an older closed fact, enqueued late by an arbitrary fold order) must
    // not re-arm the drained row: the update is monotonic on removed_at_epoch.
    enqueue_readoption_work(&tx, account(), device, stream(), [3; 32], 11, 40).unwrap();
    assert!(
        !has_pending_readoption_work(&tx, account(), stream()).unwrap(),
        "an older removal never re-arms a settled row",
    );

    // A genuinely newer removal still re-arms after completion.
    enqueue_readoption_work(&tx, account(), device, stream(), [5; 32], 31, 41).unwrap();
    let work = readoption_work_for_stream(&tx, account(), stream()).unwrap().unwrap();
    assert_eq!(work.roster_ref, [5; 32]);
    assert_eq!(work.removed_at_epoch, 31);
    tx.commit().unwrap();
}

#[test]
fn a_first_stream_context_adopts_parked_work_and_stamps_it() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let device = DeviceFingerprint::from_bytes([7; 32]);
    let parked = StreamId::from_bytes([0; 32]);

    // The removal folded while this account had NO streams: the work parks under the
    // placeholder stream id.
    enqueue_readoption_work(&tx, account(), device, parked, [3; 32], 11, 12).unwrap();
    assert!(!has_pending_readoption_work(&tx, account(), stream()).unwrap());

    // The first authored or ingested entry records this stream's context, and the parked
    // work moves onto it — once, never again.
    record_stream_context(&tx, stream(), "repo", account(), [0x44; 32], "demo/1").unwrap();
    let work = readoption_work_for_stream(&tx, account(), stream()).unwrap().unwrap();
    assert_eq!(work.roster_ref, [3; 32]);
    assert!(
        !has_pending_readoption_work(&tx, account(), parked).unwrap(),
        "the placeholder copy is stamped processed when it is adopted"
    );
    // Recording another stream does NOT re-adopt the same removal.
    let other = StreamId::from_bytes([6; 32]);
    record_stream_context(&tx, other, "repo", account(), [0x44; 32], "demo/2").unwrap();
    assert!(
        readoption_work_for_stream(&tx, account(), other).unwrap().is_none(),
        "a stamped placeholder does not copy into later streams"
    );
    tx.commit().unwrap();
}

#[test]
fn readoption_audit_records_provenance() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    record_readoption_audit(&tx, ReadoptionAudit {
        account_id: account(),
        removed: DeviceFingerprint::from_bytes([7; 32]),
        adopter: DeviceFingerprint::from_bytes([8; 32]),
        stream: stream(),
        repo_id: "repo".to_string(),
        scope_id: "demo/1".to_string(),
        table_name: "t".to_string(),
        row_pk: "aa".to_string(),
        original_lamport: 7,
        original_entry_hash: Some(EntryHash::from_bytes([1; 32])),
        adopted_entry_hash: EntryHash::from_bytes([2; 32]),
        adopted_at_ms: 42,
    })
    .unwrap();
    let row: (i64, Vec<u8>, Vec<u8>, i64) = tx
        .query_row(
            "SELECT original_lamport, original_entry_hash, adopted_entry_hash, adopted_at_ms
                 FROM table_sync_readoption_audit",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(row, (7, vec![1; 32], vec![2; 32], 42));
    tx.commit().unwrap();
}

#[test]
fn author_then_accept_round_trips_a_row_op() {
    let mut a = conn();
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let tx = a.transaction().unwrap();
    let signed = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
    tx.commit().unwrap();

    // A fresh store accepts the wire and decodes the same op.
    let mut b = conn();
    let tx = b.transaction().unwrap();
    let outcome = accept_row_entry(
        &tx,
        &AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t"],
            pubkey: &secret.public(),
            now_ms: 0,
        },
        &signed.signed_bytes,
        None,
    )
    .unwrap();
    assert_eq!(outcome, AcceptOutcome::Stored {
        op: op("r1"),
        meta: OpMeta { lamport: 0, device: secret.public().fingerprint() },
        entry_hash: signed.entry.entry_hash,
        prev_hash: None,
    });
}

#[test]
fn oversized_entries_are_refused_before_storage_and_authoring_recovers() {
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let mut local = conn();
    let tx = local.transaction().unwrap();
    let oversized = op(&"x".repeat(crate::table_sync::TABLE_SYNC_ENTRY_MAX_BYTES));
    let error = author_row_entry(&tx, stream(), &secret, &oversized, 0).unwrap_err();
    assert!(error.to_string().contains("over the 65536-byte transport limit"));
    let accepted: i64 =
        tx.query_row("SELECT COUNT(*) FROM table_sync_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(accepted, 0, "an oversized local entry never reaches accepted history");
    author_row_entry(&tx, stream(), &secret, &op("repaired"), 1).unwrap();
    tx.commit().unwrap();

    let forged = entry::sign_entry_from_op_bytes(
        &secret,
        stream(),
        None,
        0,
        vec![0; crate::table_sync::TABLE_SYNC_ENTRY_MAX_BYTES],
    );
    let mut remote = conn();
    let tx = remote.transaction().unwrap();
    let error = accept_row_entry(
        &tx,
        &AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t"],
            pubkey: &secret.public(),
            now_ms: 0,
        },
        &forged.signed_bytes,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("over the 65536-byte transport limit"));
    let accepted: i64 =
        tx.query_row("SELECT COUNT(*) FROM table_sync_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(accepted, 0, "an oversized received entry never reaches accepted history");
}

#[test]
fn retained_local_tip_blocks_second_genesis_until_the_tip_is_restored() {
    let mut c = conn();
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let tx = c.transaction().unwrap();
    let first = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
    tx.commit().unwrap();

    // Repository purge removes the accepted log but deliberately leaves the witness.
    c.execute("DELETE FROM table_sync_entries WHERE stream_id = ?1", [stream()
        .to_bytes()
        .as_slice()])
        .unwrap();
    let tx = c.transaction().unwrap();
    let error = author_row_entry(&tx, stream(), &secret, &op("r2"), 1).unwrap_err();
    assert!(error.to_string().contains("continuity is not restored"));
    tx.rollback().unwrap();

    // Re-delivering the exact retained tip restores continuity. The next local entry extends it
    // rather than emitting another genesis.
    let tx = c.transaction().unwrap();
    assert!(matches!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 2
            },
            &first.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::Stored { .. }
    ));
    let second = author_row_entry(&tx, stream(), &secret, &op("r2"), 3).unwrap();
    assert_eq!(second.entry.prev_hash, Some(first.entry.entry_hash));
    assert_eq!(second.entry.lamport, 1);
    tx.commit().unwrap();
}

#[test]
fn retained_high_lamport_tip_allows_exact_and_direct_successor_restoration() {
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let high_lamport = MAX_LAMPORT_ADVANCE * 2;
    let tip = entry::sign_entry_from_op_bytes(
        &secret,
        stream(),
        None,
        high_lamport,
        row_op::encode(&op("tip")),
    );
    let successor = entry::sign_entry_from_op_bytes(
        &secret,
        stream(),
        Some(tip.entry.entry_hash),
        high_lamport + 1,
        row_op::encode(&op("successor")),
    );

    for candidate in [&tip, &successor] {
        let mut c = conn();
        c.execute(
            "INSERT INTO table_sync_chain_tips(
                     stream_id, device_fingerprint, lamport, entry_hash
                 ) VALUES (?1, ?2, ?3, ?4)",
            params![
                stream().to_bytes().as_slice(),
                secret.public().fingerprint().to_bytes().as_slice(),
                i64::try_from(high_lamport).unwrap(),
                tip.entry.entry_hash.as_slice(),
            ],
        )
        .unwrap();
        let tx = c.transaction().unwrap();
        assert!(matches!(
            accept_row_entry(
                &tx,
                &AcceptCtx {
                    account_id: account(),
                    expected_stream: stream(),
                    expected_tables: &["t"],
                    pubkey: &secret.public(),
                    now_ms: 0
                },
                &candidate.signed_bytes,
                None
            )
            .unwrap(),
            AcceptOutcome::Stored { .. }
        ));
    }
}

#[test]
fn stream_context_conflicts_fail_closed() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    record_stream_context(&tx, stream(), "repo", account(), [1; 32], "demo/1").unwrap();
    let error =
        record_stream_context(&tx, stream(), "repo", account(), [2; 32], "demo/1").unwrap_err();
    assert!(error.to_string().contains("conflicts"));
}

#[test]
fn lamport_advances_and_restores_from_the_stored_tail() {
    let mut a = conn();
    let secret = DeviceSecret::from_seed(&[1; 32]);
    {
        let tx = a.transaction().unwrap();
        assert_eq!(
            author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap().entry.lamport,
            0
        );
        assert_eq!(
            author_row_entry(&tx, stream(), &secret, &op("r2"), 0).unwrap().entry.lamport,
            1
        );
        tx.commit().unwrap();
    }
    // Re-opening the transaction continues from the stored tail (max seen + 1), not from 0.
    let tx = a.transaction().unwrap();
    assert_eq!(author_row_entry(&tx, stream(), &secret, &op("r3"), 0).unwrap().entry.lamport, 2);
}

#[test]
fn a_redelivered_entry_is_idempotent() {
    let mut b = conn();
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let signed = {
        let mut a = conn();
        let tx = a.transaction().unwrap();
        let s = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
        tx.commit().unwrap();
        s
    };
    let tx = b.transaction().unwrap();
    assert!(matches!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &signed.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::Stored { .. }
    ));
    assert_eq!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &signed.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::AlreadyPresent,
    );
}

#[test]
fn an_entry_for_a_foreign_stream_is_rejected() {
    let mut b = conn();
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let signed = {
        let mut a = conn();
        let tx = a.transaction().unwrap();
        let s = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
        tx.commit().unwrap();
        s
    };
    let tx = b.transaction().unwrap();
    let other = StreamId::from_bytes([9; 32]);
    assert!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: other,
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &signed.signed_bytes,
            None
        )
        .is_err(),
        "an entry cannot be re-homed onto a stream it was not signed for",
    );
}

#[test]
fn a_foreign_table_op_is_stored_inert_and_does_not_wedge_the_chain() {
    let secret = DeviceSecret::from_seed(&[1; 32]);
    // Sender authors two chained ops for table "t".
    let (first, second) = {
        let mut a = conn();
        let tx = a.transaction().unwrap();
        let first = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
        let second = author_row_entry(&tx, stream(), &secret, &op("r2"), 0).unwrap();
        tx.commit().unwrap();
        (first, second)
    };
    let mut b = conn();
    let tx = b.transaction().unwrap();
    // The genesis routed to a scope that does NOT include "t": stored INERT (the chain still
    // advances), not applied.
    assert_eq!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["other"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &first.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::StoredInert {
            reason: PendingReason::TableNotInScope,
            entry_hash: first.entry.entry_hash,
            prev_hash: None,
        },
    );
    // The chain is not wedged: the next entry (which links to the first) still stores +
    // applies.
    assert!(matches!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &second.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::Stored { .. },
    ));
}

#[test]
fn a_malformed_payload_is_stored_inert_and_does_not_wedge_the_chain() {
    let secret = DeviceSecret::from_seed(&[1; 32]);
    // Sender: a genesis entry with GARBAGE (undecodable) op-bytes, then a valid entry chained
    // onto it.
    let (garbage, valid) = {
        let mut a = conn();
        let tx = a.transaction().unwrap();
        let garbage = entry::sign_entry_from_op_bytes(&secret, stream(), None, 0, vec![0x00]);
        insert_entry(&tx, &garbage.entry, &garbage.signed_bytes, 0, None).unwrap();
        let valid = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
        tx.commit().unwrap();
        (garbage, valid)
    };
    let mut b = conn();
    let tx = b.transaction().unwrap();
    assert_eq!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &garbage.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::StoredInert {
            reason: PendingReason::UndecodablePayload,
            entry_hash: garbage.entry.entry_hash,
            prev_hash: None,
        },
    );
    // One bad payload does not wedge the chain: the next valid entry still applies.
    assert!(matches!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &valid.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::Stored { .. },
    ));
}

#[test]
fn an_out_of_bound_lamport_is_rejected() {
    let secret = DeviceSecret::from_seed(&[1; 32]);
    // A signed genesis claiming a near-maximal lamport would make every peer's next
    // MAX(lamport)+1 overflow i64 at insert — it must be refused before it is stored.
    let poison = entry::sign_entry_from_op_bytes(
        &secret,
        stream(),
        None,
        u64::MAX,
        row_op::encode(&op("r1")),
    );
    let mut b = conn();
    let tx = b.transaction().unwrap();
    assert!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &poison.signed_bytes,
            None
        )
        .is_err(),
        "an out-of-bound lamport is rejected before it can poison the stream counter",
    );
}

#[test]
fn a_lamport_jump_beyond_the_advance_bound_is_rejected() {
    // On an empty stream the clock is 0, so the largest acceptable lamport is exactly the
    // advance bound; one past it is a griefing jump (it would dominate every row's LWW
    // and, near the ceiling, halt local authoring). Two fresh streams so the accepted
    // entry does not raise the clock the rejected one is measured against.
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let at_bound = entry::sign_entry_from_op_bytes(
        &secret,
        stream(),
        None,
        MAX_LAMPORT_ADVANCE,
        row_op::encode(&op("r1")),
    );
    let beyond = entry::sign_entry_from_op_bytes(
        &secret,
        stream(),
        None,
        MAX_LAMPORT_ADVANCE + 1,
        row_op::encode(&op("r2")),
    );

    let mut ok = conn();
    let tx = ok.transaction().unwrap();
    assert!(
        matches!(
            accept_row_entry(
                &tx,
                &AcceptCtx {
                    account_id: account(),
                    expected_stream: stream(),
                    expected_tables: &["t"],
                    pubkey: &secret.public(),
                    now_ms: 0
                },
                &at_bound.signed_bytes,
                None
            )
            .unwrap(),
            AcceptOutcome::Stored { .. },
        ),
        "a lamport exactly at the advance bound is accepted",
    );

    let mut bad = conn();
    let tx = bad.transaction().unwrap();
    assert!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &beyond.signed_bytes,
            None
        )
        .is_err(),
        "a lamport one past the advance bound is refused",
    );
}

#[test]
fn a_gap_is_retained_awaiting_its_predecessor() {
    // A second-position entry (lamport 1) arriving before the genesis has a missing
    // predecessor: it is held rather than dropped, so reverse delivery can still converge.
    let mut b = conn();
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let second = {
        let mut a = conn();
        let tx = a.transaction().unwrap();
        author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
        let s = author_row_entry(&tx, stream(), &secret, &op("r2"), 0).unwrap();
        tx.commit().unwrap();
        s
    };
    let tx = b.transaction().unwrap();
    assert_eq!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &second.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::GapRetained,
    );
    let held: i64 =
        tx.query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |r| r.get(0)).unwrap();
    assert_eq!(held, 1, "and it is HELD, not merely reported — the verdict alone is not the fix");
    let accepted: i64 =
        tx.query_row("SELECT COUNT(*) FROM table_sync_entries", [], |r| r.get(0)).unwrap();
    assert_eq!(accepted, 0, "but it is not on the accepted chain");
}

/// Two entries citing the SAME predecessor, both arriving before it. Both are held — neither
/// can be classified yet, because the predecessor that would make one of them a second
/// successor is not here. The equivocation only becomes visible on promotion.
#[test]
fn two_siblings_awaiting_one_predecessor_are_both_held() {
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let (genesis, first) = {
        let mut a = conn();
        let tx = a.transaction().unwrap();
        let genesis = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
        let first = author_row_entry(&tx, stream(), &secret, &op("r2"), 0).unwrap();
        tx.commit().unwrap();
        (genesis, first)
    };
    let sibling = entry::sign_entry_from_op_bytes(
        &secret,
        stream(),
        Some(genesis.entry.entry_hash),
        first.entry.lamport + 1,
        row_op::encode(&op("r_sibling")),
    );

    let mut b = conn();
    let tx = b.transaction().unwrap();
    for bytes in [&first.signed_bytes, &sibling.signed_bytes] {
        assert_eq!(
            accept_row_entry(
                &tx,
                &AcceptCtx {
                    account_id: account(),
                    expected_stream: stream(),
                    expected_tables: &["t"],
                    pubkey: &secret.public(),
                    now_ms: 0
                },
                bytes,
                None
            )
            .unwrap(),
            AcceptOutcome::GapRetained,
        );
    }
    let held: i64 =
        tx.query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |r| r.get(0)).unwrap();
    assert_eq!(held, 2, "both siblings are held; neither can be judged without the parent");
}

/// The cap evicts the FURTHEST-AHEAD held entry, not the newcomer. Refusing the newcomer would
/// let whoever filled the table first block the near-tail entry the chain actually needs.
#[test]
fn the_cap_evicts_the_furthest_ahead_held_entry() {
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let mut b = conn();
    let tx = b.transaction().unwrap();
    let stream_bytes = stream().to_bytes();
    let device_bytes = secret.public().fingerprint().to_bytes();
    // Fill the chain to the cap with synthetic held rows at high lamports.
    for i in 0..MAX_GAPPED_PER_CHAIN {
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&(i as u64).to_be_bytes());
        tx.execute(
            "INSERT INTO table_sync_gapped_entries(entry_hash, stream_id, device_fingerprint, \
             lamport, prev_hash, signed_bytes, gapped_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![
                hash.as_slice(),
                stream_bytes.as_slice(),
                device_bytes.as_slice(),
                i64::try_from(1_000_000 + i).unwrap(),
                [9u8; 32].as_slice(),
                [0u8; 4].as_slice(),
            ],
        )
        .unwrap();
    }
    let highest = i64::try_from(1_000_000 + MAX_GAPPED_PER_CHAIN - 1).unwrap();

    // A near-tail entry arrives with the table full.
    let newcomer = entry::sign_entry_from_op_bytes(
        &secret,
        stream(),
        Some(EntryHash::from_bytes([7u8; 32])),
        5,
        row_op::encode(&op("r_new")),
    );
    assert_eq!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &newcomer.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::GapRetained,
        "the newcomer is held, not refused",
    );
    let still_capped: i64 =
        tx.query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |r| r.get(0)).unwrap();
    assert_eq!(usize::try_from(still_capped).unwrap(), MAX_GAPPED_PER_CHAIN, "the cap holds");
    let evicted: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM table_sync_gapped_entries WHERE lamport = ?1",
            params![highest],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(evicted, 0, "the furthest-ahead entry made room, not the arriving one");
    let kept: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM table_sync_gapped_entries WHERE entry_hash = ?1",
            params![newcomer.entry.entry_hash.as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1, "and the near-tail newcomer is the one held");

    // The OTHER direction: an arrival further ahead than everything held is itself the entry
    // the policy drops. Evicting the stored maximum for it would invert the policy — a table of
    // near-tail entries would be hollowed out by a stream of ever-higher-lamport arrivals.
    let far = entry::sign_entry_from_op_bytes(
        &secret,
        stream(),
        Some(EntryHash::from_bytes([7u8; 32])),
        9_000_000,
        row_op::encode(&op("r_far")),
    );
    assert_eq!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &far.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::GapChainFull,
        "the furthest-ahead arrival is refused, and says so rather than reporting itself held",
    );
    let far_held: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM table_sync_gapped_entries WHERE entry_hash = ?1",
            params![far.entry.entry_hash.as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(far_held, 0, "it is not held");
    let survivors: i64 =
        tx.query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |r| r.get(0)).unwrap();
    assert_eq!(
        usize::try_from(survivors).unwrap(),
        MAX_GAPPED_PER_CHAIN,
        "and it displaced nothing",
    );
}

/// At the cap boundary the arrival's lamport can TIE the furthest-ahead held entry. Ranking on
/// lamport alone resolves that as "whoever is already held wins" — insertion order again, the
/// property the hash tie-break exists to remove. The comparison therefore uses the same
/// `(lamport, entry_hash)` order the eviction victim is chosen by, so two ties on opposite
/// sides of the held entry get opposite answers.
#[test]
fn a_lamport_tie_at_the_cap_boundary_is_broken_by_hash() {
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let mut b = conn();
    let tx = b.transaction().unwrap();
    let stream_bytes = stream().to_bytes();
    let device_bytes = secret.public().fingerprint().to_bytes();
    // Fill to the cap. Every row sits at the SAME lamport with a mid-range hash, so a real
    // signed entry at that lamport can sort on either side of the furthest-ahead one.
    const BOUNDARY: i64 = 500;
    for i in 0..MAX_GAPPED_PER_CHAIN {
        let mut hash = [0x80u8; 32];
        hash[8..16].copy_from_slice(&(i as u64).to_be_bytes());
        tx.execute(
            "INSERT INTO table_sync_gapped_entries(entry_hash, stream_id, device_fingerprint, \
             lamport, prev_hash, signed_bytes, gapped_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![
                hash.as_slice(),
                stream_bytes.as_slice(),
                device_bytes.as_slice(),
                BOUNDARY,
                [9u8; 32].as_slice(),
                [0u8; 4].as_slice(),
            ],
        )
        .unwrap();
    }
    let furthest: Vec<u8> = tx
        .query_row(
            "SELECT entry_hash FROM table_sync_gapped_entries
                  WHERE stream_id = ?1 AND lamport = ?2 ORDER BY entry_hash DESC LIMIT 1",
            params![stream_bytes.as_slice(), BOUNDARY],
            |r| r.get(0),
        )
        .unwrap();

    let (mut lower, mut higher) = (None, None);
    for nonce in 0u32..256 {
        let tie = entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            Some(EntryHash::from_bytes([7u8; 32])),
            u64::try_from(BOUNDARY).unwrap(),
            row_op::encode(&op(&format!("tie{nonce}"))),
        );
        if tie.entry.entry_hash.as_slice() < furthest.as_slice() {
            lower.get_or_insert(tie);
        } else {
            higher.get_or_insert(tie);
        }
        if lower.is_some() && higher.is_some() {
            break;
        }
    }
    let (lower, higher) = (lower.expect("a lower-hash tie"), higher.expect("a higher-hash tie"));

    assert_eq!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &higher.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::GapChainFull,
        "a tie sorting ABOVE the furthest held entry is the one dropped",
    );
    assert_eq!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &lower.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::GapRetained,
        "a tie sorting BELOW it displaces it instead",
    );
}

/// Which of two same-lamport siblings takes the successor slot must be a property of the
/// ENTRIES, not of the order they happened to be inserted in. On lamport alone, SQLite falls
/// back to physical row order, so two replicas holding the same pair could accept different
/// successors and project different rows.
#[test]
fn the_sibling_taken_first_does_not_depend_on_insertion_order() {
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let genesis = {
        let mut a = conn();
        let tx = a.transaction().unwrap();
        let g = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
        tx.commit().unwrap();
        g
    };
    let sib = |id: &str| {
        entry::sign_entry_from_op_bytes(
            &secret,
            stream(),
            Some(genesis.entry.entry_hash),
            7,
            row_op::encode(&op(id)),
        )
    };
    let (one, two) = (sib("r_one"), sib("r_two"));

    // Two stores, same pair of siblings, opposite insertion orders.
    let taken_by = |order: [&crate::entry::SignedEntry; 2]| {
        let mut b = conn();
        let tx = b.transaction().unwrap();
        for e in order {
            accept_row_entry(
                &tx,
                &AcceptCtx {
                    account_id: account(),
                    expected_stream: stream(),
                    expected_tables: &["t"],
                    pubkey: &secret.public(),
                    now_ms: 0,
                },
                &e.signed_bytes,
                None,
            )
            .unwrap();
        }
        take_gapped_child(&tx, stream(), secret.public().fingerprint(), &genesis.entry.entry_hash)
            .unwrap()
            .expect("a sibling is available")
            .entry_hash
    };

    assert_eq!(
        taken_by([&one, &two]),
        taken_by([&two, &one]),
        "the same pair yields the same winner whichever arrived first",
    );
}

#[test]
fn a_fork_linking_past_the_tail_to_a_stored_ancestor_is_a_conflict() {
    let secret = DeviceSecret::from_seed(&[1; 32]);
    // Device A's real chain: e1 (genesis) -> e2.
    let (e1, e2) = {
        let mut a = conn();
        let tx = a.transaction().unwrap();
        let e1 = author_row_entry(&tx, stream(), &secret, &op("r1"), 0).unwrap();
        let e2 = author_row_entry(&tx, stream(), &secret, &op("r2"), 0).unwrap();
        tx.commit().unwrap();
        (e1, e2)
    };
    // A FORK: a SECOND successor of e1 (prev = e1's hash) with a lamport PAST the tail e2.
    let fork = entry::sign_entry_from_op_bytes(
        &secret,
        stream(),
        Some(e1.entry.entry_hash),
        e2.entry.lamport + 1,
        row_op::encode(&op("r_fork")),
    );

    let mut b = conn();
    let tx = b.transaction().unwrap();
    accept_row_entry(
        &tx,
        &AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t"],
            pubkey: &secret.public(),
            now_ms: 0,
        },
        &e1.signed_bytes,
        None,
    )
    .unwrap();
    accept_row_entry(
        &tx,
        &AcceptCtx {
            account_id: account(),
            expected_stream: stream(),
            expected_tables: &["t"],
            pubkey: &secret.public(),
            now_ms: 0,
        },
        &e2.signed_bytes,
        None,
    )
    .unwrap();
    // Links past the tail to the STORED ancestor e1 (which already has a successor) → an
    // equivocation, not a missing predecessor.
    assert_eq!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &fork.signed_bytes,
            None
        )
        .unwrap(),
        AcceptOutcome::Fork,
        "a fork linking to a stored ancestor is a Fork, not a MissingPredecessor",
    );
}

// ─── #935: roster/role authority gate ───

/// A signed, decodable row entry from `secret` on `stream()` — built without storing (like the
/// garbage/fork helpers), enough to drive the authority gate.
fn signed_row(secret: &DeviceSecret, id: &str) -> SignedEntry {
    entry::sign_entry_from_op_bytes(secret, stream(), None, 0, row_op::encode(&op(id)))
}

fn accept(
    tx: &Transaction<'_>,
    acct: AccountId,
    signed: &SignedEntry,
    pubkey: &DevicePublic,
) -> AcceptOutcome {
    accept_row_entry(
        tx,
        &AcceptCtx {
            account_id: acct,
            expected_stream: stream(),
            expected_tables: &["t"],
            pubkey,
            now_ms: 0,
        },
        &signed.signed_bytes,
        None,
    )
    .unwrap()
}

fn stream_entry_count(tx: &Transaction<'_>) -> i64 {
    tx.query_row(
        "SELECT COUNT(*) FROM table_sync_entries WHERE stream_id = ?1",
        params![stream().to_bytes().as_slice()],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn an_off_roster_device_is_unauthorized_and_stores_nothing() {
    let secret = DeviceSecret::from_seed(&[2; 32]); // never enrolled
    let signed = signed_row(&secret, "r1");
    let mut b = conn();
    let tx = b.transaction().unwrap();
    assert_eq!(accept(&tx, account(), &signed, &secret.public()), AcceptOutcome::Unauthorized);
    assert_eq!(stream_entry_count(&tx), 0, "an unauthorized entry advances no chain");
}

#[test]
fn a_read_only_device_is_unauthorized() {
    let secret = DeviceSecret::from_seed(&[3; 32]);
    let signed = signed_row(&secret, "r1");
    let mut b = conn();
    enroll(&b, account(), secret.public().fingerprint(), "read_only");
    let tx = b.transaction().unwrap();
    assert_eq!(accept(&tx, account(), &signed, &secret.public()), AcceptOutcome::Unauthorized);
}

#[test]
fn a_member_and_an_owner_may_author() {
    for (seed, role) in [([4u8; 32], "member"), ([1u8; 32], "owner")] {
        let secret = DeviceSecret::from_seed(&seed);
        let signed = signed_row(&secret, "r1");
        let mut b = conn();
        enroll(&b, account(), secret.public().fingerprint(), role);
        let tx = b.transaction().unwrap();
        assert!(
            matches!(
                accept(&tx, account(), &signed, &secret.public()),
                AcceptOutcome::Stored { .. }
            ),
            "{role} may author table rows",
        );
    }
}

#[test]
fn a_removed_writer_is_unauthorized() {
    let secret = DeviceSecret::from_seed(&[1; 32]); // conn() enrolls it as owner
    let signed = signed_row(&secret, "r1");
    let mut b = conn();
    remove_from_roster(&b, account(), secret.public().fingerprint());
    let tx = b.transaction().unwrap();
    assert_eq!(accept(&tx, account(), &signed, &secret.public()), AcceptOutcome::Unauthorized);
}

#[test]
fn an_unauthorized_floor_redelivery_keeps_its_gapped_copy() {
    let secret = DeviceSecret::from_seed(&[1; 32]); // conn() enrolls it as owner
    let signed = signed_row(&secret, "r1");
    let mut b = conn();
    b.execute(
        "INSERT INTO table_sync_gapped_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                 gapped_at_ms
             ) VALUES (?1, ?2, ?3, 0, ?4, ?5, 0)",
        params![
            signed.entry.entry_hash.as_slice(),
            stream().to_bytes().as_slice(),
            secret.public().fingerprint().to_bytes().as_slice(),
            [0u8; 32].as_slice(),
            signed.signed_bytes.as_slice(),
        ],
    )
    .unwrap();
    remove_from_roster(&b, account(), secret.public().fingerprint());

    let tx = b.transaction().unwrap();
    assert_eq!(
        accept_row_entry(
            &tx,
            &AcceptCtx {
                account_id: account(),
                expected_stream: stream(),
                expected_tables: &["t"],
                pubkey: &secret.public(),
                now_ms: 0
            },
            &signed.signed_bytes,
            Some(ChainCursor {
                lamport: signed.entry.lamport,
                entry_hash: signed.entry.entry_hash,
            })
        )
        .unwrap(),
        AcceptOutcome::Unauthorized,
    );
    let held: i64 =
        tx.query_row("SELECT COUNT(*) FROM table_sync_gapped_entries", [], |r| r.get(0)).unwrap();
    assert_eq!(held, 1, "authority rejection does not delete the parked floor");
}

#[test]
fn a_writer_in_another_account_is_unauthorized_here() {
    let secret = DeviceSecret::from_seed(&[1; 32]); // owner in account()
    let signed = signed_row(&secret, "r1");
    let mut b = conn();
    let tx = b.transaction().unwrap();
    let other = AccountId::from_bytes([0xAA; 32]);
    assert_eq!(accept(&tx, other, &signed, &secret.public()), AcceptOutcome::Unauthorized);
}

#[test]
fn the_authority_gate_precedes_forward_compat_retention() {
    // An off-roster device authoring an UNDECODABLE payload is dropped Unauthorized, never
    // retained StoredInert: the gate runs before the forward-compat path, so an unauthorized
    // principal can never populate a retained stream.
    let secret = DeviceSecret::from_seed(&[2; 32]);
    let garbage = entry::sign_entry_from_op_bytes(&secret, stream(), None, 0, vec![0x00]);
    let mut b = conn();
    let tx = b.transaction().unwrap();
    assert_eq!(accept(&tx, account(), &garbage, &secret.public()), AcceptOutcome::Unauthorized);
    assert_eq!(stream_entry_count(&tx), 0);
}

#[test]
fn an_unauthorized_drop_heals_after_the_device_is_enrolled() {
    // Roster lag: dropped before the author's DeviceAdd folds locally, accepted on the re-offer
    // after it does — so `Unauthorized` is retryable, not terminal.
    let secret = DeviceSecret::from_seed(&[5; 32]);
    let signed = signed_row(&secret, "r1");
    let mut b = conn();
    {
        let tx = b.transaction().unwrap();
        assert_eq!(accept(&tx, account(), &signed, &secret.public()), AcceptOutcome::Unauthorized);
        tx.commit().unwrap();
    }
    enroll(&b, account(), secret.public().fingerprint(), "member"); // the DeviceAdd folded
    let tx = b.transaction().unwrap();
    assert!(
        matches!(accept(&tx, account(), &signed, &secret.public()), AcceptOutcome::Stored { .. }),
        "the re-offer is accepted once the author is roster-effective",
    );
}

#[test]
fn already_present_takes_precedence_over_a_later_removal() {
    // An entry stored while the device WAS a writer still reports AlreadyPresent after removal
    // (the gate sits after `entry_exists`), preserving dedup/frontier semantics.
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let signed = signed_row(&secret, "r1");
    let mut b = conn();
    {
        let tx = b.transaction().unwrap();
        assert!(matches!(
            accept(&tx, account(), &signed, &secret.public()),
            AcceptOutcome::Stored { .. }
        ));
        tx.commit().unwrap();
    }
    remove_from_roster(&b, account(), secret.public().fingerprint());
    let tx = b.transaction().unwrap();
    assert_eq!(accept(&tx, account(), &signed, &secret.public()), AcceptOutcome::AlreadyPresent);
}
