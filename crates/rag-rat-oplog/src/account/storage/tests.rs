use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use rag_rat_db::schema;

use super::super::test_support;
use super::*;
use crate::account::content::{
    ContentEntryHeader, ContentRefoldBudget, content_ingest, settle_pending_content_refolds,
    sign_content_entry,
};
use crate::account::envelope::sign_account_entry;
use crate::account::ops::{ContentCut, DeviceCut, DeviceRole, GrantRole};
use crate::account::test_support::Dev;

const NOW: i64 = 1_700_000_000_000;

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::test_hooks()).unwrap();
    conn
}

#[test]
fn owned_stream_reader_returns_every_stream_for_the_account() {
    let conn = db();
    let account = AccountId::from_bytes([0xa1; 32]);
    for (stream, own) in [([1u8; 32], [11u8; 32]), ([2u8; 32], [22u8; 32])] {
        conn.execute(
            "INSERT INTO account_stream_ownership(
                     stream_id, account_id, own_id, effective_at
                 ) VALUES (?1, ?2, ?3, 1)",
            params![stream.as_slice(), account.to_bytes().as_slice(), own.as_slice(),],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO account_stream_ownership(
                 stream_id, account_id, own_id, effective_at
             ) VALUES (?1, ?2, ?3, 1)",
        params![[3u8; 32].as_slice(), [0xb2u8; 32].as_slice(), [33u8; 32].as_slice()],
    )
    .unwrap();

    assert_eq!(owned_streams_for_account(&conn, account).unwrap(), vec![
        StreamId::from_bytes([1; 32]),
        StreamId::from_bytes([2; 32])
    ]);
}

#[test]
fn effective_owner_incarnation_resolves_the_open_incarnation_not_a_closed_one() {
    // Reverse lookup by device: a device can hold a CLOSED prior incarnation and an OPEN
    // current one (demote-then-repromote). The StreamKeyWrap author cites the OPEN
    // owner_id as authority_ref — returning the closed one would cite an ineffective
    // incarnation and roll the mint back. The closed row here has the LATER
    // effective_at, so a query missing the `closed_at IS NULL` filter would wrongly
    // return it: the test bites that omission.
    let conn = db();
    let account = AccountId::from_bytes([0xa1; 32]);
    let device = DeviceFingerprint::from_bytes([0xd2; 32]);
    let open_owner = [0x22u8; 32];
    let closed_owner = [0x11u8; 32];
    conn.execute(
        "INSERT INTO account_owner_incarnations(
                 owner_id, account_id, device_fingerprint, effective_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, NULL)",
        params![
            open_owner.as_slice(),
            account.to_bytes().as_slice(),
            device.to_bytes().as_slice(),
            10_i64,
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO account_owner_incarnations(
                 owner_id, account_id, device_fingerprint, effective_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            closed_owner.as_slice(),
            account.to_bytes().as_slice(),
            device.to_bytes().as_slice(),
            30_i64, // LATER than the open one — a missing closed_at filter would pick this
            40_i64,
        ],
    )
    .unwrap();

    assert_eq!(
        effective_owner_incarnation_for_device(&conn, account, device).unwrap(),
        Some(Into::into(open_owner)),
        "resolves the OPEN incarnation, never the closed prior one",
    );
    // A device with no open incarnation resolves None.
    let stranger = DeviceFingerprint::from_bytes([0xee; 32]);
    assert_eq!(
        effective_owner_incarnation_for_device(&conn, account, stranger).unwrap(),
        None,
        "a device with no open owner incarnation resolves None",
    );
}

fn genesis(founder: &Dev) -> (AccountId, Vec<u8>, [u8; 32]) {
    let op = AccountOp::AccountGenesis {
        ed25519_pubkey: founder.ed,
        x25519_pubkey: founder.x,
        nonce16: [0u8; 16],
        created_at_ms: NOW as u64,
        label: None,
    };
    let payload = ops::encode(&op).unwrap();
    let account_id = id::account_id_from_genesis_payload(&payload);
    let header = AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: founder.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: ops::entry_type::ACCOUNT_GENESIS,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 0,
        key_id: None,
        authority_ref: None,
    };
    let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
    (account_id, signed.signed_bytes, signed.entry_hash.into())
}

#[allow(clippy::too_many_arguments)]
fn op(
    account_id: AccountId,
    signer: &Dev,
    seq: u64,
    prev: Option<[u8; 32]>,
    authority_ref: Option<OwnerId>,
    op: &AccountOp,
) -> (Vec<u8>, [u8; 32]) {
    let payload = ops::encode(op).unwrap();
    let header = AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: signer.fp,
        seq,
        prev_hash: prev.map(Into::into),
        parent_ref: None,
        entry_type: ops::entry_type_of(op),
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref,
    };
    let signed = sign_account_entry(&signer.secret, &header, &payload).unwrap();
    (signed.signed_bytes, signed.entry_hash.into())
}

/// `op`, but signed by the store's OWN device — the local identity is minted, never seeded from
/// a `Dev`, so any test that drives the production authoring seam has to author its history
/// under the same key.
#[allow(clippy::too_many_arguments)]
fn op_local(
    account_id: AccountId,
    device: &crate::identity::LocalDevice,
    seq: u64,
    prev: Option<[u8; 32]>,
    authority_ref: Option<OwnerId>,
    op: &AccountOp,
) -> (Vec<u8>, [u8; 32]) {
    let payload = ops::encode(op).unwrap();
    let header = AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: device.fingerprint(),
        seq,
        prev_hash: prev.map(Into::into),
        parent_ref: None,
        entry_type: ops::entry_type_of(op),
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref,
    };
    let signed = sign_account_entry(device.secret(), &header, &payload).unwrap();
    (signed.signed_bytes, signed.entry_hash.into())
}

fn device_add(dev: &Dev, role: DeviceRole) -> AccountOp {
    AccountOp::DeviceAdd {
        device_fingerprint: dev.fp,
        ed25519_pubkey: dev.ed,
        x25519_pubkey: dev.x,
        role,
        label: None,
    }
}

#[test]
fn enrollment_verification_requires_the_genesis_to_commit_to_the_account() {
    let founder = Dev::new(0x51);
    let joiner = Dev::new(0x52);
    let (account, genesis_bytes, genesis_hash) = genesis(&founder);
    let (device_add_bytes, device_add_hash) = op(
        account,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&joiner, DeviceRole::ReadOnly),
    );
    let bootstrap = vec![genesis_bytes, device_add_bytes.clone()];
    verify_enrollment_device_add(
        &bootstrap,
        account,
        AccountEntryHash::from_bytes(device_add_hash),
        &device_add_bytes,
        joiner.ed,
        joiner.x,
    )
    .unwrap();

    // An impostor signs its OWN genesis + DeviceAdd but stamps the VICTIM's account_id into
    // both headers: every signature verifies under the impostor's founder key, so canonical
    // genesis selection must reject the root whose payload does not self-hash to the account.
    let impostor = Dev::new(0x53);
    let victim = AccountId::from_bytes([0xaa; 32]);
    let impostor_genesis_op = AccountOp::AccountGenesis {
        ed25519_pubkey: impostor.ed,
        x25519_pubkey: impostor.x,
        nonce16: [0u8; 16],
        created_at_ms: NOW as u64,
        label: None,
    };
    let impostor_genesis_payload = ops::encode(&impostor_genesis_op).unwrap();
    assert_ne!(id::account_id_from_genesis_payload(&impostor_genesis_payload), victim);
    let impostor_genesis_header = AccountEntryHeader {
        account_id: victim,
        log_id: 0,
        device_fingerprint: impostor.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: ops::entry_type::ACCOUNT_GENESIS,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 0,
        key_id: None,
        authority_ref: None,
    };
    let impostor_genesis =
        sign_account_entry(&impostor.secret, &impostor_genesis_header, &impostor_genesis_payload)
            .unwrap();
    let (impostor_add_bytes, impostor_add_hash) = op(
        victim,
        &impostor,
        1,
        Some(impostor_genesis.entry_hash.into()),
        Some(impostor_genesis.entry_hash.into()),
        &device_add(&joiner, DeviceRole::ReadOnly),
    );
    let forged = vec![impostor_genesis.signed_bytes, impostor_add_bytes.clone()];
    let error = verify_enrollment_device_add(
        &forged,
        victim,
        AccountEntryHash::from_bytes(impostor_add_hash),
        &impostor_add_bytes,
        joiner.ed,
        joiner.x,
    )
    .expect_err("a genesis that does not self-hash to the expected account is rejected");
    assert!(error.to_string().contains("no accepted account genesis"), "unexpected error: {error}");
}

#[test]
fn enrollment_verification_ignores_a_parked_future_genesis_lookalike() {
    let conn = db();
    let founder = Dev::new(0x54);
    let joiner = Dev::new(0x55);
    let stranger = Dev::new(0x56);
    let (account, genesis_bytes, genesis_hash) = genesis(&founder);
    let (device_add_bytes, device_add_hash) = op(
        account,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&joiner, DeviceRole::ReadOnly),
    );
    assert!(matches!(
        account_ingest(&conn, &genesis_bytes, NOW).unwrap(),
        IngestOutcome::Ingested { .. }
    ));
    assert!(matches!(
        account_ingest(&conn, &device_add_bytes, NOW).unwrap(),
        IngestOutcome::Ingested { .. }
    ));

    let lookalike_payload = ops::encode(&AccountOp::AccountGenesis {
        ed25519_pubkey: stranger.ed,
        x25519_pubkey: stranger.x,
        nonce16: [7; 16],
        created_at_ms: NOW as u64,
        label: None,
    })
    .unwrap();
    let lookalike_header = AccountEntryHeader {
        account_id: account,
        log_id: fold::CONTROL_LOG,
        device_fingerprint: stranger.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: ops::entry_type::ACCOUNT_GENESIS,
        op_version: fold::SUPPORTED_OP_VERSION + 1,
        crypto_suite: 0,
        auth_len: 0,
        key_id: None,
        authority_ref: None,
    };
    let lookalike =
        sign_account_entry(&stranger.secret, &lookalike_header, &lookalike_payload).unwrap();
    assert_eq!(
        account_ingest(&conn, &lookalike.signed_bytes, NOW).unwrap(),
        IngestOutcome::PreVerify,
        "an unresolved future-version row follows the normal durable parking path",
    );

    let receipt = account_entries_for_sync(&conn, account)
        .unwrap()
        .into_iter()
        .map(|entry| entry.signed_bytes)
        .collect::<Vec<_>>();
    assert!(receipt.iter().any(|bytes| bytes == &lookalike.signed_bytes));
    assert_eq!(
        verify_enrollment_device_add(
            &receipt,
            account,
            AccountEntryHash::from_bytes(device_add_hash),
            &device_add_bytes,
            joiner.ed,
            joiner.x,
        )
        .unwrap(),
        genesis_hash.into(),
        "the fold's accepted current-version genesis wins over an opaque parked lookalike",
    );
}

struct InterleavedBootstrap {
    account: AccountId,
    genesis_hash: AccountEntryHash,
    device_b: Dev,
    b0_hash: [u8; 32],
    joiner: Dev,
    add_joiner_hash: [u8; 32],
    account_entries: Vec<Vec<u8>>,
}

/// A bootstrap whose raw `(log_id, seq, entry_hash)` receipt order interleaves seq-0 control
/// entries from two promoted devices BEFORE the founder-chain DeviceAdds introducing their
/// keys — the ordering that forces simultaneous pre-verify parking when ingested raw (#945).
fn interleaved_promoted_device_bootstrap() -> InterleavedBootstrap {
    let founder = Dev::new(0x61);
    let device_b = Dev::new(0x62);
    let device_c = Dev::new(0x63);
    let joiner = Dev::new(0x64);
    let (account, genesis_bytes, genesis_hash) = genesis(&founder);
    let (add_b_bytes, add_b_hash) = op(
        account,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&device_b, DeviceRole::Member),
    );
    let (add_c_bytes, add_c_hash) = op(
        account,
        &founder,
        2,
        Some(add_b_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&device_c, DeviceRole::Member),
    );
    let (add_joiner_bytes, add_joiner_hash) = op(
        account,
        &founder,
        3,
        Some(add_c_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&joiner, DeviceRole::Member),
    );
    let (_stream, own_op) = test_support::stream_own_public(account);
    let (b0_bytes, b0_hash) = op(account, &device_b, 0, None, None, &own_op);
    let (c0_bytes, c0_hash) = op(account, &device_c, 0, None, None, &own_op);
    // The receipt order, exactly as `account_entries_for_sync` emits it.
    let mut ordered = vec![
        (0u64, genesis_hash, genesis_bytes),
        (0, b0_hash, b0_bytes),
        (0, c0_hash, c0_bytes),
        (1, add_b_hash, add_b_bytes),
        (2, add_c_hash, add_c_bytes),
        (3, add_joiner_hash, add_joiner_bytes),
    ];
    ordered.sort_by_key(|(seq, hash, _)| (*seq, *hash));
    InterleavedBootstrap {
        account,
        genesis_hash: AccountEntryHash::from_bytes(genesis_hash),
        device_b,
        b0_hash,
        joiner,
        add_joiner_hash,
        account_entries: ordered.into_iter().map(|(_, _, bytes)| bytes).collect(),
    }
}

fn park_fillers(conn: &Connection, account: AccountId, count: i64) {
    park_fillers_for_device(conn, account, Dev::new(9).fp, 0, count);
}

fn park_fillers_for_device(
    conn: &Connection,
    account: AccountId,
    fingerprint: DeviceFingerprint,
    first_ordinal: i64,
    count: i64,
) {
    for ordinal in 0..count {
        let ordinal = first_ordinal + ordinal;
        let hash = cbor::sha256(&ordinal.to_be_bytes());
        conn.execute(
            "INSERT INTO account_pre_verify(
                     signed_hash, entry_hash, claimed_account_id, claimed_fingerprint, raw_bytes,
                     received_at_ms)
                 VALUES (?1, ?2, ?3, ?4, X'00', ?5)",
            params![
                hash.as_slice(),
                hash.as_slice(),
                account.to_bytes().as_slice(),
                fingerprint.to_bytes().as_slice(),
                NOW,
            ],
        )
        .unwrap();
    }
}

#[test]
fn raw_order_bootstrap_ingestion_parks_promoted_device_entries_simultaneously() {
    // Witness for the failure the causal ingest order fixes: with one free pre-verify slot,
    // the receipt's raw order evicts on the second promoted-device entry.
    let fixture = interleaved_promoted_device_bootstrap();
    let joiner_db = db();
    park_fillers(&joiner_db, fixture.account, PRE_VERIFY_PER_ACCOUNT_MAX as i64 - 1);
    let tx = Transaction::new_unchecked(&joiner_db, TransactionBehavior::Immediate).unwrap();
    let mut evicted = false;
    for bytes in &fixture.account_entries {
        if matches!(
            account_ingest_in_tx(&tx, bytes, NOW + 1).unwrap(),
            IngestOutcome::PreVerifyWithEviction { .. }
        ) {
            evicted = true;
            break;
        }
    }
    tx.rollback().unwrap();
    assert!(evicted, "raw order needs two simultaneous pre-verify slots for this bootstrap");
}

#[test]
fn a_bootstrap_interleaving_promoted_device_entries_adopts_in_causal_order() {
    use super::super::bootstrap::{EnrollmentBootstrap, adopt_enrollment_bootstrap};

    let fixture = interleaved_promoted_device_bootstrap();
    let joiner_db = db();
    park_fillers(&joiner_db, fixture.account, PRE_VERIFY_PER_ACCOUNT_MAX as i64 - 1);
    adopt_enrollment_bootstrap(&joiner_db, EnrollmentBootstrap {
        account_entries: &fixture.account_entries,
        account_id: fixture.account,
        genesis_hash: fixture.genesis_hash,
        device_fingerprint: fixture.joiner.fp,
        device_add_hash: AccountEntryHash::from_bytes(fixture.add_joiner_hash),
        now_ms: NOW + 1,
    })
    .expect("causal-order ingestion adopts without simultaneous parking");
    assert_eq!(
        super::super::bootstrap::read_local_account(&joiner_db).unwrap(),
        Some(fixture.account),
    );
    let effective: bool = joiner_db
        .query_row(
            "SELECT EXISTS(
                     SELECT 1 FROM account_roster_history
                      WHERE account_id = ?1 AND roster_ref = ?2 AND closed_at IS NULL
                 )",
            params![fixture.account.to_bytes().as_slice(), fixture.add_joiner_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(effective, "the acknowledged DeviceAdd is roster-effective after adoption");
}

#[test]
fn enrollment_bootstrap_staging_defers_projection_until_finish() {
    let fixture = interleaved_promoted_device_bootstrap();
    let joiner_db = db();
    let tx = Transaction::new_unchecked(&joiner_db, TransactionBehavior::Immediate).unwrap();

    let mut pending: Vec<&Vec<u8>> = fixture.account_entries.iter().collect();
    let mut resolved = HashMap::new();
    while !pending.is_empty() {
        // Extract the first causal candidate without an indexed-removal panic diagnostic.
        let bytes = pending
            .extract_if(.., |bytes| {
                let signed = envelope::decode_account_signed(bytes).unwrap();
                resolved.contains_key(&signed.header.device_fingerprint)
                    || self_certifies_signer(&signed.header, &signed.payload)
            })
            .next()
            .expect("candidate snapshot has a causal authentication root");
        let signed = envelope::decode_account_signed(bytes).unwrap();
        let signer = resolved.get(&signed.header.device_fingerprint).copied();
        stage_enrollment_bootstrap_entry_in_tx(&tx, bytes, signer, NOW + 1).unwrap();
        add_self_pubkey(&mut resolved, &signed.header, &signed.payload);
    }
    let projected_before_finish: i64 = tx
        .query_row(
            "SELECT COUNT(*)
                   FROM account_entry_status s
                   JOIN account_entries e ON e.entry_hash = s.entry_hash
                  WHERE e.account_id = ?1",
            [fixture.account.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(projected_before_finish, 0, "staging must not refold partial receipts");

    finish_enrollment_bootstrap_in_tx(&tx, fixture.account, NOW + 1).unwrap();
    let projected_after_finish: i64 = tx
        .query_row(
            "SELECT COUNT(*)
                   FROM account_entry_status s
                   JOIN account_entries e ON e.entry_hash = s.entry_hash
                  WHERE e.account_id = ?1",
            [fixture.account.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        projected_after_finish as usize,
        fixture.account_entries.len(),
        "one final reconciliation projects the complete receipt"
    );
    tx.rollback().unwrap();
}

#[test]
fn enrollment_receipt_wins_capacity_over_rows_unlocked_by_the_receipt() {
    use super::super::bootstrap::{EnrollmentBootstrap, adopt_enrollment_bootstrap};

    let fixture = interleaved_promoted_device_bootstrap();
    let joiner_db = db();
    let (_, own_op) = test_support::stream_own_public(fixture.account);
    let (latent_bytes, latent_hash) =
        op(fixture.account, &fixture.device_b, 1, Some(fixture.b0_hash), None, &own_op);
    let latent_signed_hash = cbor::sha256(&latent_bytes);
    joiner_db
        .execute(
            "INSERT INTO account_pre_verify(
                     signed_hash, entry_hash, claimed_account_id, claimed_fingerprint, raw_bytes,
                     received_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                latent_signed_hash.as_slice(),
                latent_hash.as_slice(),
                fixture.account.to_bytes().as_slice(),
                fixture.device_b.fp.to_bytes().as_slice(),
                latent_bytes,
                NOW,
            ],
        )
        .unwrap();
    seed_global_candidate_rows(
        &joiner_db,
        CANDIDATES_GLOBAL_MAX - fixture.account_entries.len(),
        10_000,
    );

    adopt_enrollment_bootstrap(&joiner_db, EnrollmentBootstrap {
        account_entries: &fixture.account_entries,
        account_id: fixture.account,
        genesis_hash: fixture.genesis_hash,
        device_fingerprint: fixture.joiner.fp,
        device_add_hash: AccountEntryHash::from_bytes(fixture.add_joiner_hash),
        now_ms: NOW + 1,
    })
    .expect("pre-existing parked work must not displace the one-time receipt");
    // The production caller runs this best-effort maintenance after adoption; here it is the
    // behavior under test.
    super::super::authoring::retry_enrollment_pre_verify(&joiner_db, fixture.account, NOW + 1)
        .unwrap();

    for bytes in &fixture.account_entries {
        let (_, entry_hash) = account_entry_ref(bytes).unwrap();
        let held: bool = joiner_db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM account_entries WHERE entry_hash = ?1)",
                [entry_hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(held, "every receipt entry wins admission");
    }
    let latent_held: bool = joiner_db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM account_entries WHERE entry_hash = ?1)",
            [latent_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!latent_held, "the later latent promotion cannot exceed the terminal cap");
    let latent_parked: bool = joiner_db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM account_pre_verify WHERE signed_hash = ?1)",
            [latent_signed_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!latent_parked, "terminally capacity-blocked queue work is removed");
}

#[test]
fn enrollment_budget_reports_raw_headroom_and_clamps_at_zero() {
    let conn = db();
    let account = AccountId::from_bytes([7; 32]);
    let local_fp = crate::local_device(&conn, NOW).unwrap().fingerprint();
    let budget = super::super::bootstrap::enrollment_budget(&conn, account).unwrap();
    // The ORDINARY cap: an enrollment receipt is ordinary traffic, so the preflight measures it
    // against the budget above the view-manifest floor — the one admission will apply to it.
    assert_eq!(budget.account_entries_remaining as usize, ORDINARY_CANDIDATES_PER_ACCOUNT_MAX);
    assert_eq!(budget.global_entries_remaining as usize, CANDIDATES_GLOBAL_MAX);

    // 100 one-byte held candidates plus parked rows that are malformed or claim an unrelated
    // signer. None can promote after the local DeviceAdd, so they consume no candidate
    // headroom. Held candidates are not credited; the request proves their hashes separately.
    seed_candidate_rows(&conn, account, Dev::new(9).fp, 42, 100);
    park_fillers_for_device(&conn, account, local_fp, 0, 5);
    park_fillers_for_device(&conn, account, Dev::new(9).fp, 100, 5);
    for (ordinal, fingerprint) in [(0u8, local_fp), (1, Dev::new(9).fp)] {
        conn.execute(
            "INSERT INTO content_pre_verify(
                     signed_hash, entry_hash, claimed_stream_id, claimed_author_account_id,
                     claimed_fingerprint, roster_ref, raw_bytes, received_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, X'00', ?7)",
            params![
                [0xa0 + ordinal; 32].as_slice(),
                [0xb0 + ordinal; 32].as_slice(),
                [0xc0 + ordinal; 32].as_slice(),
                account.to_bytes().as_slice(),
                fingerprint.to_bytes().as_slice(),
                [0xd0 + ordinal; 32].as_slice(),
                NOW,
            ],
        )
        .unwrap();
    }
    let budget = super::super::bootstrap::enrollment_budget(&conn, account).unwrap();
    assert_eq!(
        budget.account_entries_remaining as usize,
        ORDINARY_CANDIDATES_PER_ACCOUNT_MAX - 100
    );
    assert_eq!(budget.global_entries_remaining as usize, CANDIDATES_GLOBAL_MAX - 100);
    assert_eq!(
        budget.account_bytes_remaining as usize,
        ORDINARY_CANDIDATE_BYTES_PER_ACCOUNT_MAX - 100
    );
    assert_eq!(budget.global_bytes_remaining as usize, CANDIDATE_BYTES_GLOBAL_MAX - 100);

    // Past the grow-only caps the budget clamps at zero rather than wrapping.
    let clamped = db();
    seed_global_candidate_rows(&clamped, CANDIDATES_GLOBAL_MAX + 10, 1_000);
    let budget = super::super::bootstrap::enrollment_budget(&clamped, account).unwrap();
    assert_eq!(budget.global_entries_remaining, 0, "past-cap headroom clamps to zero");
}

#[test]
fn enrollment_budget_does_not_reserve_non_fatal_pre_verify_promotions() {
    let conn = db();
    let local = Dev::new(0x31);
    let child = Dev::new(0x32);
    conn.execute(
        "INSERT INTO oplog_device_identity(id, seed, public_key, fingerprint, created_at_ms)
             VALUES (0, ?1, ?2, ?3, ?4)",
        params![[0x31u8; 32].as_slice(), local.ed.as_slice(), local.fp.to_bytes().as_slice(), NOW,],
    )
    .unwrap();
    let (account, _, genesis_hash) = genesis(&local);
    let (add_child_bytes, add_child_hash) = op(
        account,
        &local,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&child, DeviceRole::Member),
    );
    let (_, own_op) = test_support::stream_own_public(account);
    let (child_bytes, child_hash) = op(account, &child, 0, None, None, &own_op);
    for (raw, entry_hash, fingerprint) in
        [(add_child_bytes, add_child_hash, local.fp), (child_bytes, child_hash, child.fp)]
    {
        let signed_hash = cbor::sha256(&raw);
        conn.execute(
            "INSERT INTO account_pre_verify(
                     signed_hash, entry_hash, claimed_account_id, claimed_fingerprint, raw_bytes,
                     received_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                signed_hash.as_slice(),
                entry_hash.as_slice(),
                account.to_bytes().as_slice(),
                fingerprint.to_bytes().as_slice(),
                raw,
                NOW,
            ],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO content_pre_verify(
                 signed_hash, entry_hash, claimed_stream_id, claimed_author_account_id,
                 claimed_fingerprint, roster_ref, raw_bytes, received_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, X'00', ?7)",
        params![
            [0xe1u8; 32].as_slice(),
            [0xe2u8; 32].as_slice(),
            [0xe3u8; 32].as_slice(),
            account.to_bytes().as_slice(),
            child.fp.to_bytes().as_slice(),
            add_child_hash.as_slice(),
            NOW,
        ],
    )
    .unwrap();

    let budget = super::super::bootstrap::enrollment_budget(&conn, account).unwrap();
    // Account and content retries happen only after every receipt entry wins admission, so
    // they cannot roll one-time adoption back even when they form a valid transitive closure.
    assert_eq!(budget.account_entries_remaining as usize, ORDINARY_CANDIDATES_PER_ACCOUNT_MAX);
    assert_eq!(budget.global_entries_remaining as usize, CANDIDATES_GLOBAL_MAX);
}

#[test]
fn a_real_wrap_entry_fits_the_enrollment_preflight_bound() {
    let conn = db();
    let _account = super::super::bootstrap::local_account(&conn, NOW).unwrap();
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let stream = super::super::authoring::ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW).unwrap();
    super::super::secrets::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW).unwrap();
    tx.commit().unwrap();
    let wrap_len: i64 = conn
        .query_row("SELECT length(signed_bytes) FROM account_entries WHERE log_id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    let bound = super::super::secrets::single_recipient_wrap_envelope_bytes();
    assert!(
        wrap_len as usize <= bound,
        "a real wrap entry ({wrap_len} bytes) exceeds the charged bound ({bound})"
    );
    // The bound is TIGHT: an account with 256 live key targets must fit the per-account
    // byte budget — the old per-entry §18a-maximum charge refused this outright.
    let device_add =
        super::super::authoring::device_add_envelope_bytes(DeviceRole::Member, None).unwrap();
    assert!(
        256 * bound + device_add <= ORDINARY_CANDIDATE_BYTES_PER_ACCOUNT_MAX,
        "256 live targets fit the per-account byte budget"
    );
}

#[test]
fn enrollment_authoring_fits_gates_the_invite_boundary_on_candidate_headroom() {
    // A fresh store with no owned streams fits the lone DeviceAdd redemption authors.
    let conn = db();
    let account = super::super::bootstrap::local_account(&conn, NOW).unwrap();
    super::super::authoring::enrollment_authoring_fits(
        &conn,
        account,
        &[],
        DeviceRole::Member,
        Some("laptop"),
    )
    .unwrap();

    // Saturate the account's grow-only candidate budget so not even the DeviceAdd fits:
    // minting here would distribute a permanently unredeemable ticket.
    seed_candidate_rows(
        &conn,
        account,
        Dev::new(9).fp,
        42,
        ORDINARY_CANDIDATES_PER_ACCOUNT_MAX - 1,
    );
    let error = super::super::authoring::enrollment_authoring_fits(
        &conn,
        account,
        &[],
        DeviceRole::Member,
        Some("laptop"),
    )
    .expect_err("a store without headroom for the DeviceAdd itself must refuse");
    assert!(error.to_string().contains("unredeemable"), "unexpected error: {error}");

    // Leave exactly one slot for the mandatory DeviceAdd. Opaque parked work is retried only
    // after enrollment commits and therefore cannot make this exact preflight refuse.
    let conn = db();
    let account = super::super::bootstrap::local_account(&conn, NOW).unwrap();
    seed_candidate_rows(
        &conn,
        account,
        Dev::new(8).fp,
        43,
        ORDINARY_CANDIDATES_PER_ACCOUNT_MAX - 2,
    );
    conn.execute(
        "INSERT INTO account_pre_verify(
                 signed_hash, entry_hash, claimed_account_id, claimed_fingerprint, raw_bytes,
                 received_at_ms)
             VALUES (?1, ?2, ?3, ?4, X'00', ?5)",
        params![
            [0xe1u8; 32].as_slice(),
            [0xe2u8; 32].as_slice(),
            account.to_bytes().as_slice(),
            [0xe3u8; 32].as_slice(),
            NOW,
        ],
    )
    .unwrap();
    super::super::authoring::enrollment_authoring_fits(
        &conn,
        account,
        &[],
        DeviceRole::Member,
        None,
    )
    .expect("latent pre-verify work is not part of mandatory enrollment capacity");
}

#[test]
fn enrollment_bootstrap_finish_leaves_parked_rows_for_post_commit_retry() {
    let fixture = interleaved_promoted_device_bootstrap();
    let joiner_db = db();
    let tx = Transaction::new_unchecked(&joiner_db, TransactionBehavior::Immediate).unwrap();
    let mut pending: Vec<&Vec<u8>> = fixture.account_entries.iter().collect();
    let mut resolved = HashMap::new();
    while !pending.is_empty() {
        // Extract the first causal candidate without an indexed-removal panic diagnostic.
        let bytes = pending
            .extract_if(.., |bytes| {
                let signed = envelope::decode_account_signed(bytes).unwrap();
                resolved.contains_key(&signed.header.device_fingerprint)
                    || self_certifies_signer(&signed.header, &signed.payload)
            })
            .next()
            .expect("candidate snapshot has a causal authentication root");
        let signed = envelope::decode_account_signed(bytes).unwrap();
        let signer = resolved.get(&signed.header.device_fingerprint).copied();
        stage_enrollment_bootstrap_entry_in_tx(&tx, bytes, signer, NOW + 1).unwrap();
        add_self_pubkey(&mut resolved, &signed.header, &signed.payload);
    }

    // A valid parked row whose signer the receipt certifies (the founder) must NOT enter the
    // one-time adoption fold: a parked sibling DeviceAdd for the joining fingerprint could
    // otherwise win branch selection and roll the acknowledged enrollment back after the
    // owner already consumed the nonce.
    let founder = Dev::new(0x61);
    let (sibling_bytes, sibling_hash) = op(
        fixture.account,
        &founder,
        1,
        Some(fixture.genesis_hash.into()),
        Some(fixture.genesis_hash.into()),
        &device_add(&fixture.joiner, DeviceRole::Owner),
    );
    let sibling_signed_hash = cbor::sha256(&sibling_bytes);
    tx.execute(
        "INSERT INTO account_pre_verify(
                 signed_hash, entry_hash, claimed_account_id, claimed_fingerprint, raw_bytes,
                 received_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            sibling_signed_hash.as_slice(),
            sibling_hash.as_slice(),
            fixture.account.to_bytes().as_slice(),
            founder.fp.to_bytes().as_slice(),
            sibling_bytes,
            NOW,
        ],
    )
    .unwrap();

    finish_enrollment_bootstrap_in_tx(&tx, fixture.account, NOW + 1).unwrap();
    let still_parked: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM account_pre_verify WHERE signed_hash = ?1)",
            [sibling_signed_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(still_parked, "the one-time adoption fold must not promote parked rows");
    let promoted: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM account_entries WHERE entry_hash = ?1)",
            [sibling_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!promoted, "parked siblings enter the fold only after adoption commits");
}

#[test]
fn adopt_local_account_adopts_an_accepted_genesis_and_refuses_conflicts() {
    let fixture = interleaved_promoted_device_bootstrap();
    let conn = db();
    // Ingest the bootstrap as ordinary candidates (the genesis folds accepted among them —
    // `account_entries` is (seq, hash)-sorted, so no fixed index names the genesis), then
    // adopt it directly.
    for bytes in &fixture.account_entries {
        account_ingest(&conn, bytes, NOW).unwrap();
    }
    super::super::bootstrap::adopt_local_account(
        &conn,
        fixture.account,
        fixture.genesis_hash,
        NOW + 1,
    )
    .unwrap();
    assert_eq!(super::super::bootstrap::read_local_account(&conn).unwrap(), Some(fixture.account));
    // Idempotent for the same account…
    super::super::bootstrap::adopt_local_account(
        &conn,
        fixture.account,
        fixture.genesis_hash,
        NOW + 2,
    )
    .unwrap();
    // …but a store can never change identity.
    let (other_account, _, other_genesis_hash) = genesis(&Dev::new(0x77));
    let error = super::super::bootstrap::adopt_local_account(
        &conn,
        other_account,
        AccountEntryHash::from_bytes(other_genesis_hash),
        NOW + 3,
    )
    .expect_err("a second account identity must be refused");
    assert!(error.to_string().contains("another local account"), "unexpected error: {error}");
    // An unaccepted genesis hash cannot be adopted either.
    let stranger = db();
    let error = super::super::bootstrap::adopt_local_account(
        &stranger,
        fixture.account,
        AccountEntryHash::from_bytes([0xfe; 32]),
        NOW,
    )
    .expect_err("adopting an unknown genesis must fail");
    assert!(error.to_string().contains("not accepted"), "unexpected error: {error}");
}

#[test]
fn enrollment_bootstrap_rejects_an_entry_whose_signer_the_snapshot_never_certifies() {
    let fixture = interleaved_promoted_device_bootstrap();
    let stranger = Dev::new(0x7f);
    let (_stream, own_op) = test_support::stream_own_public(fixture.account);
    let (stranger_bytes, _) = op(fixture.account, &stranger, 0, None, None, &own_op);
    let mut entries: Vec<Vec<u8>> = fixture.account_entries.iter().take(2).cloned().collect();
    entries.push(stranger_bytes);

    let joiner_db = db();
    let error = super::super::bootstrap::adopt_enrollment_bootstrap(
        &joiner_db,
        super::super::bootstrap::EnrollmentBootstrap {
            account_entries: &entries,
            account_id: fixture.account,
            genesis_hash: fixture.genesis_hash,
            device_fingerprint: fixture.joiner.fp,
            device_add_hash: AccountEntryHash::from_bytes(fixture.add_joiner_hash),
            now_ms: NOW + 1,
        },
    )
    .expect_err("an entry no snapshot key certifies must refuse the bootstrap");
    assert!(error.to_string().contains("not certified"), "unexpected error: {error}");
    let stored: i64 =
        joiner_db.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(stored, 0, "the failed adoption rolls every staged entry back");
}

#[test]
fn enrollment_bootstrap_refuses_to_replace_a_conflicting_local_account() {
    let fixture = interleaved_promoted_device_bootstrap();
    let joiner_db = db();
    let _other_account = super::super::bootstrap::local_account(&joiner_db, NOW).unwrap();
    let error = super::super::bootstrap::adopt_enrollment_bootstrap(
        &joiner_db,
        super::super::bootstrap::EnrollmentBootstrap {
            account_entries: &fixture.account_entries,
            account_id: fixture.account,
            genesis_hash: fixture.genesis_hash,
            device_fingerprint: fixture.joiner.fp,
            device_add_hash: AccountEntryHash::from_bytes(fixture.add_joiner_hash),
            now_ms: NOW + 1,
        },
    )
    .expect_err("adopting a second account identity must refuse");
    assert!(error.to_string().contains("another local account"), "unexpected error: {error}");
}

#[test]
fn reservation_upsert_updates_and_release_is_idempotent() {
    let conn = db();
    let account = super::super::bootstrap::local_account(&conn, NOW).unwrap();
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    super::super::bootstrap::upsert_account_candidate_reservation_in_tx(
        &tx,
        account,
        [0x53; 32],
        1,
        100,
        0,
        NOW + 100,
    )
    .unwrap();
    super::super::bootstrap::upsert_account_candidate_reservation_in_tx(
        &tx,
        account,
        [0x53; 32],
        5,
        500,
        4,
        NOW + 200,
    )
    .unwrap();
    let (entries, bytes, targets, expiry): (i64, i64, i64, i64) = tx
        .query_row(
            "SELECT reserved_entries, reserved_bytes, reserved_targets, expires_at_ms
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
            [[0x53; 32].as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((entries, bytes, targets, expiry), (5, 500, 4, NOW + 200));
    super::super::bootstrap::release_account_candidate_reservation_in_tx(&tx, [0x53; 32]).unwrap();
    super::super::bootstrap::release_account_candidate_reservation_in_tx(&tx, [0x53; 32]).unwrap();
    tx.commit().unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM account_candidate_reservations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "releasing twice is a no-op, not an error");
}

/// Reserve two live key targets for `account` under `expires_at_ms`. The shape (3 entries /
/// 100_000 bytes / 2 targets) is deliberately larger than the fixture's real live target set, so a
/// top-up that runs is visible as a shrink and one that is skipped leaves the row verbatim.
fn reserve_two_targets(
    conn: &Connection,
    account: AccountId,
    reservation_id: [u8; 32],
    expires_at_ms: i64,
) {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
    super::super::bootstrap::upsert_account_candidate_reservation_in_tx(
        &tx,
        account,
        reservation_id,
        3,
        100_000,
        2,
        expires_at_ms,
    )
    .unwrap();
    tx.commit().unwrap();
}

fn reservation_row(conn: &Connection, reservation_id: [u8; 32]) -> (i64, i64, i64) {
    conn.query_row(
        "SELECT reserved_entries, reserved_bytes, reserved_targets
               FROM account_candidate_reservations WHERE reservation_id = ?1",
        [reservation_id.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

#[test]
fn invite_reservations_shrink_when_the_live_target_set_shrinks() {
    let conn = db();
    let account = super::super::bootstrap::local_account(&conn, NOW).unwrap();
    // A reservation recorded when two key targets were live must not keep their capacity
    // after a fold reduces the live set: the top-up is bidirectional. The expiry is WALL-CLOCK
    // live, which is what makes the invite outstanding at all — `NOW` is a fixed past instant, so
    // a TTL near it is expired and the top-up would correctly skip the row entirely.
    reserve_two_targets(&conn, account, [0x52; 32], rag_rat_base::time::now_ms() + 3_600_000);

    refold_account(&conn, account).unwrap();
    let (entries, bytes, targets) = reservation_row(&conn, [0x52; 32]);
    assert_eq!(targets, 0, "no live key targets remain");
    assert_eq!(entries, 1, "only the mandatory DeviceAdd stays reserved");
    assert!(bytes < 100_000, "the reclaimed wrap bytes return to the shared headroom");
}

/// An invite TTL is wall-clock, but `refold_account` folds with `coalesce(max(received_at_ms), 0)`
/// — the newest entry's ARRIVAL. Any reservation minted after that arrival outranks the coordinate
/// forever, so a long-lapsed invite would read as outstanding and keep reserving headroom against a
/// ticket nothing can redeem (#1362).
///
/// Asserts a NEGATIVE — the row is left untouched — so a top-up that never runs at all also
/// satisfies it. `invite_reservations_shrink_when_the_live_target_set_shrinks` and the two
/// wall-clock-live enrollment tests in `rag-rat-sync`
/// (`new_mandatory_key_targets_grow_an_outstanding_invites_reservation`,
/// `synced_key_target_growth_tops_up_the_outstanding_reservation`) are what prove the top-up
/// ever fires. Deleting those believing this pair covers the ground leaves a dead top-up
/// uncaught.
#[test]
fn a_lapsed_reservation_is_not_outstanding_under_a_stale_fold_clock() {
    let conn = db();
    let account = super::super::bootstrap::local_account(&conn, NOW).unwrap();
    // Expires just after the fixture's arrival stamp, and years before the real wall clock.
    reserve_two_targets(&conn, account, [0x54; 32], NOW + 1_000);

    refold_account(&conn, account).unwrap();

    assert_eq!(
        reservation_row(&conn, [0x54; 32]),
        (3, 100_000, 2),
        "a wall-clock-expired reservation is not outstanding, so the top-up leaves it untouched",
    );
}

/// The same rule for the migration backfill, which replays every account with a literal `0` clock.
/// The expiry comparison must not inherit that coordinate either — but note the backfill's `0` has
/// to stay a replay coordinate for the authority projection it rewrites, which is why the wall
/// clock is read inside the top-up rather than supplied by this caller.
///
/// Asserts a NEGATIVE — the row is left untouched — so a top-up that never runs at all also
/// satisfies it. `invite_reservations_shrink_when_the_live_target_set_shrinks` and the two
/// wall-clock-live enrollment tests in `rag-rat-sync`
/// (`new_mandatory_key_targets_grow_an_outstanding_invites_reservation`,
/// `synced_key_target_growth_tops_up_the_outstanding_reservation`) are what prove the top-up
/// ever fires. Deleting those believing this pair covers the ground leaves a dead top-up
/// uncaught.
#[test]
fn a_lapsed_reservation_is_not_outstanding_under_the_migration_backfill_clock() {
    let conn = db();
    let account = super::super::bootstrap::local_account(&conn, NOW).unwrap();
    reserve_two_targets(&conn, account, [0x55; 32], NOW + 1_000);

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    backfill_authority_projection(&tx).unwrap();
    tx.commit().unwrap();

    assert_eq!(
        reservation_row(&conn, [0x55; 32]),
        (3, 100_000, 2),
        "the backfill's zero clock must not make a lapsed reservation outstanding",
    );
}

#[test]
fn enrollment_reservations_reserve_candidate_capacity_until_consumed_or_expired() {
    let conn = db();
    let account = super::super::bootstrap::local_account(&conn, NOW).unwrap();
    let fits = || {
        super::super::authoring::enrollment_authoring_fits(
            &conn,
            account,
            &[],
            DeviceRole::Member,
            Some("laptop"),
        )
    };
    fits().unwrap();

    // An outstanding invite's reservation consumes the same grow-only counters ordinary
    // ingest enforces, so the remaining headroom no longer fits another redemption. The charging
    // paths judge expiry against the wall clock (#1362), so the TTL is wall-clock live and this
    // test moves the ROW's expiry rather than walking a caller's clock across it.
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    super::super::bootstrap::upsert_account_candidate_reservation_in_tx(
        &tx,
        account,
        [0x51; 32],
        (CANDIDATES_PER_ACCOUNT_MAX - 1) as u64,
        0,
        0,
        rag_rat_base::time::now_ms() + 3_600_000,
    )
    .unwrap();
    tx.commit().unwrap();
    let error = fits().expect_err("reserved headroom is not available to a new mint");
    assert!(error.to_string().contains("unredeemable"), "unexpected error: {error}");

    // Expiry frees the reservation (the counters filter on `expires_at_ms > now`; pruning only
    // keeps the table bounded), and redemption's release removes the row outright.
    conn.execute(
        "UPDATE account_candidate_reservations SET expires_at_ms = ?1 WHERE reservation_id = ?2",
        params![rag_rat_base::time::now_ms() - 1_000, [0x51u8; 32].as_slice()],
    )
    .unwrap();
    fits().expect("an expired reservation frees its capacity");
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    super::super::bootstrap::prune_account_candidate_reservations_in_tx(
        &tx,
        rag_rat_base::time::now_ms(),
    )
    .unwrap();
    tx.commit().unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM account_candidate_reservations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "pruning removes expired reservation rows");
}

#[test]
fn stream_access_mode_reads_public_and_private_and_fails_closed_on_unknown() {
    let conn = db();
    let founder = Dev::new(1);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    // A public_read stream: the accessor decodes PublicRead from the folded StreamOwn spec.
    let (public_stream, public_op) =
        test_support::stream_own_mode(account_id, AccessMode::PublicRead, "repo-pub");
    let (public_bytes, public_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &public_op,
    );
    account_ingest(&conn, &public_bytes, NOW + 1).unwrap();
    assert_eq!(
        stream_access_mode(&conn, account_id, public_stream).unwrap(),
        AccessMode::PublicRead,
    );

    // A private stream on the same account resolves Private — and has a DISTINCT id, since the
    // mode folds into the stream identity.
    let (private_stream, private_op) =
        test_support::stream_own_mode(account_id, AccessMode::Private, "repo-priv");
    let (private_bytes, _) = op(
        account_id,
        &founder,
        2,
        Some(public_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &private_op,
    );
    account_ingest(&conn, &private_bytes, NOW + 2).unwrap();
    assert_ne!(public_stream, private_stream);
    assert_eq!(stream_access_mode(&conn, account_id, private_stream).unwrap(), AccessMode::Private,);

    // A stream with no folded ownership fact fails closed to Private — never public — so an
    // unknown or not-yet-synced owner opens nothing at an admission caller.
    let unknown = StreamId::from_bytes([0x77; 32]);
    assert_eq!(stream_access_mode(&conn, account_id, unknown).unwrap(), AccessMode::Private);
}

#[test]
fn account_is_fully_public_gates_on_every_stream_own_being_public() {
    let conn = db();
    let founder = Dev::new(1);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    // No StreamOwn yet — vacuously fully public.
    assert!(account_is_fully_public(&conn, account_id).unwrap());

    // One public StreamOwn — still fully public.
    let (_pub_stream, public_op) =
        test_support::stream_own_mode(account_id, AccessMode::PublicRead, "repo-pub");
    let (public_bytes, public_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &public_op,
    );
    account_ingest(&conn, &public_bytes, NOW + 1).unwrap();
    assert!(account_is_fully_public(&conn, account_id).unwrap());

    // Add a private StreamOwn — no longer fully public, so anonymous serving must be refused.
    let (_priv_stream, private_op) =
        test_support::stream_own_mode(account_id, AccessMode::Private, "repo-priv");
    let (private_bytes, _) = op(
        account_id,
        &founder,
        2,
        Some(public_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &private_op,
    );
    account_ingest(&conn, &private_bytes, NOW + 2).unwrap();
    assert!(!account_is_fully_public(&conn, account_id).unwrap());
}

fn device_remove(dev: &Dev, control_cut: super::super::cut::Cut) -> AccountOp {
    AccountOp::DeviceRemove {
        device_fingerprint: dev.fp,
        control_cut,
        secrets_cut: super::super::cut::Cut::Empty,
        content_cuts: Vec::new(),
        reason: "revoked".to_string(),
    }
}

fn projected_nodes(conn: &Connection, stream_id: StreamId) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT node_id FROM content_projected_nodes
                 WHERE stream_id = ?1 ORDER BY node_id",
        )
        .unwrap();
    stmt.query_map([stream_id.to_bytes().as_slice()], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn content_verdict(conn: &Connection, entry_hash: &AccountEntryHash) -> (String, i64) {
    conn.query_row(
        "SELECT s.status, e.accepted FROM content_entries e
             JOIN content_entry_status s ON s.entry_hash = e.entry_hash
             WHERE e.entry_hash = ?1",
        [entry_hash.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

fn signed_member_content(
    member: &Dev,
    account_id: AccountId,
    stream_id: StreamId,
    roster_ref: RosterRef,
    auth_len: u64,
) -> crate::account::content::SignedContentEntry {
    let header = ContentEntryHeader {
        stream_id,
        author_account_id: account_id,
        device_fingerprint: member.fp,
        seq: 0,
        lamport: 1,
        prev_hash: None,
        grant_id: None,
        roster_ref,
        owner_auth_len: auth_len,
        author_auth_len: auth_len,
        crypto_suite: 0,
        key_id: None,
    };
    let op = crate::op::MemoryOp::NodeCreate {
        node_id: crate::op::NodeId::from("remote-node"),
        content: crate::op::NodeContent {
            kind: "Invariant".into(),
            title: "remote node".into(),
            body: "body".into(),
            confidence: "high".into(),
            source: "agent".into(),
            tags: Vec::new(),
            payload: None,
        },
    };
    sign_content_entry(&member.secret, &header, &crate::op::encode(&op)).unwrap()
}

fn owner_demote(dev: &Dev, owner_id: OwnerId) -> AccountOp {
    AccountOp::OwnerDemote {
        device_fingerprint: dev.fp,
        owner_id,
        control_cut: super::super::cut::Cut::Empty,
        secrets_cut: super::super::cut::Cut::Empty,
        reason: "demoted".to_string(),
    }
}

fn cut_extend_ctrl(
    account_id: AccountId,
    subject: &Dev,
    new_seq: u64,
    new_entry_hash: AccountEntryHash,
) -> AccountOp {
    AccountOp::CutExtend {
        chain_kind: super::super::ops::ChainKind::Ctrl,
        stream_id: None,
        incarnation_id: None,
        subject_account_id: account_id,
        device_fingerprint: subject.fp,
        new_seq,
        new_entry_hash,
    }
}

fn status(conn: &Connection, hash: &[u8; 32]) -> Option<String> {
    entry_status(conn, &(*(hash)).into()).unwrap().map(|(s, _)| s)
}

fn seed_candidate_rows(
    conn: &Connection,
    account_id: AccountId,
    fingerprint: DeviceFingerprint,
    fixture_namespace: u64,
    count: usize,
) {
    for ordinal in 0..count {
        let entry_hash = cbor::sha256(
            &[
                fixture_namespace.to_be_bytes().as_slice(),
                u64::try_from(ordinal).unwrap().to_be_bytes().as_slice(),
            ]
            .concat(),
        );
        conn.execute(
            "INSERT INTO account_entries(
                     entry_hash, account_id, log_id, device_fingerprint, seq, entry_type,
                     accepted, signed_bytes, received_at_ms)
                 VALUES (?1, ?2, 0, ?3, ?4, 99, 0, X'00', ?4)",
            params![
                entry_hash.as_slice(),
                account_id.to_bytes().as_slice(),
                fingerprint.to_bytes().as_slice(),
                i64::try_from(ordinal).unwrap(),
            ],
        )
        .unwrap();
    }
}

fn seed_global_candidate_rows(conn: &Connection, mut count: usize, namespace_start: u64) {
    let fingerprint = Dev::new(9).fp;
    let mut account_ordinal = 0u64;
    while count > 0 {
        let fixture_namespace = namespace_start + account_ordinal;
        let account_id = AccountId::from_bytes(cbor::sha256(&fixture_namespace.to_be_bytes()));
        let account_count = count.min(CANDIDATES_PER_ACCOUNT_MAX);
        seed_candidate_rows(conn, account_id, fingerprint, fixture_namespace, account_count);
        count -= account_count;
        account_ordinal += 1;
    }
}

#[test]
fn a_genesis_ingests_and_is_accepted() {
    let conn = db();
    let (_acct, bytes, gh) = genesis(&Dev::new(1));
    let out = account_ingest(&conn, &bytes, NOW).unwrap();
    assert_eq!(out, IngestOutcome::Ingested {
        status: "accepted".into(),
        account_promotions: PromotionOutcome::default(),
        content_promotions: content::ContentPromotionOutcome::default()
    });
    assert_eq!(status(&conn, &gh).as_deref(), Some("accepted"));
}

#[test]
fn refold_projects_stream_authority_and_revoke_cuts_for_keyed_queries() {
    let conn = db();
    let founder = Dev::new(1);
    let grantee_device = Dev::new(2);
    let grantee = AccountId::from_bytes([0x44; 32]);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let (stream_id, own_op) = test_support::stream_own_public(account_id);
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own_op,
    );
    account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
    let grant_op = AccountOp::StreamGrant {
        stream_id,
        grantee_account_id: grantee,
        grant_role: GrantRole::Writer,
    };
    let (grant_bytes, grant_id) = op(
        account_id,
        &founder,
        2,
        Some(own_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &grant_op,
    );
    account_ingest(&conn, &grant_bytes, NOW + 2).unwrap();
    assert!(matches!(
        grant_effective_for_device(
            &conn,
            account_id,
            GrantId::from_bytes(grant_id),
            stream_id,
            grantee,
            grantee_device.fp,
        )
        .unwrap(),
        fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority {
            boundary: fold::GrantDeviceBoundary::Open,
            ..
        }),
    ));
    let cut_hash = [0x99; 32];
    let revoke_op = AccountOp::StreamRevoke {
        stream_id,
        grantee_account_id: grantee,
        grant_id: GrantId::from_bytes(grant_id),
        device_cuts: vec![DeviceCut {
            device_fingerprint: grantee_device.fp,
            seq: u64::MAX,
            hash: AccountEntryHash::from_bytes(cut_hash),
        }],
        reason: "access ended".to_string(),
    };
    let (revoke_bytes, _) = op(
        account_id,
        &founder,
        3,
        Some(grant_id),
        Some(OwnerId::from_bytes(genesis_hash)),
        &revoke_op,
    );
    account_ingest(&conn, &revoke_bytes, NOW + 3).unwrap();

    let state: (String, i64) = conn
        .query_row(
            "SELECT classification, effective_count FROM account_auth_state
                 WHERE account_id = ?1",
            [account_id.to_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, ("live".to_string(), 4));
    let grant: (String, i64, Option<i64>) = conn
        .query_row(
            "SELECT role, effective_at, closed_at FROM account_stream_grants
                 WHERE grant_id = ?1",
            [grant_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(grant, ("writer".to_string(), 2, Some(3)));
    let cut: (Vec<u8>, Vec<u8>, Vec<u8>) = conn
        .query_row(
            "SELECT device_fingerprint, seq, entry_hash
                 FROM account_stream_grant_cuts WHERE grant_id = ?1",
            [grant_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        cut,
        (grantee_device.fp.to_bytes().to_vec(), u64::MAX.to_be_bytes().to_vec(), cut_hash.to_vec(),),
    );
    assert_eq!(
        stream_owner_effective(&conn, account_id, stream_id).unwrap(),
        fold::AuthorityQuery::Effective(own_hash.into()),
    );
    assert_eq!(
        grant_device_cut(&conn, account_id, GrantId::from_bytes(grant_id), grantee_device.fp)
            .unwrap(),
        fold::AuthorityQuery::Effective(Some(DeviceCut {
            device_fingerprint: grantee_device.fp,
            seq: u64::MAX,
            hash: AccountEntryHash::from_bytes(cut_hash),
        })),
    );
    assert_eq!(
        grant_effective_for_device(
            &conn,
            account_id,
            GrantId::from_bytes(grant_id),
            stream_id,
            grantee,
            grantee_device.fp,
        )
        .unwrap(),
        fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority {
            grant: fold::GrantAuthority {
                stream_id,
                grantee_account_id: grantee,
                role: GrantRole::Writer,
            },
            boundary: fold::GrantDeviceBoundary::Cut(DeviceCut {
                device_fingerprint: grantee_device.fp,
                seq: u64::MAX,
                hash: AccountEntryHash::from_bytes(cut_hash),
            }),
        }),
    );
    assert_eq!(
        grant_device_cut(&conn, account_id, GrantId::from_bytes(grant_id), Dev::new(3).fp).unwrap(),
        fold::AuthorityQuery::Effective(None),
    );
    assert!(matches!(
        grant_effective_for_device(
            &conn,
            account_id,
            GrantId::from_bytes(grant_id),
            stream_id,
            grantee,
            Dev::new(3).fp,
        )
        .unwrap(),
        fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority {
            boundary: fold::GrantDeviceBoundary::Closed,
            ..
        }),
    ));
    // A citation that files a grant we DO hold under an account it does not belong to is
    // refuted by the entry's own bytes, so it is a wrong subject — not an unknown reference.
    // (The account-wide `auth_len` preflight used to mask this as `Unknown` whenever we
    // happened to hold no authority state for the claimed account.)
    assert_eq!(
        grant_device_cut(
            &conn,
            AccountId::from_bytes([0x66; 32]),
            GrantId::from_bytes(grant_id),
            grantee_device.fp,
        )
        .unwrap(),
        fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject),
    );
    // A grant we hold nothing about stays recoverable: refetch and re-evaluate.
    assert_eq!(
        grant_device_cut(
            &conn,
            AccountId::from_bytes([0x66; 32]),
            GrantId::from_bytes([0x77; 32]),
            grantee_device.fp,
        )
        .unwrap(),
        fold::AuthorityQuery::Unknown,
    );
    assert!(matches!(
        grant_effective(&conn, account_id, GrantId::from_bytes(grant_id), stream_id, grantee)
            .unwrap(),
        fold::AuthorityQuery::Effective(fold::GrantAuthority { role: GrantRole::Writer, .. })
    ));
    assert_eq!(
        grant_effective(&conn, account_id, GrantId::from_bytes(grant_id), stream_id, grantee)
            .unwrap(),
        fold::AuthorityQuery::Effective(fold::GrantAuthority {
            stream_id,
            grantee_account_id: grantee,
            role: GrantRole::Writer,
        }),
    );
    assert!(matches!(
        roster_ref_effective(&conn, account_id, RosterRef::from_bytes(genesis_hash), founder.fp)
            .unwrap(),
        fold::AuthorityQuery::Effective(fold::RosterAuthority {
            current_role: DeviceRole::Owner,
            ..
        })
    ));
    assert!(matches!(
        owner_incarnation_effective(
            &conn,
            account_id,
            OwnerId::from_bytes(genesis_hash),
            founder.fp
        )
        .unwrap(),
        fold::AuthorityQuery::Effective(_)
    ));
    assert_eq!(
        grant_effective(&conn, account_id, GrantId::from_bytes([0xaa; 32]), stream_id, grantee)
            .unwrap(),
        fold::AuthorityQuery::Unknown,
    );

    // Corrupt projection state must fail closed: an open grant cannot legitimately retain a
    // revoke cut from an older projection round.
    conn.execute(
        "UPDATE account_stream_grants SET closed_at = NULL
             WHERE owner_account_id = ?1 AND grant_id = ?2",
        params![account_id.to_bytes().as_slice(), grant_id.as_slice()],
    )
    .unwrap();
    let error = grant_effective_for_device(
        &conn,
        account_id,
        GrantId::from_bytes(grant_id),
        stream_id,
        grantee,
        grantee_device.fp,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("open grant unexpectedly has a persisted device cut"),
        "unexpected corrupt-projection error: {error:#}",
    );
}

#[test]
fn the_authority_backfill_purges_a_grant_a_pre_gate_binary_folded_on_a_private_stream() {
    // Simulates the V115 upgrade input: a binary predating the grants-require-PublicRead
    // fold gate ingested a hand-crafted private-stream grant and projected it effective. The
    // all-account backfill must re-judge it — the projected row would otherwise keep
    // answering Effective until some unrelated ingest refolds this account.
    let conn = db();
    let founder = Dev::new(1);
    let grantee = AccountId::from_bytes([0x44; 32]);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (stream_id, own_op) =
        test_support::stream_own_mode(account_id, crate::stream::AccessMode::Private, "repo-a");
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own_op,
    );
    account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
    let grant_op = AccountOp::StreamGrant {
        stream_id,
        grantee_account_id: grantee,
        grant_role: GrantRole::Writer,
    };
    let (grant_bytes, grant_id) = op(
        account_id,
        &founder,
        2,
        Some(own_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &grant_op,
    );
    // The CURRENT fold rejects this at ingest; overwrite its verdict and plant the projected
    // row exactly as the pre-gate fold left them.
    account_ingest(&conn, &grant_bytes, NOW + 2).unwrap();
    conn.execute("UPDATE account_entries SET accepted = 1 WHERE entry_hash = ?1", [
        grant_id.as_slice()
    ])
    .unwrap();
    conn.execute(
        "INSERT INTO account_stream_grants(
                 grant_id, owner_account_id, stream_id, grantee_account_id, role,
                 effective_at, closed_at)
             VALUES(?1, ?2, ?3, ?4, 'writer', 3, NULL)",
        params![
            grant_id.as_slice(),
            account_id.to_bytes().as_slice(),
            stream_id.to_bytes().as_slice(),
            grantee.to_bytes().as_slice(),
        ],
    )
    .unwrap();
    assert_eq!(
        open_writer_grants(&conn, account_id, stream_id, grantee).unwrap(),
        vec![GrantId::from(grant_id)],
        "the planted legacy projection answers Effective before the backfill",
    );

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    backfill_authority_projection(&tx).unwrap();
    tx.commit().unwrap();
    assert!(
        open_writer_grants(&conn, account_id, stream_id, grantee).unwrap().is_empty(),
        "the backfill re-judges the private-stream grant out of the projection",
    );
    let accepted: bool = conn
        .query_row(
            "SELECT accepted FROM account_entries WHERE entry_hash = ?1",
            [grant_id.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!accepted, "the grant entry itself is re-judged rejected");
}

#[test]
fn writer_grantees_list_open_grants_and_every_grantee_stays_ever_granted() {
    let conn = db();
    let founder = Dev::new(1);
    let writer = AccountId::from_bytes([0x44; 32]);
    let reader = AccountId::from_bytes([0x55; 32]);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (stream_id, own_op) = test_support::stream_own_public(account_id);
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own_op,
    );
    account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
    let writer_op = AccountOp::StreamGrant {
        stream_id,
        grantee_account_id: writer,
        grant_role: GrantRole::Writer,
    };
    let (writer_bytes, grant_id) = op(
        account_id,
        &founder,
        2,
        Some(own_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &writer_op,
    );
    account_ingest(&conn, &writer_bytes, NOW + 2).unwrap();
    // A Reader never authors content, so it must not become a pull target.
    let reader_op = AccountOp::StreamGrant {
        stream_id,
        grantee_account_id: reader,
        grant_role: GrantRole::Reader,
    };
    let (reader_bytes, reader_hash) = op(
        account_id,
        &founder,
        3,
        Some(grant_id),
        Some(OwnerId::from_bytes(genesis_hash)),
        &reader_op,
    );
    account_ingest(&conn, &reader_bytes, NOW + 3).unwrap();
    assert_eq!(effective_writer_grantees(&conn, account_id).unwrap(), vec![writer]);
    let stranger = AccountId::from_bytes([0x66; 32]);
    assert!(owner_ever_granted(&conn, account_id, writer).unwrap());
    assert!(owner_ever_granted(&conn, account_id, reader).unwrap(), "any role counts");
    assert!(!owner_ever_granted(&conn, account_id, stranger).unwrap());

    let revoke_op = AccountOp::StreamRevoke {
        stream_id,
        grantee_account_id: writer,
        grant_id: GrantId::from_bytes(grant_id),
        device_cuts: vec![DeviceCut {
            device_fingerprint: Dev::new(2).fp,
            seq: u64::MAX,
            hash: AccountEntryHash::from_bytes([0x99; 32]),
        }],
        reason: "access ended".to_string(),
    };
    let (revoke_bytes, _) = op(
        account_id,
        &founder,
        4,
        Some(reader_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &revoke_op,
    );
    account_ingest(&conn, &revoke_bytes, NOW + 4).unwrap();
    assert!(effective_writer_grantees(&conn, account_id).unwrap().is_empty());
    assert!(
        owner_ever_granted(&conn, account_id, writer).unwrap(),
        "a revoked grantee's pre-cut history must stay verifiable, so it is still relayed",
    );
    let mut relayed = vec![writer, reader];
    relayed.sort_by_key(|account| account.to_bytes());
    assert_eq!(ever_granted_accounts(&conn, account_id).unwrap(), relayed);
}

#[test]
fn combined_grant_query_never_mixes_projection_rounds_across_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grant-snapshot.db");
    let setup = Connection::open(&path).unwrap();
    schema::apply(&setup, &crate::test_hooks()).unwrap();
    setup.execute_batch("PRAGMA journal_mode = WAL;").unwrap();

    let founder = Dev::new(1);
    let grantee_device = Dev::new(2);
    let grantee = AccountId::from_bytes([0x44; 32]);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&setup, &genesis_bytes, NOW).unwrap();
    let (stream_id, own_op) = test_support::stream_own_public(account_id);
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own_op,
    );
    account_ingest(&setup, &own_bytes, NOW + 1).unwrap();
    let grant_op = AccountOp::StreamGrant {
        stream_id,
        grantee_account_id: grantee,
        grant_role: GrantRole::Writer,
    };
    let (grant_bytes, grant_id) = op(
        account_id,
        &founder,
        2,
        Some(own_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &grant_op,
    );
    account_ingest(&setup, &grant_bytes, NOW + 2).unwrap();
    let cut_hash = [0x99; 32];
    let revoke_op = AccountOp::StreamRevoke {
        stream_id,
        grantee_account_id: grantee,
        grant_id: GrantId::from_bytes(grant_id),
        device_cuts: vec![DeviceCut {
            device_fingerprint: grantee_device.fp,
            seq: 7,
            hash: AccountEntryHash::from_bytes(cut_hash),
        }],
        reason: "access ended".to_string(),
    };
    let (revoke_bytes, _) = op(
        account_id,
        &founder,
        3,
        Some(grant_id),
        Some(OwnerId::from_bytes(genesis_hash)),
        &revoke_op,
    );
    account_ingest(&setup, &revoke_bytes, NOW + 3).unwrap();
    drop(setup);

    let reader = Connection::open(&path).unwrap();
    let writer = Connection::open(&path).unwrap();
    let snapshot = Transaction::new_unchecked(&reader, TransactionBehavior::Deferred).unwrap();
    assert_eq!(
        auth_len_freshness(&snapshot, account_id, 4).unwrap(),
        fold::AuthorityFreshness::CurrentOrBehind,
    );

    let write_tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).unwrap();
    write_tx
        .execute(
            "DELETE FROM account_stream_grant_cuts
                 WHERE owner_account_id = ?1 AND grant_id = ?2",
            params![account_id.to_bytes().as_slice(), grant_id.as_slice()],
        )
        .unwrap();
    write_tx
        .execute(
            "UPDATE account_stream_grants SET closed_at = NULL
                 WHERE owner_account_id = ?1 AND grant_id = ?2",
            params![account_id.to_bytes().as_slice(), grant_id.as_slice()],
        )
        .unwrap();
    write_tx.commit().unwrap();

    let old_round = grant_effective_for_device_in_snapshot(
        &snapshot,
        account_id,
        GrantId::from_bytes(grant_id),
        stream_id,
        grantee,
        grantee_device.fp,
    )
    .unwrap();
    assert!(matches!(
        old_round,
        fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority {
            boundary: fold::GrantDeviceBoundary::Cut(DeviceCut { seq: 7, hash, .. }),
            ..
        }) if hash == AccountEntryHash::from_bytes(cut_hash)
    ));
    drop(snapshot);

    assert!(matches!(
        grant_effective_for_device(
            &reader,
            account_id,
            GrantId::from_bytes(grant_id),
            stream_id,
            grantee,
            grantee_device.fp,
        )
        .unwrap(),
        fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority {
            boundary: fold::GrantDeviceBoundary::Open,
            ..
        }),
    ));
}

#[test]
fn authority_projection_failure_rolls_back_candidate_status_and_prior_shadow_state() {
    let conn = db();
    let founder = Dev::new(1);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_stream_authority_projection
             BEFORE INSERT ON account_stream_ownership
             BEGIN SELECT RAISE(ABORT, 'injected authority projection failure'); END;",
    )
    .unwrap();
    let (_, own_op) = test_support::stream_own_public(account_id);
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own_op,
    );

    assert!(account_ingest(&conn, &own_bytes, NOW + 1).is_err());
    let candidate_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_entries WHERE account_id = ?1",
            [account_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(candidate_count, 1, "the newly inserted candidate rolled back");
    assert_eq!(status(&conn, &own_hash), None, "its projected status rolled back");
    let effective_count: i64 = conn
        .query_row(
            "SELECT effective_count FROM account_auth_state WHERE account_id = ?1",
            [account_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(effective_count, 1, "the prior shadow projection survived intact");
}

/// A cut that condemns control ops an author had counted shrinks the effective count below the
/// length that author legitimately cited. Freshness measures against the held control log,
/// which never shrinks, so the content stays accepted and its memory stays projected (#1282).
#[test]
fn content_citing_ops_a_later_cut_condemns_stays_accepted() {
    let conn = db();
    let (founder, owner, member) = (Dev::new(0x41), Dev::new(0x42), Dev::new(0x43));
    let (x, y) = (Dev::new(0x44), Dev::new(0x45));
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (stream_id, own) = test_support::stream_own_public(account_id);
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own,
    );
    account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
    let (add_owner_bytes, add_owner) = op(
        account_id,
        &founder,
        2,
        Some(own_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&owner, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_owner_bytes, NOW + 2).unwrap();
    let (add_member_bytes, add_member) = op(
        account_id,
        &founder,
        3,
        Some(add_owner),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&member, DeviceRole::Member),
    );
    account_ingest(&conn, &add_member_bytes, NOW + 3).unwrap();
    // The added owner authors two ops of its own.
    let (o1_bytes, o1) = op(
        account_id,
        &owner,
        0,
        None,
        Some(OwnerId::from_bytes(add_owner)),
        &device_add(&x, DeviceRole::Member),
    );
    account_ingest(&conn, &o1_bytes, NOW + 4).unwrap();
    let (o2_bytes, _) = op(
        account_id,
        &owner,
        1,
        Some(o1),
        Some(OwnerId::from_bytes(add_owner)),
        &device_add(&y, DeviceRole::Member),
    );
    account_ingest(&conn, &o2_bytes, NOW + 5).unwrap();
    assert_eq!(account_effective_count(&conn, account_id).unwrap(), 6);

    let content =
        signed_member_content(&member, account_id, stream_id, RosterRef::from_bytes(add_member), 6);
    content_ingest(&conn, &content.signed_bytes, NOW + 6).unwrap();
    settle_pending_content_refolds(&conn, &ContentRefoldBudget::unbounded(), NOW).unwrap();
    assert_eq!(content_verdict(&conn, &content.entry_hash), ("accepted".into(), 1));

    // The founder removes the owner and condemns everything it authored.
    let remove = device_remove(&owner, super::super::cut::Cut::Empty);
    let (remove_bytes, remove_hash) = op(
        account_id,
        &founder,
        4,
        Some(add_member),
        Some(OwnerId::from_bytes(genesis_hash)),
        &remove,
    );
    account_ingest(&conn, &remove_bytes, NOW + 7).unwrap();
    settle_pending_content_refolds(&conn, &ContentRefoldBudget::unbounded(), NOW).unwrap();
    assert_eq!(status(&conn, &remove_hash).as_deref(), Some("accepted"));
    assert!(
        account_effective_count(&conn, account_id).unwrap() < 6,
        "premise: the cut shrank the effective count below the cited length",
    );
    assert_eq!(content_verdict(&conn, &content.entry_hash), ("accepted".into(), 1));
    assert_eq!(projected_nodes(&conn, stream_id), vec!["remote-node".to_string()]);
}

#[test]
fn remote_revoke_updates_authority_now_but_content_and_projection_only_at_settle() {
    let conn = db();
    let founder = Dev::new(0x31);
    let member = Dev::new(0x32);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let (stream_id, own) = test_support::stream_own_public(account_id);
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own,
    );
    account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
    let (add_bytes, add_hash) = op(
        account_id,
        &founder,
        2,
        Some(own_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&member, DeviceRole::Member),
    );
    account_ingest(&conn, &add_bytes, NOW + 2).unwrap();

    let content =
        signed_member_content(&member, account_id, stream_id, RosterRef::from_bytes(add_hash), 3);
    content_ingest(&conn, &content.signed_bytes, NOW + 3).unwrap();
    assert_eq!(
        settle_pending_content_refolds(&conn, &ContentRefoldBudget::unbounded(), NOW)
            .unwrap()
            .settled_streams,
        1
    );
    assert_eq!(content_verdict(&conn, &content.entry_hash), ("accepted".into(), 1));
    assert_eq!(projected_nodes(&conn, stream_id), vec!["remote-node".to_string()]);

    let remove = device_remove(&member, super::super::cut::Cut::Empty);
    let (remove_bytes, _) = op(
        account_id,
        &founder,
        3,
        Some(add_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &remove,
    );
    account_ingest(&conn, &remove_bytes, NOW + 4).unwrap();

    assert!(matches!(
        roster_ref_effective(&conn, account_id, RosterRef::from_bytes(add_hash), member.fp)
            .unwrap(),
        fold::AuthorityQuery::Invalid(_),
    ));
    assert_eq!(
        content_verdict(&conn, &content.entry_hash),
        ("accepted".into(), 1),
        "remote account ingest leaves content at the last completed fold",
    );
    assert_eq!(projected_nodes(&conn, stream_id), vec!["remote-node".to_string()]);
    let reason: i64 = conn
        .query_row(
            "SELECT reason_mask FROM content_streams_pending_refold WHERE stream_id = ?1",
            [stream_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reason, 2, "the revoke queues ACCOUNT_CHANGE");

    assert_eq!(
        settle_pending_content_refolds(&conn, &ContentRefoldBudget::unbounded(), NOW)
            .unwrap()
            .settled_streams,
        1
    );
    let (status, accepted) = content_verdict(&conn, &content.entry_hash);
    assert_eq!(accepted, 0);
    assert!(status.starts_with("condemned{"), "unexpected settled verdict: {status}");
    assert!(projected_nodes(&conn, stream_id).is_empty());
}

#[test]
fn trusted_revoke_projection_failure_rolls_back_authority_content_and_projection() {
    let conn = db();
    let founder = Dev::new(0x33);
    let member = Dev::new(0x34);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (stream_id, own) = test_support::stream_own_public(account_id);
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own,
    );
    account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
    let (add_bytes, add_hash) = op(
        account_id,
        &founder,
        2,
        Some(own_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&member, DeviceRole::Member),
    );
    account_ingest(&conn, &add_bytes, NOW + 2).unwrap();
    let content =
        signed_member_content(&member, account_id, stream_id, RosterRef::from_bytes(add_hash), 3);
    content_ingest(&conn, &content.signed_bytes, NOW + 3).unwrap();
    settle_pending_content_refolds(&conn, &ContentRefoldBudget::unbounded(), NOW).unwrap();

    conn.execute_batch(
        "CREATE TRIGGER fail_content_reproject
             BEFORE DELETE ON content_projected_nodes
             BEGIN SELECT RAISE(ABORT, 'injected content projection failure'); END;",
    )
    .unwrap();
    let remove = device_remove(&member, super::super::cut::Cut::Empty);
    let (remove_bytes, remove_hash) = op(
        account_id,
        &founder,
        3,
        Some(add_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &remove,
    );
    let signed = envelope::verify_account_signed(&remove_bytes, &founder.secret.public()).unwrap();
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    insert_candidate(&tx, &signed, &remove_bytes, NOW + 4).unwrap();
    assert!(refold_in_tx(&tx, account_id, NOW + 4).is_err());
    drop(tx);

    assert_eq!(status(&conn, &remove_hash), None, "the trusted candidate rolled back");
    assert!(matches!(
        roster_ref_effective(&conn, account_id, RosterRef::from_bytes(add_hash), member.fp)
            .unwrap(),
        fold::AuthorityQuery::Effective(_),
    ));
    assert_eq!(content_verdict(&conn, &content.entry_hash), ("accepted".into(), 1));
    assert_eq!(projected_nodes(&conn, stream_id), vec!["remote-node".to_string()]);
}

#[test]
fn v064_forward_migration_backfills_populated_account_histories() {
    let conn = db();
    let founder = Dev::new(1);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (stream_id, own_op) = test_support::stream_own_public(account_id);
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own_op,
    );
    account_ingest(&conn, &own_bytes, NOW + 1).unwrap();

    conn.execute("DELETE FROM schema_version WHERE id = '064_account_authority_projection'", [])
        .unwrap();
    conn.execute_batch(
        "DROP TABLE account_stream_grant_cuts;
             DROP TABLE account_stream_grants;
             DROP TABLE account_stream_ownership;
             DROP TABLE account_owner_incarnations;
             DROP TABLE account_roster_history;
             DROP TABLE account_auth_state;",
    )
    .unwrap();

    schema::migrate_forward(&conn, &crate::test_hooks()).unwrap();
    assert_eq!(
        stream_owner_effective(&conn, account_id, stream_id).unwrap(),
        fold::AuthorityQuery::Effective(own_hash.into()),
    );
    assert!(matches!(
        roster_ref_effective(&conn, account_id, RosterRef::from_bytes(genesis_hash), founder.fp)
            .unwrap(),
        fold::AuthorityQuery::Effective(fold::RosterAuthority {
            current_role: DeviceRole::Owner,
            ..
        })
    ));
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
}

#[test]
fn v064_backfill_failure_rolls_back_ddl_projection_and_ledger_together() {
    let conn = db();
    conn.execute("DELETE FROM schema_version WHERE id = '064_account_authority_projection'", [])
        .unwrap();
    conn.execute_batch(
        "DROP TABLE account_stream_grant_cuts;
             DROP TABLE account_stream_grants;
             DROP TABLE account_stream_ownership;
             DROP TABLE account_owner_incarnations;
             DROP TABLE account_roster_history;
             DROP TABLE account_auth_state;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO account_entries(
                 entry_hash, account_id, log_id, device_fingerprint, seq, entry_type,
                 accepted, signed_bytes, received_at_ms
             ) VALUES (?1, ?2, 0, ?1, 0, 99, 0, X'00', 0)",
        params![[0x11u8; 32].as_slice(), [0x22u8; 31].as_slice()],
    )
    .unwrap();

    assert!(schema::migrate_forward(&conn, &crate::test_hooks()).is_err());
    let table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'account_auth_state'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_count, 0, "V064 DDL rolled back with the failed backfill");
    let ledger_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version
                 WHERE id = '064_account_authority_projection'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger_count, 0, "a failed projection never claims V064 was applied");
}

#[test]
fn authority_queries_fail_closed_on_subject_mismatch_ahead_state_and_corrupt_rows() {
    let conn = db();
    let founder = Dev::new(1);
    let grantee = AccountId::from_bytes([0x44; 32]);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (stream_id, own_op) = test_support::stream_own_public(account_id);
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own_op,
    );
    account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
    let (grant_bytes, grant_id) = op(
        account_id,
        &founder,
        2,
        Some(own_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &AccountOp::StreamGrant {
            stream_id,
            grantee_account_id: grantee,
            grant_role: GrantRole::Reader,
        },
    );
    account_ingest(&conn, &grant_bytes, NOW + 2).unwrap();
    let (self_grant_bytes, self_grant_id) = op(
        account_id,
        &founder,
        3,
        Some(grant_id),
        Some(OwnerId::from_bytes(genesis_hash)),
        &AccountOp::StreamGrant {
            stream_id,
            grantee_account_id: account_id,
            grant_role: GrantRole::Reader,
        },
    );
    account_ingest(&conn, &self_grant_bytes, NOW + 3).unwrap();

    // Freshness is its own seam: an ahead assertion is measured against the fold, and it does
    // NOT reach into the fact queries — the grant resolves from the current fold either way, so
    // an ahead counter can neither hide nor manufacture an authority verdict.
    assert_eq!(auth_len_freshness(&conn, account_id, 99).unwrap(), fold::AuthorityFreshness::Ahead,);
    assert_eq!(
        auth_len_freshness(&conn, account_id, 3).unwrap(),
        fold::AuthorityFreshness::CurrentOrBehind,
    );
    assert!(matches!(
        grant_effective(&conn, account_id, GrantId::from_bytes(grant_id), stream_id, grantee)
            .unwrap(),
        fold::AuthorityQuery::Effective(_),
    ));
    assert_eq!(
        auth_len_freshness(&conn, AccountId::from_bytes([0x7e; 32]), 1).unwrap(),
        fold::AuthorityFreshness::Ahead,
        "an account we hold nothing for has folded zero effective ops",
    );
    assert_eq!(
        grant_effective(
            &conn,
            account_id,
            GrantId::from_bytes(grant_id),
            crate::stream::StreamId::from_bytes([0x55; 32]),
            grantee,
        )
        .unwrap(),
        fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject),
    );
    assert_eq!(
        grant_effective(
            &conn,
            account_id,
            GrantId::from_bytes(self_grant_id),
            stream_id,
            account_id
        )
        .unwrap(),
        fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::ReferencedEntryNotEffective,),
    );

    conn.execute("UPDATE account_stream_grants SET role = 'admin' WHERE grant_id = ?1", [
        grant_id.as_slice()
    ])
    .unwrap();
    assert!(
        grant_effective(&conn, account_id, GrantId::from_bytes(grant_id), stream_id, grantee)
            .is_err()
    );
    conn.execute(
        "UPDATE account_stream_grants SET role = 'reader', effective_at = -1
             WHERE grant_id = ?1",
        [grant_id.as_slice()],
    )
    .unwrap();
    assert!(
        grant_effective(&conn, account_id, GrantId::from_bytes(grant_id), stream_id, grantee)
            .is_err()
    );
    conn.execute("UPDATE account_stream_grants SET effective_at = 2 WHERE grant_id = ?1", [
        grant_id.as_slice(),
    ])
    .unwrap();
    conn.execute("UPDATE account_stream_ownership SET own_id = ?2 WHERE stream_id = ?1", params![
        stream_id.to_bytes().as_slice(),
        [0u8; 31].as_slice()
    ])
    .unwrap();
    assert!(stream_owner_effective(&conn, account_id, stream_id).is_err());
}

#[test]
fn malformed_envelopes_and_wrong_genesis_self_hashes_never_touch_storage() {
    let conn = db();
    assert!(matches!(account_ingest(&conn, &[0xff], NOW).unwrap(), IngestOutcome::Rejected(_)));

    let founder = Dev::new(1);
    let genesis_op = AccountOp::AccountGenesis {
        ed25519_pubkey: founder.ed,
        x25519_pubkey: founder.x,
        nonce16: [0u8; 16],
        created_at_ms: NOW as u64,
        label: None,
    };
    let payload = ops::encode(&genesis_op).unwrap();
    let wrong_account = AccountId::from_bytes([0x55; 32]);
    assert_ne!(wrong_account, id::account_id_from_genesis_payload(&payload));
    let header = AccountEntryHeader {
        account_id: wrong_account,
        log_id: 0,
        device_fingerprint: founder.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: ops::entry_type::ACCOUNT_GENESIS,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 0,
        key_id: None,
        authority_ref: None,
    };
    let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
    assert_eq!(
        account_ingest(&conn, &signed.signed_bytes, NOW).unwrap(),
        IngestOutcome::Rejected("genesis payload does not hash to its account_id".into()),
    );
    let stored: i64 =
        conn.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(stored, 0, "structural rejects never create candidate rows");
}

#[test]
fn a_forged_re_signed_genesis_is_rejected_not_stored() {
    // A non-owner re-signs the victim's genesis payload under its own key: the fold binds the
    // founder key, so ingest verifies but the fold classifies the forgery non-effective. (The
    // signature verifies under the attacker key AND fingerprint matches — the takeover defence
    // is the fold's founder binding, exercised end-to-end through ingest.)
    let conn = db();
    let (acct, real_bytes, real_gh) = genesis(&Dev::new(1));
    account_ingest(&conn, &real_bytes, NOW).unwrap();
    // The attacker re-signs the SAME payload (same account_id) under its own key.
    let attacker = Dev::new(9);
    let victim = Dev::new(1);
    let op = AccountOp::AccountGenesis {
        ed25519_pubkey: victim.ed,
        x25519_pubkey: victim.x,
        nonce16: [0u8; 16],
        created_at_ms: NOW as u64,
        label: None,
    };
    let payload = ops::encode(&op).unwrap();
    let header = AccountEntryHeader {
        account_id: acct,
        log_id: 0,
        device_fingerprint: attacker.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: ops::entry_type::ACCOUNT_GENESIS,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 0,
        key_id: None,
        authority_ref: None,
    };
    let signed = sign_account_entry(&attacker.secret, &header, &payload).unwrap();
    account_ingest(&conn, &signed.signed_bytes, NOW).unwrap();
    assert_eq!(status(&conn, &real_gh).as_deref(), Some("accepted"), "the real founder holds it");
    assert_ne!(
        status(&conn, &signed.entry_hash.into()).as_deref(),
        Some("accepted"),
        "the forged re-signed genesis never becomes the accepted root",
    );
}

#[test]
fn an_owner_added_after_genesis_is_accepted() {
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();
    let b = Dev::new(2);
    let (add_bytes, add_hash) = op(
        acct,
        &founder,
        1,
        Some(gh),
        Some(OwnerId::from_bytes(gh)),
        &device_add(&b, DeviceRole::Owner),
    );
    let out = account_ingest(&conn, &add_bytes, NOW).unwrap();
    assert_eq!(out, IngestOutcome::Ingested {
        status: "accepted".into(),
        account_promotions: PromotionOutcome::default(),
        content_promotions: content::ContentPromotionOutcome::default()
    });
    assert_eq!(status(&conn, &add_hash).as_deref(), Some("accepted"));
}

/// The sync existence check is by the EXACT signed envelope, not the entry_hash: two envelopes
/// can share a body (hence entry_hash) yet carry different signatures, and treating them as the
/// same would let holding one suppress the other on the wire (#406). A byte-flipped variant
/// therefore does NOT count as already held, and hashes distinctly.
#[test]
fn signed_entry_existence_is_by_exact_envelope_not_entry_hash() {
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, _gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();

    assert!(
        account_signed_entry_exists(&conn, acct, &gbytes).unwrap(),
        "the exact stored envelope is held",
    );
    // A different byte string (a corrupted-signature variant of the same body) is a DISTINCT
    // envelope: not held, and a distinct wire dedup hash.
    let mut variant = gbytes.clone();
    *variant.last_mut().unwrap() ^= 0x01;
    assert_ne!(variant, gbytes);
    assert!(
        !account_signed_entry_exists(&conn, acct, &variant).unwrap(),
        "a distinct signed envelope is not suppressed as already-held",
    );
    assert_ne!(
        account_signed_hash(&gbytes),
        account_signed_hash(&variant),
        "distinct envelopes get distinct wire dedup keys",
    );
}

/// A PARKED entry (signer not yet known) is part of what a peer must be offered for sync — a
/// peer holding the authorizer promotes it, and omitting it would let a session complete with
/// the dependent entry silently missing (#406). `account_entries_for_sync` includes both the
/// held genesis and the parked add.
#[test]
fn account_entries_for_sync_includes_a_parked_entry() {
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();

    // An entry signed by a device NOT yet known here parks in pre-verify — but it is real and a
    // peer with its authorizer can use it, so it must still be offered.
    let stranger = Dev::new(9);
    let (parked_bytes, parked_hash) = op(
        acct,
        &stranger,
        0,
        None,
        Some(OwnerId::from_bytes(gh)),
        &device_add(&Dev::new(3), DeviceRole::Owner),
    );
    assert_eq!(account_ingest(&conn, &parked_bytes, NOW).unwrap(), IngestOutcome::PreVerify);
    assert_eq!(status(&conn, &parked_hash), None, "parked, not a stored candidate");

    let offered = account_entries_for_sync(&conn, acct).unwrap();
    let hashes: Vec<AccountEntryHash> = offered.iter().map(|e| e.entry_hash).collect();
    assert!(hashes.contains(&gh.into()), "the held genesis is offered");
    assert!(hashes.contains(&parked_hash.into()), "the parked entry is offered too, not hidden");
    // And its bytes are the exact parked bytes, so a peer re-ingests the real entry.
    let parked =
        offered.iter().find(|e| e.entry_hash == AccountEntryHash::from_bytes(parked_hash)).unwrap();
    assert_eq!(parked.signed_bytes, parked_bytes);
}

#[test]
fn an_entry_whose_device_is_unknown_is_pre_verified_then_promoted() {
    // Ingest a founder-signed DeviceAdd BEFORE the genesis: the founder's key isn't resolvable,
    // so it parks in pre-verify. The genesis arrival resolves the founder and promotes it.
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    let b = Dev::new(2);
    let (add_bytes, add_hash) = op(
        acct,
        &founder,
        1,
        Some(gh),
        Some(OwnerId::from_bytes(gh)),
        &device_add(&b, DeviceRole::Owner),
    );

    // The add arrives first — unresolvable device → pre-verify.
    assert_eq!(account_ingest(&conn, &add_bytes, NOW).unwrap(), IngestOutcome::PreVerify);
    assert_eq!(status(&conn, &add_hash), None, "not yet a stored candidate");

    // The genesis resolves the founder and promotes the queued add.
    account_ingest(&conn, &gbytes, NOW).unwrap();
    assert_eq!(status(&conn, &gh).as_deref(), Some("accepted"));
    assert_eq!(status(&conn, &add_hash).as_deref(), Some("accepted"), "the queued add promoted");
    let pending: i64 =
        conn.query_row("SELECT COUNT(*) FROM account_pre_verify", [], |r| r.get(0)).unwrap();
    assert_eq!(pending, 0, "the pre-verify queue is drained");
}

#[test]
fn only_a_genesis_or_device_add_sweeps_parked_content_for_promotion() {
    // #798 adversarial finding 4: the pre-verify promotion sweep must stay gated on the entry
    // types that can actually make an unresolvable roster key resolve. Running it on EVERY
    // account entry decodes up to the per-author parked cap per entry — reintroducing exactly
    // the per-entry amplification the deferred-refold path exists to remove — and is pure waste
    // for every other entry type.
    //
    // Observable form: a `StreamOwn` (neither genesis nor `DeviceAdd`) must leave the parked
    // row exactly where it is; the `DeviceAdd` that follows is what drains it.
    let conn = db();
    let founder = Dev::new(0x41);
    let member = Dev::new(0x42);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let (stream_id, own) = test_support::stream_own_public(account_id);
    let (own_bytes, own_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &own,
    );
    account_ingest(&conn, &own_bytes, NOW + 1).unwrap();

    // Build the rest of the chain up front: the content binds to the DeviceAdd's hash as its
    // roster_ref, so the add's exact bytes (and therefore its seq/prev) must be fixed before
    // the content is signed. Chain order is genesis -> own -> second_own -> add.
    let (_, second_own) = test_support::stream_own_public(account_id);
    let (second_own_bytes, second_own_hash) = op(
        account_id,
        &founder,
        2,
        Some(own_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &second_own,
    );
    let (add_bytes, add_hash) = op(
        account_id,
        &founder,
        3,
        Some(second_own_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&member, DeviceRole::Member),
    );

    // The member is not on the roster yet, so its content parks pre-verified.
    let content =
        signed_member_content(&member, account_id, stream_id, RosterRef::from_bytes(add_hash), 4);
    content_ingest(&conn, &content.signed_bytes, NOW + 2).unwrap();
    let parked = |conn: &Connection| -> i64 {
        conn.query_row("SELECT COUNT(*) FROM content_pre_verify", [], |row| row.get(0)).unwrap()
    };
    assert_eq!(parked(&conn), 1, "the member's content parks behind its unresolvable device");

    // A `StreamOwn` — a perfectly ordinary control entry that resolves no device. The sweep
    // must not RUN at all; asserting only that the parked row survives would pass with the gate
    // deleted, because a sweep that can resolve nothing promotes nothing.
    super::super::content::reset_pre_verify_content_sweeps();
    account_ingest(&conn, &second_own_bytes, NOW + 3).unwrap();
    assert_eq!(
        super::super::content::pre_verify_content_sweeps(),
        0,
        "a non-resolving entry must not sweep the parked rows at all",
    );
    assert_eq!(parked(&conn), 1, "and the parked row is untouched");

    // The `DeviceAdd` is what can resolve the key, so it runs the sweep and drains the row.
    super::super::content::reset_pre_verify_content_sweeps();
    account_ingest(&conn, &add_bytes, NOW + 4).unwrap();
    assert_eq!(
        super::super::content::pre_verify_content_sweeps(),
        1,
        "the DeviceAdd runs the sweep exactly once",
    );
    assert_eq!(parked(&conn), 0, "the DeviceAdd promotes the parked content");
}

#[test]
fn an_annex_entry_is_stored_inert_and_never_touches_control_acceptance() {
    // #609 C6: the annex log (3) exists so a bookkeeping artifact can be authority-INERT by
    // topology. Two properties, and both must hold on a binary that knows nothing about it:
    // it is stored and retained header-only, and the control chain is completely unaffected.
    //
    // `entry_type = 0` is deliberate: on log 0 that is `AccountGenesis`, so this also proves
    // the log gate runs BEFORE any tag dispatch and an annex tag can never be misread as the
    // control tag with the same number.
    let conn = db();
    let founder = Dev::new(0x81);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let manifest = annex::ops::encode(&annex::ops::AnnexOp::Snapshot {
        state_format_version: annex::ops::SNAPSHOT_STATE_FORMAT_V1,
        moderation_epoch: 0,
        targets: Vec::new(),
    })
    .unwrap();
    let header = AccountEntryHeader {
        account_id,
        log_id: fold::ANNEX_LOG,
        device_fingerprint: founder.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: annex::ops::entry_type::SNAPSHOT,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes(genesis_hash)),
    };
    let annex = sign_account_entry(&founder.secret, &header, &manifest).unwrap();
    assert_eq!(
        account_ingest(&conn, &annex.signed_bytes, NOW + 1).unwrap(),
        IngestOutcome::Ingested {
            status: "retained_unfolded".into(),
            account_promotions: PromotionOutcome::default(),
            content_promotions: content::ContentPromotionOutcome::default()
        },
        "an annex entry is stored and retained, never folded and never rejected",
    );

    // The control chain is untouched: a later control op still accepts at its own seq. This is
    // the property a never-effective entry on log 0 would break (#809).
    let member = Dev::new(0x82);
    let (add_bytes, add_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&member, DeviceRole::Member),
    );
    account_ingest(&conn, &add_bytes, NOW + 2).unwrap();
    assert_eq!(status(&conn, &genesis_hash).as_deref(), Some("accepted"));
    assert_eq!(
        status(&conn, &add_hash).as_deref(),
        Some("accepted"),
        "the annex entry did not orphan the control chain",
    );
    assert_eq!(status(&conn, &annex.entry_hash.into()).as_deref(), Some("retained_unfolded"));
}

/// Sign one control-v2 view manifest as an annex entry of `signer`'s own chain.
fn view_manifest_entry(
    account_id: AccountId,
    signer: &Dev,
    seq: u64,
) -> envelope::SignedAccountEntry {
    let view = crate::account::control_v2::views::ViewManifest {
        checkpoint: [0x5c; 32],
        entries: Vec::new(),
    };
    let header = AccountEntryHeader {
        account_id,
        log_id: fold::ANNEX_LOG,
        device_fingerprint: signer.fp,
        seq,
        prev_hash: None,
        parent_ref: None,
        entry_type: annex::ops::entry_type::VIEW_MANIFEST,
        op_version: fold::SUPPORTED_OP_VERSION,
        crypto_suite: 0,
        auth_len: 0,
        key_id: None,
        authority_ref: None,
    };
    sign_account_entry(&signer.secret, &header, &view.encode().unwrap()).unwrap()
}

/// The branch an ingest took, as a stable token an assertion message can name.
///
/// Assertions about an ingest name the branch instead of interpolating the `IngestOutcome`: the
/// outcome is a value `account_ingest` returned, and formatting a returned value into a panic
/// message is the shape a cleartext-logging scan reads as writing it to a log. A closed match to
/// string literals carries nothing out of the outcome, so there is no value to leak — and a new
/// variant breaks this arm rather than going unnamed in a failure.
fn ingest_branch(outcome: &IngestOutcome) -> &'static str {
    match outcome {
        IngestOutcome::Rejected(_) => "rejected",
        IngestOutcome::PreVerify => "pre_verify",
        IngestOutcome::PreVerifyWithEviction { .. } => "pre_verify_with_eviction",
        IngestOutcome::CapacityReached { .. } => "capacity_reached",
        IngestOutcome::Ingested { .. } => "ingested",
    }
}

#[test]
fn a_view_manifest_reaches_capacity_an_ordinary_candidate_cannot() {
    // A cut names its evidence as a DETACHED manifest that competes for the same grow-only budget
    // as ordinary traffic. Without a floor reserved for it, an insider who exhausts the account's
    // budget parks every revocation on `ParkCause::Manifest` permanently — leaving the devices
    // those cuts revoke un-revoked — and capacity never drains, so that state is terminal.
    // The account is filled through the reservation counters rather than with seeded rows: a
    // manifest arrival refolds, and a refold re-decodes every stored candidate, so filler bytes
    // that are not real entries would fail the load rather than the budget.
    let ordinary_and_manifest = |reserved_entries: u64, reserved_bytes: u64| {
        let conn = db();
        // An outstanding reservation puts the fold's invite top-up on the path, and that resolves
        // key targets through the local device.
        crate::local_device(&conn, NOW).unwrap();
        let founder = Dev::new(0x91);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        super::super::bootstrap::upsert_account_candidate_reservation_in_tx(
            &tx,
            account_id,
            [0x7f; 32],
            reserved_entries,
            reserved_bytes,
            0,
            i64::MAX,
        )
        .unwrap();
        tx.commit().unwrap();
        let member = Dev::new(0x92);
        let (add_bytes, _) = op(
            account_id,
            &founder,
            1,
            Some(genesis_hash),
            Some(OwnerId::from_bytes(genesis_hash)),
            &device_add(&member, DeviceRole::Member),
        );
        let ordinary = account_ingest(&conn, &add_bytes, NOW + 1).unwrap();
        let manifest = view_manifest_entry(account_id, &founder, 0);
        let reserved = account_ingest(&conn, &manifest.signed_bytes, NOW + 2).unwrap();
        (ordinary, reserved)
    };

    // The entry floor: the account already holds (or has spoken for) every candidate slot ordinary
    // traffic may take — the genesis is one of them.
    let (ordinary, reserved) =
        ordinary_and_manifest((ORDINARY_CANDIDATES_PER_ACCOUNT_MAX - 1) as u64, 0);
    assert_eq!(ordinary, IngestOutcome::CapacityReached { scope: CapacityScope::CandidateAccount });
    assert_eq!(ingest_branch(&reserved), "ingested", "a view manifest reaches the reserved slots",);

    // The byte floor is a separate counter and needs its own case: entry slots are free here, and
    // only the ordinary byte budget is spoken for.
    let (ordinary, reserved) =
        ordinary_and_manifest(0, ORDINARY_CANDIDATE_BYTES_PER_ACCOUNT_MAX as u64);
    assert_eq!(ordinary, IngestOutcome::CapacityReached {
        scope: CapacityScope::CandidateAccountBytes
    });
    assert_eq!(ingest_branch(&reserved), "ingested", "a view manifest reaches the reserved bytes",);
}

#[test]
fn a_view_manifest_from_an_uncertified_device_is_only_parked() {
    // Why a manifest must be authored by the device that authors its cut: an unknown signer lands
    // in the pre-verify queue, which is capped per account and evicts oldest-first. Evidence a cut
    // depends on for as long as the cut exists cannot live somewhere it can be evicted from.
    let conn = db();
    let founder = Dev::new(0x95);
    let (account_id, genesis_bytes, _) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let manifest = view_manifest_entry(account_id, &Dev::new(0x96), 0);
    assert_eq!(
        account_ingest(&conn, &manifest.signed_bytes, NOW + 1).unwrap(),
        IngestOutcome::PreVerify
    );
    let held: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM account_entries WHERE entry_hash = ?1)",
            [manifest.entry_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!held, "a manifest no held key certifies never becomes durable evidence");
}

#[test]
fn a_garbage_annex_manifest_is_refused_at_ingest() {
    // The manifest is structurally validated at ingest (the per-log twin of the control and
    // secrets validators), so garbage can never chain — even though nothing folds this log.
    let conn = db();
    let founder = Dev::new(0x83);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let header = AccountEntryHeader {
        account_id,
        log_id: fold::ANNEX_LOG,
        device_fingerprint: founder.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: annex::ops::entry_type::SNAPSHOT,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes(genesis_hash)),
    };
    // A CBOR map where the manifest wire requires a 3-element array.
    let garbage = sign_account_entry(&founder.secret, &header, &[0xa1, 0x01, 0x02]).unwrap();
    let outcome = account_ingest(&conn, &garbage.signed_bytes, NOW + 1).unwrap();
    assert!(
        matches!(&outcome, IngestOutcome::Rejected(err) if err.contains("annex op payload")),
        "a structurally invalid manifest is rejected, not stored",
    );
    assert_eq!(status(&conn, &garbage.entry_hash.into()), None, "and it never became a chain link");

    // An UNKNOWN annex tag is the forward-compat case and must still be stored — it is a valid
    // chain link for a binary that does not know it, exactly like an unknown secrets tag.
    let forward =
        sign_account_entry(&founder.secret, &AccountEntryHeader { entry_type: 7, ..header }, &[
            0x81, 0x01,
        ])
        .unwrap();
    assert_eq!(
        account_ingest(&conn, &forward.signed_bytes, NOW + 2).unwrap(),
        IngestOutcome::Ingested {
            status: "retained_unfolded".into(),
            account_promotions: PromotionOutcome::default(),
            content_promotions: content::ContentPromotionOutcome::default()
        },
        "an unknown annex tag is retained, never rejected",
    );
}

/// The unsent-work guard reads the control log for enrolments, and the log keeps entries it
/// cannot decode (a future `op_version`, a sealed payload, an unknown tag). Those must be
/// skipped, not surfaced: the read backs every received row on a non-writer device, so an
/// error here would fail table sync outright for as long as the entry is stored — forever.
#[test]
fn an_undecodable_retained_control_entry_does_not_fail_the_writer_enrolment_read() {
    let conn = db();
    let founder = Dev::new(0x91);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let base = AccountEntryHeader {
        account_id,
        log_id: fold::CONTROL_LOG,
        device_fingerprint: founder.fp,
        seq: 1,
        prev_hash: Some(AccountEntryHash::from_bytes(genesis_hash)),
        parent_ref: Some(AccountEntryHash::from_bytes(genesis_hash)),
        entry_type: ops::entry_type::DEVICE_ADD,
        op_version: fold::SUPPORTED_OP_VERSION + 1,
        crypto_suite: 0,
        key_id: None,
        auth_len: 1,
        authority_ref: Some(OwnerId::from_bytes(genesis_hash)),
    };
    let retained = sign_account_entry(&founder.secret, &base, &[0x81, 0x01]).unwrap();
    assert_eq!(
        account_ingest(&conn, &retained.signed_bytes, NOW + 1).unwrap(),
        IngestOutcome::Ingested {
            status: "retained_unfolded".into(),
            account_promotions: PromotionOutcome::default(),
            content_promotions: content::ContentPromotionOutcome::default()
        },
    );
    // Empty the projection so the read walks the log; the stranger's answer has to get
    // past the retained entry (the founder's genesis short-circuits before it).
    conn.execute("DELETE FROM account_roster_history WHERE account_id = ?1", [account_id
        .to_bytes()
        .as_slice()])
        .unwrap();
    assert!(device_ever_enrolled_as_writer(&conn, account_id, founder.fp).unwrap());
    assert!(!device_ever_enrolled_as_writer(&conn, account_id, Dev::new(0x92).fp).unwrap());
}

/// TRIPWIRE (#809): a retained entry on the CONTROL log quarantines the rest of its own chain.
///
/// This is SPEC, not a defect, and this test exists so it cannot drift into one silently.
/// Branch selection accepts one contiguous chain per `(log, device)` built from EFFECTIVE
/// entries, so an entry the fold retains rather than folds breaks the `seq` walk at its slot
/// and every later entry from that device forks.
///
/// Why that is safe rather than a convergence bug: NO binary folds such an entry, so every
/// binary truncates at the same slot and honest peers still agree; and a third party cannot
/// place an entry on a device's chain (ingest verifies the signature), so the quarantine is
/// self-inflicted by the signer. The property is load-bearing only because **log 0's tag set is
/// closed** — a new artifact class gets its own log, as C6 did with `ANNEX_LOG`, never a new
/// tag, a bumped `op_version`, or a sealed payload here.
///
/// If this test fails, someone changed one of those two things: either branch selection now
/// walks through retained entries, or log 0 grew a class it cannot fold. Both are decisions to
/// make deliberately (see #809), not to absorb by editing the expectation.
///
/// Revisit only when an op is proposed that (a) must occupy a slot in log 0's per-device chain
/// with a cross-log hash anchor demonstrably insufficient, (b) is authority-inert — otherwise
/// `auth_len`/`AuthLenAhead` divergence kills it across versions regardless of chain walking —
/// and (c) cannot be expressed as an annex artifact keyed on a control-fold-derivable
/// condition. That change must land BEFORE transport ships (#406) or behind a protocol version
/// fence: once peers are live, accepting entries a peer forks is a mesh-splitting change.
#[test]
fn a_retained_entry_on_the_control_log_quarantines_the_rest_of_its_own_chain() {
    // Every class `fold_account` retains rather than folds, ON the control log. A new retained
    // class added here without a decision on #809 fails this test.
    enum Retained {
        UnknownTag,
        FutureVersion,
        SealedPayload,
    }
    let cases = [
        ("unknown entry_type", Retained::UnknownTag),
        ("future op_version", Retained::FutureVersion),
        ("sealed payload", Retained::SealedPayload),
    ];

    for (label, retained_class) in cases {
        let conn = db();
        let founder = Dev::new(0x91);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();

        let base = AccountEntryHeader {
            account_id,
            log_id: fold::CONTROL_LOG,
            device_fingerprint: founder.fp,
            seq: 1,
            prev_hash: Some(AccountEntryHash::from_bytes(genesis_hash)),
            parent_ref: Some(AccountEntryHash::from_bytes(genesis_hash)),
            entry_type: ops::entry_type::DEVICE_ADD,
            op_version: fold::SUPPORTED_OP_VERSION,
            crypto_suite: 0,
            key_id: None,
            auth_len: 1,
            authority_ref: Some(OwnerId::from_bytes(genesis_hash)),
        };
        let header = match retained_class {
            Retained::UnknownTag => AccountEntryHeader { entry_type: 250, ..base },
            Retained::FutureVersion =>
                AccountEntryHeader { op_version: fold::SUPPORTED_OP_VERSION + 1, ..base },
            Retained::SealedPayload =>
                AccountEntryHeader { crypto_suite: 1, key_id: Some([0x77; 32]), ..base },
        };
        let retained = sign_account_entry(&founder.secret, &header, &[0x81, 0x01]).unwrap();
        assert_eq!(
            account_ingest(&conn, &retained.signed_bytes, NOW + 1).unwrap(),
            IngestOutcome::Ingested {
                status: "retained_unfolded".into(),
                account_promotions: PromotionOutcome::default(),
                content_promotions: content::ContentPromotionOutcome::default()
            },
            "{label}: the entry is retained, never rejected — that half is the forward-compat \
             promise and must not regress either",
        );

        // An ordinary, perfectly valid op by the same device, chaining from it.
        let (add_bytes, add_hash) = op(
            account_id,
            &founder,
            2,
            Some(retained.entry_hash.into()),
            Some(OwnerId::from_bytes(genesis_hash)),
            &device_add(&Dev::new(0x92), DeviceRole::Owner),
        );
        account_ingest(&conn, &add_bytes, NOW + 2).unwrap();

        assert_eq!(
            status(&conn, &genesis_hash).as_deref(),
            Some("accepted"),
            "{label}: the chain up to the retained entry is unaffected",
        );
        assert_eq!(
            status(&conn, &add_hash).as_deref(),
            Some("forked"),
            "{label}: everything after a retained entry is quarantined — SPEC, see the doc \
             comment before changing this",
        );
    }
}

#[test]
fn a_sealed_snapshot_is_refused_rather_than_retained() {
    // A sealed manifest can never serve its purpose (§4.7's whole value is that a peer verifies
    // coverage WITHOUT plaintext), so it is refused rather than stored as opaque bytes no
    // binary could ever interpret. Contrast the control/secrets logs, where a sealed payload is
    // deliberately retained for a newer binary to fold.
    let conn = db();
    let founder = Dev::new(0x84);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let header = AccountEntryHeader {
        account_id,
        log_id: fold::ANNEX_LOG,
        device_fingerprint: founder.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: annex::ops::entry_type::SNAPSHOT,
        op_version: 1,
        crypto_suite: 1,
        key_id: Some([0x77; 32]),
        auth_len: 1,
        authority_ref: Some(OwnerId::from_bytes(genesis_hash)),
    };
    let sealed = sign_account_entry(&founder.secret, &header, &[0x81, 0x01]).unwrap();
    let outcome = account_ingest(&conn, &sealed.signed_bytes, NOW + 1).unwrap();
    assert!(
        matches!(&outcome, IngestOutcome::Rejected(err) if err.contains("plaintext-signed")),
        "a sealed snapshot is rejected",
    );
    assert_eq!(status(&conn, &sealed.entry_hash.into()), None, "and never became a chain link");

    // "A snapshot is plaintext" is a property of the artifact CLASS, not of one version's
    // encoding, so a future-op_version sealed snapshot is refused just the same — it would be
    // exactly as uninterpretable, and retaining it would leave a grow-only hole no verifier
    // could ever evaluate.
    let future_version =
        sign_account_entry(&founder.secret, &AccountEntryHeader { op_version: 2, ..header }, &[
            0x81, 0x01,
        ])
        .unwrap();
    let outcome = account_ingest(&conn, &future_version.signed_bytes, NOW + 2).unwrap();
    assert!(
        matches!(&outcome, IngestOutcome::Rejected(err) if err.contains("plaintext-signed")),
        "a sealed snapshot at a future op_version is refused too",
    );

    // The rejection is scoped to the SNAPSHOT tag, not the annex log: a future annex artifact
    // class may legitimately be sealed, and that option must survive this slice.
    let other_tag =
        sign_account_entry(&founder.secret, &AccountEntryHeader { entry_type: 9, ..header }, &[
            0x81, 0x01,
        ])
        .unwrap();
    assert_eq!(
        account_ingest(&conn, &other_tag.signed_bytes, NOW + 3).unwrap(),
        IngestOutcome::Ingested {
            status: "retained_unfolded".into(),
            account_promotions: PromotionOutcome::default(),
            content_promotions: content::ContentPromotionOutcome::default()
        },
        "a sealed NON-snapshot annex entry is still retained",
    );
}

/// END-TO-END through storage: author a real snapshot over the account's own history, ingest
/// it, and verify it back out. The algorithm has unit coverage against a fold fixture; this is
/// the path — stored bytes, manifest decode, covered walk over real rows — actually working.
fn author_snapshot_over(
    conn: &Connection,
    account_id: AccountId,
    founder: &Dev,
    genesis_hash: AccountEntryHash,
    mangle: impl FnOnce(&mut annex::ops::SnapshotTarget),
) -> [u8; 32] {
    // The honest claim: every device's control-chain head, and the hash of folding exactly
    // that.
    let rows = load_candidates(conn, account_id).unwrap();
    let held: Vec<_> = rows.iter().map(|r| r.verified.clone()).collect();
    let mut heads: std::collections::HashMap<DeviceFingerprint, (u64, [u8; 32])> =
        std::collections::HashMap::new();
    for entry in &held {
        if entry.header.log_id != fold::CONTROL_LOG {
            continue;
        }
        let slot = heads
            .entry(entry.header.device_fingerprint)
            .or_insert((entry.header.seq, entry.entry_hash.into()));
        if entry.header.seq >= slot.0 {
            *slot = (entry.header.seq, entry.entry_hash.into());
        }
    }
    let control_only: Vec<_> =
        held.iter().filter(|e| e.header.log_id == fold::CONTROL_LOG).cloned().collect();
    let mut target = annex::ops::SnapshotTarget {
        log_id: fold::CONTROL_LOG,
        stream_id: None,
        subject_account_id: None,
        folded_state_hash: annex::projection::folded_state_hash(&fold::fold_account(&control_only)),
        covered: heads
            .into_iter()
            .map(|(device_fingerprint, (seq, entry_hash))| annex::ops::CoveredWatermark {
                device_fingerprint,
                seq,
                entry_hash: AccountEntryHash::from_bytes(entry_hash),
            })
            .collect(),
    };
    mangle(&mut target);

    let payload = annex::ops::encode(&annex::ops::AnnexOp::Snapshot {
        state_format_version: annex::ops::SNAPSHOT_STATE_FORMAT_V1,
        moderation_epoch: 0,
        targets: vec![target],
    })
    .unwrap();
    let header = AccountEntryHeader {
        account_id,
        log_id: fold::ANNEX_LOG,
        device_fingerprint: founder.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: annex::ops::entry_type::SNAPSHOT,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref: Some(genesis_hash.into()),
    };
    let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
    account_ingest(conn, &signed.signed_bytes, NOW + 9).unwrap();
    signed.entry_hash.into()
}

#[test]
fn a_stored_snapshot_verifies_end_to_end_and_a_forged_one_does_not() {
    let conn = db();
    let founder = Dev::new(0x91);
    let member = Dev::new(0x92);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (add_bytes, _) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&member, DeviceRole::Member),
    );
    account_ingest(&conn, &add_bytes, NOW + 1).unwrap();

    let honest = author_snapshot_over(
        &conn,
        account_id,
        &founder,
        AccountEntryHash::from_bytes(genesis_hash),
        |_| {},
    );
    assert_eq!(
        verify_stored_snapshots(&conn, account_id).unwrap(),
        vec![(honest.into(), annex::verify::SnapshotVerdict::Verified)],
        "an honest claim over stored history verifies through the real read path",
    );

    // A forged hash on an otherwise well-formed manifest is detected, and — the load-bearing
    // half — the entry is still STORED. Verification never unaccepts anything.
    let conn = db();
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let forged = author_snapshot_over(
        &conn,
        account_id,
        &founder,
        AccountEntryHash::from_bytes(genesis_hash),
        |target| {
            target.folded_state_hash = [0xff; 32];
        },
    );
    assert_eq!(verify_stored_snapshots(&conn, account_id).unwrap(), vec![(
        forged.into(),
        annex::verify::SnapshotVerdict::Mismatch
    )],);
    assert_eq!(
        status(&conn, &forged).as_deref(),
        Some("retained_unfolded"),
        "a false claim is still stored — verification decides trust, never storage",
    );
}

/// THE LOOP CLOSED: production authoring feeds production verification and selection.
///
/// Every earlier C6 slice was exercised by hand-built snapshots. This is the first test where
/// the artifact is minted by the same code a real caller would use, which is what makes the
/// author/verifier agreement real rather than asserted — they share `on_branch_prefix`, so any
/// drift in what a watermark vector DENOTES fails here immediately, and would otherwise look
/// like a fold bug rather than a disagreement about set membership.
#[test]
fn an_authored_snapshot_verifies_and_is_selected_through_production_code() {
    let conn = db();
    let device = crate::local_device(&conn, NOW).unwrap();
    super::super::bootstrap::local_account(&conn, NOW).unwrap();
    let account =
        super::super::bootstrap::local_account_ref(&conn).unwrap().expect("account minted");

    let effective_before = account_effective_count(&conn, account.account_id).unwrap();

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let outcome =
        annex::author::author_snapshot_in_tx(&tx, &device, account.account_id, NOW + 2).unwrap();
    tx.commit().unwrap();

    let annex::author::SnapshotAuthorOutcome::Authored(hash) = outcome else {
        panic!("the local founder is an open owner with history");
    };

    // The claim the author made is one the verifier independently reproduces from the same
    // watermark vector — the whole point of sharing the prefix walk.
    assert_eq!(
        verify_stored_snapshots(&conn, account.account_id).unwrap(),
        vec![(hash, annex::verify::SnapshotVerdict::Verified)],
        "an authored snapshot must verify against the history it was authored over",
    );
    assert_eq!(
        selected_snapshot(&conn, account.account_id).unwrap().map(|s| s.entry_hash),
        Some(hash),
    );

    // It rides the annex log and is INERT there, asserted two ways. The status row proves
    // authoring refolded in its own transaction — without that, the entry exists in
    // `account_entries` with no projection row and a status-based reader silently omits it.
    assert_eq!(
        status(&conn, &hash.into()).as_deref(),
        Some("retained_unfolded"),
        "an authored snapshot must be projected, and projected as unfolded",
    );
    // And the count is the property that actually matters: an annex entry must not move
    // `effective_count`, or every later control op asserting the higher `auth_len` would park
    // un-healably on a binary that does not know the type (#809).
    assert_eq!(
        account_effective_count(&conn, account.account_id).unwrap(),
        effective_before,
        "an annex entry must not shift the control fold's effective count",
    );

    // And the claim is about real history: the covered vector names the founder's own chain.
    let usable = usable_snapshots(&conn, account.account_id).unwrap();
    let covered = &usable[0].targets[0].covered;
    assert_eq!(covered.len(), 1, "one device has control history so far");
    assert_eq!(covered[0].device_fingerprint, device.fingerprint());
}

/// A `DeviceAdd` for the store's OWN device — the local identity is minted, never seeded from a
/// `Dev`, so a foreign founder enrolling it has to name its real keys.
fn device_add_local(device: &crate::identity::LocalDevice, role: DeviceRole) -> AccountOp {
    AccountOp::DeviceAdd {
        device_fingerprint: device.fingerprint(),
        ed25519_pubkey: device.public().to_bytes(),
        x25519_pubkey: device.x25519_public().to_bytes(),
        role,
        label: None,
    }
}

/// A member holds the same history an owner does and can verify every snapshot it receives — it
/// simply has no incarnation to CITE, and the manifest is only usable while the incarnation it
/// cites stays open. So authoring reports the state instead of minting an unusable artifact.
#[test]
fn a_member_device_has_no_authority_to_cite_and_authors_nothing() {
    let conn = db();
    let device = crate::local_device(&conn, NOW).unwrap();
    let founder = Dev::new(0xa1);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let (add_local, _) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add_local(&device, DeviceRole::Member),
    );
    account_ingest(&conn, &add_local, NOW + 1).unwrap();

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let outcome = annex::author::author_snapshot_in_tx(&tx, &device, account_id, NOW + 2).unwrap();
    tx.commit().unwrap();

    assert_eq!(outcome, annex::author::SnapshotAuthorOutcome::NotAnOpenOwner);
    assert!(
        usable_snapshots(&conn, account_id).unwrap().is_empty(),
        "reporting the state must not have minted anything",
    );
}

/// A contested account is one whose authority is under dispute; a snapshot of it is a
/// clean-looking claim about disputed state. Verification already folds the full held set and
/// requires `Live`, so authoring one would only mint an artifact guaranteed to be rejected —
/// this device declines at the source rather than emitting garbage for peers to refuse.
#[test]
fn a_contested_account_is_never_snapshotted_even_by_an_open_owner() {
    let conn = db();
    let device = crate::local_device(&conn, NOW).unwrap();
    let (founder, a, b) = (Dev::new(0xa1), Dev::new(0xa2), Dev::new(0xa3));
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    // The local device is a bona fide open owner throughout — the refusal must come from the
    // account's classification, not from this device lacking authority.
    let (add_local, add_local_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add_local(&device, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_local, NOW + 1).unwrap();

    // Two other owners cut each other at the same depth: the mutual-condemnation cycle that
    // makes the fold fail closed to state_before(1).
    let (add_a, owner_a) = op(
        account_id,
        &founder,
        2,
        Some(add_local_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&a, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_a, NOW + 2).unwrap();
    let (add_b, owner_b) = op(
        account_id,
        &founder,
        3,
        Some(owner_a),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&b, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_b, NOW + 3).unwrap();
    let (remove_b, _) = op(
        account_id,
        &a,
        0,
        None,
        Some(OwnerId::from_bytes(owner_a)),
        &owner_demote(&b, OwnerId::from_bytes(owner_b)),
    );
    account_ingest(&conn, &remove_b, NOW + 4).unwrap();
    let (remove_a, _) = op(
        account_id,
        &b,
        0,
        None,
        Some(OwnerId::from_bytes(owner_b)),
        &owner_demote(&a, OwnerId::from_bytes(owner_a)),
    );
    account_ingest(&conn, &remove_a, NOW + 5).unwrap();

    let held = account_entries_view(&conn, account_id).unwrap();
    assert!(
        matches!(
            fold::fold_account(held.held()).classification(),
            fold::AccountClassification::Contested { .. }
        ),
        "the setup must actually contest the account, or this test proves nothing",
    );

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let outcome = annex::author::author_snapshot_in_tx(&tx, &device, account_id, NOW + 6).unwrap();
    tx.commit().unwrap();

    assert_eq!(outcome, annex::author::SnapshotAuthorOutcome::AccountNotLive);
    assert!(
        usable_snapshots(&conn, account_id).unwrap().is_empty(),
        "declining must not have minted anything",
    );
}

/// A snapshot's `parent_ref` is the CANONICAL root, not the lowest-hashed genesis-tagged entry.
///
/// Ingest validates a genesis payload against its `account_id` and its founder key, but NOT the
/// canonical header shape (§6). So a founder-signed entry carrying the real genesis payload at
/// a NON-origin seq is accepted and stored alongside the true root. `fold::find_genesis`
/// excludes it (a genesis is seq 0 by definition), which is why the account still folds `Live`
/// — but a naive "first entry with the genesis tag, in hash order" scan picks it whenever its
/// hash sorts lower, and `is_genesis` tests only the log and the tag. An off-origin seq is also
/// off every covered coordinate, so the equivocation guard does not fire either. Nothing
/// downstream revalidates `parent_ref`, so such a snapshot would store, report as authored, and
/// be selectable.
#[test]
fn a_snapshot_parents_the_canonical_genesis_not_a_lower_hashed_impostor() {
    let conn = db();
    let device = crate::local_device(&conn, NOW).unwrap();
    let founder = Dev::new(0xa1);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    // The same genesis payload re-signed with a non-null `parent_ref` — malformed per §6, yet
    // it satisfies every check ingest actually performs. Vary the bogus parent until the entry
    // hash sorts BELOW the real root, which is what makes a tag scan choose it.
    let genesis_op = AccountOp::AccountGenesis {
        ed25519_pubkey: founder.ed,
        x25519_pubkey: founder.x,
        nonce16: [0u8; 16],
        created_at_ms: NOW as u64,
        label: None,
    };
    let payload = ops::encode(&genesis_op).unwrap();
    let mut impostor = None;
    for seq in 2u64..4096 {
        let header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: founder.fp,
            seq, // <- the malformation: a genesis is seq 0 by definition (§6)
            prev_hash: Some(AccountEntryHash::from_bytes(genesis_hash)), /* ingest requires null
                  * iff seq == 0 */
            parent_ref: None,
            entry_type: ops::entry_type::ACCOUNT_GENESIS,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: None,
        };
        let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
        if signed.entry_hash < AccountEntryHash::from_bytes(genesis_hash) {
            impostor = Some(signed);
            break;
        }
    }
    let impostor = impostor.expect("some off-origin seq hashes below the real root");
    account_ingest(&conn, &impostor.signed_bytes, NOW + 1).unwrap();
    assert!(
        impostor.entry_hash < AccountEntryHash::from_bytes(genesis_hash),
        "the impostor must sort first, or this test cannot distinguish the two selections",
    );

    // The local device becomes an open owner so it can author at all.
    let (add_local, _) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add_local(&device, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_local, NOW + 2).unwrap();

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let outcome = annex::author::author_snapshot_in_tx(&tx, &device, account_id, NOW + 3).unwrap();
    tx.commit().unwrap();
    let annex::author::SnapshotAuthorOutcome::Authored(hash) = outcome else {
        panic!("the account still folds Live despite the impostor");
    };

    let stored = load_candidates(&conn, account_id)
        .unwrap()
        .into_iter()
        .find(|row| row.entry_hash == hash)
        .expect("the authored snapshot is stored");
    assert_eq!(
        stored.verified.header.parent_ref,
        Some(Into::into(genesis_hash)),
        "the snapshot must parent the canonical root the fold selected",
    );
    assert_ne!(stored.verified.header.parent_ref, Some(impostor.entry_hash), "never the impostor");
}

/// I4 SURVIVES A SNAPSHOT: the bound tombstone set is TOTAL, never windowed (#609).
///
/// I4 says a tombstoned fingerprint never re-enrolls. A snapshot binds folded state, so if that
/// state carried only *recent* tombstones — anything windowed, aged out, or summarised into an
/// accumulator — a peer that trusted the snapshot would admit a `DeviceAdd` for a fingerprint
/// removed deeper in the covered history, and I4 would hold locally while failing across the
/// very artifact meant to convey state.
///
/// That is also why no digest-style accumulator is sound here: I4 needs deterministic
/// *rejection* of a re-add, i.e. exact membership. A digest without a universally verifiable
/// non-membership proof makes the verdict depend on which peer is checking — the fold-firewall
/// failure this phase exists to prevent.
///
/// The removal is placed at the very bottom of the chain and the snapshot authored well after
/// it, so a windowed set would drop it.
#[test]
fn a_snapshot_binds_the_total_tombstone_set_so_a_deep_removal_still_bars_re_enrollment() {
    let conn = db();
    let device = crate::local_device(&conn, NOW).unwrap();
    super::super::bootstrap::local_account(&conn, NOW).unwrap();
    let account =
        super::super::bootstrap::local_account_ref(&conn).unwrap().expect("account minted");
    let (account_id, genesis_hash) = (account.account_id, account.genesis_hash);

    // Deep history: enroll a device, then remove it — this tombstone must outlive everything.
    let doomed = Dev::new(0xd1);
    let (add_bytes, roster_ref) = op_local(
        account_id,
        &device,
        1,
        Some(genesis_hash.into()),
        Some(genesis_hash.into()),
        &device_add(&doomed, DeviceRole::Member),
    );
    account_ingest(&conn, &add_bytes, NOW + 1).unwrap();
    let (remove_bytes, remove_hash) = op_local(
        account_id,
        &device,
        2,
        Some(roster_ref),
        Some(genesis_hash.into()),
        &device_remove(&doomed, super::super::cut::Cut::Empty),
    );
    account_ingest(&conn, &remove_bytes, NOW + 2).unwrap();

    // Bury it: unrelated history piles up on top, so a windowed tombstone set would age it out.
    let mut prev = remove_hash;
    for (index, seed) in (0xe0u8..0xe6).enumerate() {
        let (bytes, hash) = op_local(
            account_id,
            &device,
            3 + index as u64,
            Some(prev),
            Some(genesis_hash.into()),
            &device_add(&Dev::new(seed), DeviceRole::Member),
        );
        account_ingest(&conn, &bytes, NOW + 3 + index as i64).unwrap();
        prev = hash;
    }

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let outcome = annex::author::author_snapshot_in_tx(&tx, &device, account_id, NOW + 20).unwrap();
    tx.commit().unwrap();
    let annex::author::SnapshotAuthorOutcome::Authored(hash) = outcome else {
        panic!("the local founder is an open owner of a live account");
    };
    assert_eq!(
        verify_stored_snapshots(&conn, account_id).unwrap(),
        vec![(hash, annex::verify::SnapshotVerdict::Verified)],
        "the snapshot must verify, or it binds nothing at all",
    );

    // The BOUND state carries the deep tombstone. Asserted over the snapshot's own covered
    // prefix rather than over everything held: the prefix is what `folded_state_hash` is
    // computed from, so this is the set a peer inherits by trusting the manifest — folding all
    // held entries instead would prove only that the local fold works.
    let held = account_entries_view(&conn, account_id).unwrap();
    let usable = usable_snapshots(&conn, account_id).unwrap();
    let covered = &usable[0].targets[0].covered;
    let by_hash = held.held().iter().map(|entry| (entry.entry_hash, entry)).collect();
    let prefix =
        annex::verify::on_branch_prefix(covered, &by_hash).expect("the covered prefix is walkable");
    let bound = fold::fold_account(&prefix);
    assert!(
        bound.tombstoned().any(|fingerprint| *fingerprint == doomed.fp),
        "a removal at the bottom of the chain is still in the tombstone set the snapshot binds",
    );

    // And the guarantee bites: re-adding that fingerprint after the snapshot is not effective.
    let (readd_bytes, readd_hash) = op_local(
        account_id,
        &device,
        9,
        Some(prev),
        Some(genesis_hash.into()),
        &device_add(&doomed, DeviceRole::Member),
    );
    account_ingest(&conn, &readd_bytes, NOW + 21).unwrap();
    assert_eq!(
        status(&conn, &readd_hash).as_deref(),
        Some("rejected"),
        "a tombstoned fingerprint never re-enrolls, snapshot or no snapshot (I4) — asserted as \
         the exact verdict, since a merely-not-accepted re-add could be forked for an unrelated \
         chain reason and prove nothing",
    );
}

/// A coverage claim names the ACCEPTED branch, never merely the highest-sequence entry held.
///
/// The two sets diverge exactly under equivocation: a device that signs two different entries
/// at the SAME seq puts both in the candidate store, and branch selection accepts one. Sequence
/// alone cannot break that tie — both forks share a seq — so a "highest seq wins" rule falls
/// through to load order, which is not the store's branch selection.
///
/// Why this matters even though the snapshot below is refused locally: a peer that never
/// received the losing fork has no equivocation to object to. Handed a watermark naming the
/// losing side, it either cannot walk the chain at all or — if it holds only that side —
/// accepts a claim about a branch this store rejected.
#[test]
fn a_coverage_claim_names_the_accepted_fork_not_the_losing_one() {
    let conn = db();
    let device = crate::local_device(&conn, NOW).unwrap();
    let founder = Dev::new(0xa1);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let (add_local, add_local_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add_local(&device, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_local, NOW + 1).unwrap();

    // The founder equivocates: two DIFFERENT entries at seq 2 off the same prev.
    let (fork_x, fork_x_hash) = op(
        account_id,
        &founder,
        2,
        Some(add_local_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&Dev::new(0xb1), DeviceRole::Member),
    );
    account_ingest(&conn, &fork_x, NOW + 2).unwrap();
    let (fork_y, fork_y_hash) = op(
        account_id,
        &founder,
        2,
        Some(add_local_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&Dev::new(0xb2), DeviceRole::Member),
    );
    account_ingest(&conn, &fork_y, NOW + 3).unwrap();

    // Which side the store kept is its business; the watermark must follow it, whichever it is.
    let (winner, loser) =
        match (status(&conn, &fork_x_hash).as_deref(), status(&conn, &fork_y_hash).as_deref()) {
            (Some("accepted"), Some("forked")) => (fork_x_hash, fork_y_hash),
            (Some("forked"), Some("accepted")) => (fork_y_hash, fork_x_hash),
            other =>
                panic!("the setup must produce exactly one accepted and one forked: {other:?}"),
        };
    assert!(loser != winner);

    // The production head selection the author uses — asserted directly, because the end-to-end
    // path below deliberately refuses this snapshot and so cannot discriminate the branches.
    let heads = account_entries_view(&conn, account_id).unwrap().accepted_control_heads();
    let founder_head = heads
        .iter()
        .find(|w| w.device_fingerprint == founder.fp)
        .expect("the founder's chain is covered");
    assert_eq!(
        founder_head.entry_hash,
        AccountEntryHash::from_bytes(winner),
        "the watermark must name the branch the store accepted, not the higher hash",
    );
    assert_eq!(founder_head.seq, 2, "and it is still that device's head");

    // End to end, a device that HOLDS the equivocation declines rather than publishing a
    // one-branch view of a chain it knows to be forked — and it declines BEFORE minting, since
    // the artifact would be refused by this very device and would only burn candidate capacity.
    let effective_before = account_effective_count(&conn, account_id).unwrap();
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let outcome = annex::author::author_snapshot_in_tx(&tx, &device, account_id, NOW + 4).unwrap();
    tx.commit().unwrap();
    assert_eq!(outcome, annex::author::SnapshotAuthorOutcome::HeldEvidenceOffBranch);
    assert!(
        verify_stored_snapshots(&conn, account_id).unwrap().is_empty(),
        "declining must not have stored a snapshot to verify",
    );
    assert!(usable_snapshots(&conn, account_id).unwrap().is_empty());
    assert_eq!(
        account_effective_count(&conn, account_id).unwrap(),
        effective_before,
        "and the control fold is untouched either way",
    );
}

/// Selection ranks by VERIFIED coverage, never by claims the verifier skipped.
///
/// An unsupported target (secrets, or content until #406) is passed over by verification
/// without affecting the verdict. If selection counted it, padding a manifest with fabricated
/// coverage would be a way to outrank an honest snapshot — winning on a claim nobody checked.
#[test]
fn padding_a_manifest_with_unverifiable_coverage_does_not_win_selection() {
    let conn = db();
    let founder = Dev::new(0xb1);
    let member = Dev::new(0xb2);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (add_bytes, _) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&member, DeviceRole::Member),
    );
    account_ingest(&conn, &add_bytes, NOW + 1).unwrap();

    // An honest control-only snapshot, and a padded one claiming extra secrets-log coverage
    // this binary cannot check. The padded one is authored second so its entry_hash is not
    // guaranteed to lose the tiebreak — the assertion below is about coverage, so pin whichever
    // wins by comparing against the honest hash directly.
    let honest = author_snapshot_over(
        &conn,
        account_id,
        &founder,
        AccountEntryHash::from_bytes(genesis_hash),
        |_| {},
    );
    let padded = author_snapshot_over(
        &conn,
        account_id,
        &founder,
        AccountEntryHash::from_bytes(genesis_hash),
        |target| {
            target.covered.push(annex::ops::CoveredWatermark {
                device_fingerprint: member.fp,
                seq: 0,
                entry_hash: AccountEntryHash::from_bytes([0xcd; 32]),
            });
        },
    );
    assert_ne!(honest, padded, "the two snapshots are distinct entries");

    let usable = usable_snapshots(&conn, account_id).unwrap();
    // The padded snapshot names a watermark this device does not hold, so it does not even
    // verify — the first line of defence. What matters for selection is that a snapshot cannot
    // gain rank from coverage that was never checked.
    assert!(
        usable.iter().all(|s| s.targets.iter().all(annex::verify::is_supported_target)),
        "only verified targets may reach the selector",
    );
    assert_eq!(
        selected_snapshot(&conn, account_id).unwrap().map(|s| s.entry_hash),
        Some(Into::into(honest)),
        "the honest snapshot is chosen; unverifiable coverage buys no rank",
    );
}

/// The revocation story for this artifact class, and the reason it is incarnation-scoped.
///
/// Registers are minted per log and `ChainKind` has no annex variant, so NO control op can cut
/// an annex chain — a revoked device's snapshots cannot be condemned by watermark the way its
/// control entries are. Usability is therefore tied to the owner incarnation the snapshot
/// cites, which closing kills wholesale: every snapshot authored under it becomes unusable,
/// including ones authored long before the revocation. Coarser than a beyond-cut boundary, and
/// for a claim about folded state that is the safer direction.
#[test]
fn closing_an_incarnation_makes_every_snapshot_it_authored_unusable() {
    let conn = db();
    let founder = Dev::new(0xa1);
    let owner_b = Dev::new(0xa2);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    // A second owner; the DeviceAdd mints its incarnation, and that hash is its `owner_id`.
    let (add_b, owner_b_id) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&owner_b, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_b, NOW + 1).unwrap();

    // `owner_b` snapshots under its own incarnation, BEFORE any revocation.
    let snapshot_hash = author_snapshot_over(
        &conn,
        account_id,
        &owner_b,
        AccountEntryHash::from_bytes(owner_b_id),
        |_| {},
    );
    let usable = usable_snapshots(&conn, account_id).unwrap();
    assert_eq!(usable.len(), 1, "an open incarnation's snapshot is usable");
    assert_eq!(usable[0].entry_hash, AccountEntryHash::from_bytes(snapshot_hash));
    assert_eq!(
        selected_snapshot(&conn, account_id).unwrap().map(|s| s.entry_hash),
        Some(Into::into(snapshot_hash)),
    );

    // The founder closes that incarnation. The snapshot is untouched and still verifies against
    // local history — its claim was never false — but the authority it was authored under is
    // gone.
    let (demote_bytes, _) = op(
        account_id,
        &founder,
        2,
        Some(owner_b_id),
        Some(OwnerId::from_bytes(genesis_hash)),
        &owner_demote(&owner_b, OwnerId::from_bytes(owner_b_id)),
    );
    account_ingest(&conn, &demote_bytes, NOW + 3).unwrap();

    assert_eq!(
        verify_stored_snapshots(&conn, account_id).unwrap(),
        vec![(snapshot_hash.into(), annex::verify::SnapshotVerdict::Verified)],
        "the claim is still true — revocation is about authority, not correctness",
    );
    assert!(
        usable_snapshots(&conn, account_id).unwrap().is_empty(),
        "a pre-revocation snapshot dies with its incarnation, not at a watermark",
    );
    assert_eq!(selected_snapshot(&conn, account_id).unwrap(), None);
    assert_eq!(
        status(&conn, &snapshot_hash).as_deref(),
        Some("retained_unfolded"),
        "and it is still stored — usability decides trust, never storage",
    );
}

#[test]
fn pre_verify_budget_evicts_oldest_per_account_and_globally() {
    let conn = db();
    let account_a = AccountId::from_bytes([0xa1; 32]);
    for ordinal in 0..PRE_VERIFY_PER_ACCOUNT_MAX + 2 {
        let raw = ordinal.to_be_bytes();
        insert_pre_verify(
            &conn,
            &cbor::sha256(&raw).into(),
            account_a,
            Dev::new(2).fp,
            &raw,
            ordinal as i64,
        )
        .unwrap();
    }
    let account_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_pre_verify WHERE claimed_account_id = ?1",
            params![account_a.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(account_count, PRE_VERIFY_PER_ACCOUNT_MAX as i64);
    let oldest_remaining: i64 = conn
        .query_row(
            "SELECT MIN(received_at_ms) FROM account_pre_verify
                 WHERE claimed_account_id = ?1",
            params![account_a.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(oldest_remaining, 2, "the oldest account rows are evicted first");

    for account_byte in 0xb0..0xb4 {
        let account = AccountId::from_bytes([account_byte; 32]);
        for ordinal in 0..PRE_VERIFY_PER_ACCOUNT_MAX {
            let raw = [account_byte, ordinal as u8];
            insert_pre_verify(
                &conn,
                &cbor::sha256(&raw).into(),
                account,
                Dev::new(3).fp,
                &raw,
                1_000 + i64::try_from(ordinal).unwrap(),
            )
            .unwrap();
        }
    }
    let global_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM account_pre_verify", [], |row| row.get(0)).unwrap();
    assert_eq!(global_count, PRE_VERIFY_GLOBAL_MAX as i64);
    let account_a_remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_pre_verify WHERE claimed_account_id = ?1",
            params![account_a.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(account_a_remaining, 0, "global eviction removes the globally oldest rows");

    let evicted_raw = [0xfe, 0xed];
    assert_eq!(
        insert_pre_verify(
            &conn,
            &cbor::sha256(&evicted_raw).into(),
            AccountId::from_bytes([0xcc; 32]),
            Dev::new(4).fp,
            &evicted_raw,
            -1,
        )
        .unwrap(),
        PreVerifyInsert::AtCapacity(CapacityScope::PreVerifyGlobal),
        "the insert result reports which budget evicted the new row",
    );
}

#[test]
fn pre_verify_oldest_ties_are_broken_by_signed_hash() {
    let conn = db();
    let account_id = AccountId::from_bytes([0xd1; 32]);
    let mut expected_hashes = Vec::new();
    for ordinal in 0..=PRE_VERIFY_PER_ACCOUNT_MAX {
        let raw = ordinal.to_be_bytes();
        let signed_hash = cbor::sha256(&raw);
        expected_hashes.push(signed_hash);
        insert_pre_verify(
            &conn,
            &cbor::sha256(&signed_hash).into(),
            account_id,
            Dev::new(2).fp,
            &raw,
            NOW,
        )
        .unwrap();
    }
    expected_hashes.sort_unstable();
    expected_hashes.remove(0);
    let mut retained = conn
        .prepare(
            "SELECT signed_hash FROM account_pre_verify
                 WHERE claimed_account_id = ?1 ORDER BY signed_hash",
        )
        .unwrap()
        .query_map(params![account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|row| id::fixed(&row.unwrap()).unwrap())
        .collect::<Vec<_>>();
    retained.sort_unstable();
    assert_eq!(retained, expected_hashes);
}

#[test]
fn account_ingest_reports_the_budget_that_evicted_its_pre_verify_row() {
    let founder = Dev::new(1);
    let (account_id, _genesis_bytes, genesis_hash) = genesis(&founder);
    let added = Dev::new(2);
    let (pending_bytes, _) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&added, DeviceRole::Owner),
    );

    let per_account = db();
    for ordinal in 0..PRE_VERIFY_PER_ACCOUNT_MAX {
        let raw = ordinal.to_be_bytes();
        assert_eq!(
            insert_pre_verify(
                &per_account,
                &cbor::sha256(&raw).into(),
                account_id,
                Dev::new(3).fp,
                &raw,
                NOW + 1,
            )
            .unwrap(),
            PreVerifyInsert::Parked { evicted: Vec::new() },
        );
    }
    assert_eq!(
        account_ingest(&per_account, &pending_bytes, NOW).unwrap(),
        IngestOutcome::CapacityReached { scope: CapacityScope::PreVerifyAccount },
    );

    let global = db();
    for account_byte in 0..PRE_VERIFY_GLOBAL_MAX / PRE_VERIFY_PER_ACCOUNT_MAX {
        let parked_account = AccountId::from_bytes([account_byte as u8; 32]);
        for ordinal in 0..PRE_VERIFY_PER_ACCOUNT_MAX {
            let raw = [account_byte as u8, ordinal as u8];
            assert_eq!(
                insert_pre_verify(
                    &global,
                    &cbor::sha256(&raw).into(),
                    parked_account,
                    Dev::new(4).fp,
                    &raw,
                    NOW + 1,
                )
                .unwrap(),
                PreVerifyInsert::Parked { evicted: Vec::new() },
            );
        }
    }
    assert_eq!(
        account_ingest(&global, &pending_bytes, NOW).unwrap(),
        IngestOutcome::CapacityReached { scope: CapacityScope::PreVerifyGlobal },
    );
}

#[test]
fn newer_pre_verify_admission_surfaces_collateral_oldest_eviction() {
    let conn = db();
    let founder = Dev::new(1);
    let (account_id, _genesis_bytes, genesis_hash) = genesis(&founder);
    let added = Dev::new(2);
    let (pending_bytes, _) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&added, DeviceRole::Owner),
    );
    let mut oldest_signed_hash = None;
    for ordinal in 0..PRE_VERIFY_PER_ACCOUNT_MAX {
        let raw = ordinal.to_be_bytes();
        oldest_signed_hash.get_or_insert_with(|| cbor::sha256(&raw));
        assert_eq!(
            insert_pre_verify(
                &conn,
                &cbor::sha256(&raw).into(),
                account_id,
                Dev::new(3).fp,
                &raw,
                NOW + i64::try_from(ordinal).unwrap(),
            )
            .unwrap(),
            PreVerifyInsert::Parked { evicted: Vec::new() },
        );
    }

    assert_eq!(
        account_ingest(&conn, &pending_bytes, NOW + 1_000).unwrap(),
        IngestOutcome::PreVerifyWithEviction { scopes: vec![CapacityScope::PreVerifyAccount] },
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_pre_verify WHERE claimed_account_id = ?1",
            params![account_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, PRE_VERIFY_PER_ACCOUNT_MAX as i64);
    assert!(PRE_VERIFY.contains(&conn, &cbor::sha256(&pending_bytes).into()).unwrap());
    assert!(!PRE_VERIFY.contains(&conn, &oldest_signed_hash.unwrap().into()).unwrap());
}

#[test]
fn candidate_ceiling_rejects_new_history_before_refold_work_grows_unbounded() {
    let conn = db();
    let founder = Dev::new(1);
    let (account_id, genesis_bytes, _) = genesis(&founder);
    let verified =
        envelope::verify_account_signed(&genesis_bytes, &founder.secret.public()).unwrap();
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    seed_candidate_rows(&tx, account_id, founder.fp, 0, CANDIDATES_PER_ACCOUNT_MAX);
    assert_eq!(
        insert_candidate(&tx, &verified, &genesis_bytes, NOW).unwrap(),
        CandidateInsert::AtCapacity(CapacityScope::CandidateAccount),
    );
    tx.commit().unwrap();
    assert_eq!(
        account_ingest(&conn, &genesis_bytes, NOW).unwrap(),
        IngestOutcome::CapacityReached { scope: CapacityScope::CandidateAccount },
    );
}

#[test]
fn candidate_byte_budgets_bound_refold_materialization_even_below_count_limits() {
    let conn = db();
    let founder = Dev::new(1);
    let (account_id, genesis_bytes, _) = genesis(&founder);
    let verified =
        envelope::verify_account_signed(&genesis_bytes, &founder.secret.public()).unwrap();

    seed_candidate_rows(&conn, account_id, founder.fp, 0, 1);
    conn.execute("UPDATE account_entries SET signed_bytes = zeroblob(?1)", [i64::try_from(
        CANDIDATE_BYTES_PER_ACCOUNT_MAX - genesis_bytes.len() + 1,
    )
    .unwrap()])
        .unwrap();
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    assert_eq!(
        insert_candidate(&tx, &verified, &genesis_bytes, NOW).unwrap(),
        CandidateInsert::AtCapacity(CapacityScope::CandidateAccountBytes),
    );
    tx.rollback().unwrap();

    let conn = db();
    let other_account = AccountId::from_bytes([0x55; 32]);
    seed_candidate_rows(&conn, other_account, founder.fp, 0, 1);
    conn.execute("UPDATE account_entries SET signed_bytes = zeroblob(?1)", [i64::try_from(
        CANDIDATE_BYTES_GLOBAL_MAX - genesis_bytes.len() + 1,
    )
    .unwrap()])
        .unwrap();
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    assert_eq!(
        insert_candidate(&tx, &verified, &genesis_bytes, NOW).unwrap(),
        CandidateInsert::AtCapacity(CapacityScope::CandidateGlobalBytes),
    );
    tx.rollback().unwrap();
}

#[test]
fn global_candidate_ceiling_bounds_many_attacker_created_accounts() {
    let conn = db();
    seed_global_candidate_rows(&conn, CANDIDATES_GLOBAL_MAX, 0);

    let founder = Dev::new(1);
    let (_account_id, genesis_bytes, _) = genesis(&founder);
    assert_eq!(
        account_ingest(&conn, &genesis_bytes, NOW).unwrap(),
        IngestOutcome::CapacityReached { scope: CapacityScope::CandidateGlobal },
    );
    let stored: i64 =
        conn.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(stored, CANDIDATES_GLOBAL_MAX as i64);
}

#[test]
fn concurrent_candidate_admission_cannot_overshoot_the_global_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("account-cap.db");
    let setup = Connection::open(&path).unwrap();
    schema::apply(&setup, &crate::test_hooks()).unwrap();
    let tx = Transaction::new_unchecked(&setup, TransactionBehavior::Immediate).unwrap();
    seed_global_candidate_rows(&tx, CANDIDATES_GLOBAL_MAX - 1, 0);
    tx.commit().unwrap();
    drop(setup);

    let first = genesis(&Dev::new(1)).1;
    let second = genesis(&Dev::new(2)).1;
    let barrier = Arc::new(Barrier::new(2));
    let handles = [first, second].map(|bytes| {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            let conn = Connection::open(path).unwrap();
            conn.busy_timeout(Duration::from_secs(5)).unwrap();
            barrier.wait();
            account_ingest(&conn, &bytes, NOW).unwrap()
        })
    });
    let outcomes = handles.map(|handle| handle.join().unwrap());
    assert_eq!(
        outcomes.iter().filter(|outcome| matches!(outcome, IngestOutcome::Ingested { .. })).count(),
        1,
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, IngestOutcome::CapacityReached {
                scope: CapacityScope::CandidateGlobal,
            }))
            .count(),
        1,
    );
    let conn = Connection::open(path).unwrap();
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(count, CANDIDATES_GLOBAL_MAX as i64);
}

#[test]
fn a_bad_known_key_signature_is_rejected_without_waiting_for_the_writer_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known-key.db");
    let setup = Connection::open(&path).unwrap();
    schema::apply(&setup, &crate::test_hooks()).unwrap();
    let founder = Dev::new(1);
    let (_account_id, mut forged, _) = genesis(&founder);
    account_ingest(&setup, &forged, NOW).unwrap();
    *forged.last_mut().unwrap() ^= 1;

    let holder = Connection::open(&path).unwrap();
    let tx = Transaction::new_unchecked(&holder, TransactionBehavior::Immediate).unwrap();
    let contender = Connection::open(&path).unwrap();
    contender.busy_timeout(Duration::ZERO).unwrap();
    assert!(matches!(
        account_ingest(&contender, &forged, NOW + 1).unwrap(),
        IngestOutcome::Rejected(_),
    ));
    tx.rollback().unwrap();
}

#[test]
fn ingest_reports_valid_promotions_rejected_at_terminal_global_capacity() {
    let conn = db();
    let founder = Dev::new(1);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let pending_device = Dev::new(2);
    let (pending_bytes, pending_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&pending_device, DeviceRole::Owner),
    );
    assert_eq!(
            insert_pre_verify(
                &conn,
                &pending_hash.into(),
                account_id,
                founder.fp,
                &pending_bytes,
                NOW,
            )
            .unwrap(),
            PreVerifyInsert::Parked { evicted: Vec::new() },
        );

    // Leave exactly one global slot. The trigger occupies it, so promotion must remove and
    // identify the valid row that cannot enter terminal grow-only candidate storage.
    seed_global_candidate_rows(&conn, CANDIDATES_GLOBAL_MAX - 2, 100);
    let trigger_device = Dev::new(3);
    let (trigger_bytes, trigger_hash) = op(
        account_id,
        &founder,
        2,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&trigger_device, DeviceRole::Member),
    );
    let outcome = account_ingest(&conn, &trigger_bytes, NOW + 1).unwrap();
    assert_eq!(outcome, IngestOutcome::Ingested {
        status: "forked".into(),
        account_promotions: PromotionOutcome {
            scope: Some(CapacityScope::CandidateGlobal),
            entry_hashes: vec![AccountEntryHash::from_bytes(pending_hash)]
        },
        content_promotions: content::ContentPromotionOutcome::default(),
    },);
    assert!(!PRE_VERIFY.contains(&conn, &cbor::sha256(&pending_bytes).into()).unwrap());
    assert_eq!(status(&conn, &trigger_hash), Some("forked".into()));
    assert_eq!(status(&conn, &pending_hash), None, "the rejected row was not half-promoted");
}

#[test]
fn exact_candidate_redelivery_is_idempotent_at_capacity_but_a_re_signature_is_verified() {
    let conn = db();
    let founder = Dev::new(1);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    assert_eq!(account_ingest(&conn, &genesis_bytes, NOW).unwrap(), IngestOutcome::Ingested {
        status: "accepted".into(),
        account_promotions: PromotionOutcome::default(),
        content_promotions: content::ContentPromotionOutcome::default(),
    },);
    seed_candidate_rows(&conn, account_id, founder.fp, 1, CANDIDATES_PER_ACCOUNT_MAX - 1);
    assert_eq!(account_ingest(&conn, &genesis_bytes, NOW + 1).unwrap(), IngestOutcome::Ingested {
        status: "accepted".into(),
        account_promotions: PromotionOutcome::default(),
        content_promotions: content::ContentPromotionOutcome::default()
    },);

    let mut forged_envelope = genesis_bytes.clone();
    *forged_envelope.last_mut().unwrap() ^= 1;
    let forged = envelope::decode_account_signed(&forged_envelope).unwrap();
    assert_eq!(
        forged.entry_hash,
        AccountEntryHash::from_bytes(genesis_hash),
        "signature bytes are outside the entry hash"
    );
    assert!(matches!(
        account_ingest(&conn, &forged_envelope, NOW + 2).unwrap(),
        IngestOutcome::Rejected(_)
    ));
}

#[test]
fn exact_redelivery_repairs_a_missing_projection_row() {
    let conn = db();
    let founder = Dev::new(1);
    let (_account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    conn.execute("DELETE FROM account_entry_status WHERE entry_hash = ?1", params![
        genesis_hash.as_slice()
    ])
    .unwrap();

    assert_eq!(account_ingest(&conn, &genesis_bytes, NOW + 1).unwrap(), IngestOutcome::Ingested {
        status: "accepted".into(),
        account_promotions: PromotionOutcome::default(),
        content_promotions: content::ContentPromotionOutcome::default()
    },);
    assert_eq!(status(&conn, &genesis_hash), Some("accepted".into()));
}

#[test]
fn exact_redelivery_reports_an_unrecognized_stored_status_verbatim() {
    // `account_entry_status.status` is unconstrained TEXT: a redelivery reports whatever token
    // is stored, including one this build does not know, instead of failing the ingest.
    let conn = db();
    let (_account_id, genesis_bytes, genesis_hash) = genesis(&Dev::new(1));
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    conn.execute(
        "UPDATE account_entry_status SET status = 'future_status' WHERE entry_hash = ?1",
        params![genesis_hash.as_slice()],
    )
    .unwrap();
    assert_eq!(account_ingest(&conn, &genesis_bytes, NOW + 1).unwrap(), IngestOutcome::Ingested {
        status: "future_status".into(),
        account_promotions: PromotionOutcome::default(),
        content_promotions: content::ContentPromotionOutcome::default()
    },);
}

#[test]
fn capacity_blocked_promotion_returns_redelivery_hash_and_clears_terminal_queue_state() {
    let conn = db();
    let founder = Dev::new(1);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let added = Dev::new(2);
    let (pending_bytes, pending_hash) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&added, DeviceRole::Owner),
    );
    assert_eq!(
            insert_pre_verify(
                &conn,
                &pending_hash.into(),
                account_id,
                founder.fp,
                &pending_bytes,
                NOW,
            )
            .unwrap(),
            PreVerifyInsert::Parked { evicted: Vec::new() },
        );
    seed_candidate_rows(&conn, account_id, founder.fp, 2, CANDIDATES_PER_ACCOUNT_MAX - 1);

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    assert_eq!(promote_pre_verify(&tx, account_id, NOW + 1).unwrap(), PromotionOutcome {
        scope: Some(CapacityScope::CandidateAccount),
        entry_hashes: vec![AccountEntryHash::from_bytes(pending_hash)],
    },);
    tx.commit().unwrap();
    assert!(!PRE_VERIFY.contains(&conn, &cbor::sha256(&pending_bytes).into()).unwrap());
    assert_eq!(status(&conn, &pending_hash), None, "the blocked row was not half-promoted");
}

#[test]
fn promotion_at_the_last_slot_is_stable_across_opposite_arrival_orders() {
    let run = |reverse: bool| {
        let conn = db();
        let founder = Dev::new(1);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        let a = Dev::new(2);
        let b = Dev::new(3);
        let first = op(
            account_id,
            &founder,
            1,
            Some(genesis_hash),
            Some(OwnerId::from_bytes(genesis_hash)),
            &device_add(&a, DeviceRole::Member),
        );
        let second = op(
            account_id,
            &founder,
            2,
            Some(genesis_hash),
            Some(OwnerId::from_bytes(genesis_hash)),
            &device_add(&b, DeviceRole::Member),
        );
        let rows = if reverse { [&second, &first] } else { [&first, &second] };
        for (bytes, hash) in rows {
            assert_eq!(
                insert_pre_verify(&conn, &(*(hash)).into(), account_id, founder.fp, bytes, NOW)
                    .unwrap(),
                PreVerifyInsert::Parked { evicted: Vec::new() },
            );
        }
        // One free slot at the cap ORDINARY traffic reaches — the view-manifest floor above it is
        // not a slot a promoted control entry may take.
        seed_candidate_rows(
            &conn,
            account_id,
            founder.fp,
            50,
            ORDINARY_CANDIDATES_PER_ACCOUNT_MAX - 2,
        );
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let outcome = promote_pre_verify(&tx, account_id, NOW + 1).unwrap();
        tx.commit().unwrap();
        let admitted = [first.1, second.1]
            .into_iter()
            .find(|hash| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM account_entries WHERE entry_hash = ?1)",
                    params![hash.as_slice()],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
            })
            .unwrap();
        (admitted, outcome)
    };

    let forward = run(false);
    let reverse = run(true);
    assert_eq!(forward, reverse);
    assert_eq!(forward.1.scope, Some(CapacityScope::CandidateAccount));
    assert_eq!(forward.1.entry_hashes.len(), 1);
}

#[test]
fn a_depth_two_pre_verify_chain_promotes_to_a_fixpoint_when_the_root_arrives() {
    // A device chain delivered before its authorizers: founder→B (DeviceAdd signed by the
    // founder) →C (DeviceAdd signed by B). Deliver in REVERSE so both the C-add and the B-add
    // park. The genesis arrival resolves the founder and promotes the B-add; the newly added B
    // key must then resolve the parked C-add in the SAME drain (the fixpoint). A one-shot
    // device-set snapshot would strand the C-add forever — a peer that received the three
    // entries in forward order would accept it, so the two peers would diverge.
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    let b = Dev::new(2);
    let (add_b_bytes, add_b) = op(
        acct,
        &founder,
        1,
        Some(gh),
        Some(OwnerId::from_bytes(gh)),
        &device_add(&b, DeviceRole::Owner),
    );
    // C added by owner B on B's own log (seq 0), authorized by the entry that made B an owner.
    let c = Dev::new(3);
    let (add_c_bytes, add_c) = op(
        acct,
        &b,
        0,
        None,
        Some(OwnerId::from_bytes(add_b)),
        &device_add(&c, DeviceRole::Member),
    );

    // Reverse delivery: the C-add parks (B unknown), then the B-add parks (founder unknown).
    assert_eq!(account_ingest(&conn, &add_c_bytes, NOW).unwrap(), IngestOutcome::PreVerify);
    assert_eq!(account_ingest(&conn, &add_b_bytes, NOW).unwrap(), IngestOutcome::PreVerify);

    // The genesis resolves the founder → promotes add_b → the fed-back B key promotes add_c.
    account_ingest(&conn, &gbytes, NOW).unwrap();
    assert_eq!(status(&conn, &gh).as_deref(), Some("accepted"), "genesis accepted");
    assert_eq!(status(&conn, &add_b).as_deref(), Some("accepted"), "the B-add promoted (depth 1)");
    assert_eq!(
        status(&conn, &add_c).as_deref(),
        Some("accepted"),
        "the C-add promoted transitively (depth 2 — the fixpoint drain)",
    );
    let pending: i64 =
        conn.query_row("SELECT COUNT(*) FROM account_pre_verify", [], |r| r.get(0)).unwrap();
    assert_eq!(pending, 0, "the pre-verify queue is fully drained");
}

#[test]
fn a_cut_promotes_its_named_branch_regardless_of_arrival_order_p8() {
    // B equivocates at seq 0 (two heads b0a/b0b). A DeviceRemove(B) names b0b as its watermark:
    // the register condemns the OFF-branch head (b0a) and keeps b0b — promoting the cut's
    // chosen branch over the unforced-fork hash tiebreak. The verdict is identical
    // whichever head (or the cut) arrives first (P8 arrival-independence, I10a holds
    // throughout).
    for order in 0..3u8 {
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        let b = Dev::new(2);
        let (add_bytes, add_b) = op(
            acct,
            &founder,
            1,
            Some(gh),
            Some(OwnerId::from_bytes(gh)),
            &device_add(&b, DeviceRole::Owner),
        );
        // B's two equivocating seq-0 heads (each adds a throwaway member so they differ).
        let (t8, t9) = (Dev::new(8), Dev::new(9));
        let (b0a_bytes, b0a) = op(
            acct,
            &b,
            0,
            None,
            Some(OwnerId::from_bytes(add_b)),
            &device_add(&t8, DeviceRole::Member),
        );
        let (b0b_bytes, b0b) = op(
            acct,
            &b,
            0,
            None,
            Some(OwnerId::from_bytes(add_b)),
            &device_add(&t9, DeviceRole::Member),
        );
        // The founder removes B, watermark = b0b (keep b0b's branch).
        let (rm_bytes, _rm) = op(
            acct,
            &founder,
            2,
            Some(add_b),
            Some(OwnerId::from_bytes(gh)),
            &device_remove(&b, super::super::cut::Cut::At {
                seq: 0,
                hash: AccountEntryHash::from_bytes(b0b),
            }),
        );

        // Ingest genesis + add_b first (so B resolves), then the three in a rotated order.
        account_ingest(&conn, &gbytes, NOW).unwrap();
        account_ingest(&conn, &add_bytes, NOW).unwrap();
        let mut rest = [&b0a_bytes, &b0b_bytes, &rm_bytes];
        rest.rotate_left(order as usize);
        for bytes in rest {
            account_ingest(&conn, bytes, NOW).unwrap();
        }

        assert_eq!(
            status(&conn, &b0b).as_deref(),
            Some("accepted"),
            "the cut-named branch b0b is accepted (order {order})",
        );
        assert_eq!(
            status(&conn, &b0a).as_deref(),
            Some("condemned"),
            "the off-branch head b0a is condemned (order {order})",
        );
    }
}

#[test]
fn late_cut_and_extend_atomically_remove_then_restore_authority_shadow_rows() {
    let conn = db();
    let (founder, owner, d, e, g) =
        (Dev::new(1), Dev::new(2), Dev::new(5), Dev::new(6), Dev::new(7));
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (add_owner_bytes, add_owner) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&owner, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_owner_bytes, NOW + 1).unwrap();
    let (b0_bytes, b0) = op(
        account_id,
        &owner,
        0,
        None,
        Some(OwnerId::from_bytes(add_owner)),
        &device_add(&d, DeviceRole::Member),
    );
    let (b1_bytes, b1) = op(
        account_id,
        &owner,
        1,
        Some(b0),
        Some(OwnerId::from_bytes(add_owner)),
        &device_add(&e, DeviceRole::Member),
    );
    let (b2_bytes, b2) = op(
        account_id,
        &owner,
        2,
        Some(b1),
        Some(OwnerId::from_bytes(add_owner)),
        &device_add(&g, DeviceRole::Member),
    );
    for (offset, bytes) in [&b0_bytes, &b1_bytes, &b2_bytes].into_iter().enumerate() {
        account_ingest(&conn, bytes, NOW + 2 + i64::try_from(offset).unwrap()).unwrap();
    }
    assert!(matches!(
        roster_ref_effective(&conn, account_id, RosterRef::from_bytes(b2), g.fp).unwrap(),
        fold::AuthorityQuery::Effective(_)
    ));

    let (remove_bytes, remove_hash) = op(
        account_id,
        &founder,
        2,
        Some(add_owner),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_remove(&owner, super::super::cut::Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes(b0),
        }),
    );
    account_ingest(&conn, &remove_bytes, NOW + 5).unwrap();
    assert_eq!(status(&conn, &b1).as_deref(), Some("condemned"));
    assert_eq!(status(&conn, &b2).as_deref(), Some("condemned"));
    assert_eq!(
        roster_ref_effective(&conn, account_id, RosterRef::from_bytes(b2), g.fp).unwrap(),
        fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::ReferencedEntryNotEffective),
    );

    let (extend_bytes, _) = op(
        account_id,
        &founder,
        3,
        Some(remove_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &cut_extend_ctrl(account_id, &owner, 2, AccountEntryHash::from_bytes(b2)),
    );
    account_ingest(&conn, &extend_bytes, NOW + 6).unwrap();
    assert_eq!(status(&conn, &b1).as_deref(), Some("accepted"));
    assert_eq!(status(&conn, &b2).as_deref(), Some("accepted"));
    assert!(matches!(
        roster_ref_effective(&conn, account_id, RosterRef::from_bytes(b2), g.fp).unwrap(),
        fold::AuthorityQuery::Effective(_)
    ));
    assert_eq!(
        owner_control_authority(&conn, account_id, OwnerId::from_bytes(add_owner), owner.fp)
            .unwrap(),
        fold::AuthorityQuery::Effective(fold::OwnerChainAuthority {
            owner: fold::OwnerAuthority { device_fingerprint: owner.fp },
            device_boundary: fold::AuthorityBoundary::Cut {
                seq: 2,
                hash: AccountEntryHash::from_bytes(b2)
            },
            incarnation_boundary: fold::AuthorityBoundary::Open,
        }),
        "the query exposes the final joined device register and the independent incarnation",
    );

    // Simulate an already-populated V064 projection upgrading: additive columns begin at
    // their legacy-safe Open default, then V065's same-transaction refold must replace them
    // before its ledger stamp commits.
    conn.execute("DELETE FROM schema_version WHERE id = '065_account_authority_boundaries'", [])
        .unwrap();
    conn.execute(
        "UPDATE account_roster_history
             SET control_boundary = 'open', control_seq = NULL, control_hash = NULL
             WHERE roster_ref = ?1",
        [add_owner.as_slice()],
    )
    .unwrap();
    schema::migrate_forward(&conn, &crate::test_hooks()).unwrap();
    assert!(matches!(
        owner_control_authority(&conn, account_id, OwnerId::from_bytes(add_owner), owner.fp).unwrap(),
        fold::AuthorityQuery::Effective(fold::OwnerChainAuthority {
            device_boundary: fold::AuthorityBoundary::Cut { seq: 2, hash },
            ..
        }) if hash == AccountEntryHash::from_bytes(b2)
    ));
}

#[test]
fn contested_fold_persists_the_state_before_depth_and_halts_authority_mutation() {
    let conn = db();
    let (founder, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (add_a_bytes, add_a) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&a, DeviceRole::Owner),
    );
    let (add_b_bytes, add_b) = op(
        account_id,
        &founder,
        2,
        Some(add_a),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&b, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_a_bytes, NOW + 1).unwrap();
    account_ingest(&conn, &add_b_bytes, NOW + 2).unwrap();
    let (remove_b_bytes, remove_b) = op(
        account_id,
        &a,
        0,
        None,
        Some(OwnerId::from_bytes(add_a)),
        &device_remove(&b, super::super::cut::Cut::Empty),
    );
    let (remove_a_bytes, remove_a) = op(
        account_id,
        &b,
        0,
        None,
        Some(OwnerId::from_bytes(add_b)),
        &device_remove(&a, super::super::cut::Cut::Empty),
    );
    account_ingest(&conn, &remove_b_bytes, NOW + 3).unwrap();
    account_ingest(&conn, &remove_a_bytes, NOW + 4).unwrap();

    let state: (String, Option<i64>, Option<Vec<u8>>, i64) = conn
        .query_row(
            "SELECT classification, contested_depth, successor_account_id, effective_count
                 FROM account_auth_state WHERE account_id = ?1",
            [account_id.to_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(state, ("contested".to_string(), Some(1), None, 3));
    assert_eq!(status(&conn, &remove_a).as_deref(), Some("parked"));
    assert_eq!(status(&conn, &remove_b).as_deref(), Some("parked"));
    let owner_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_owner_incarnations WHERE account_id = ?1",
            [account_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owner_count, 3, "only state-before owner incarnations are projected");
}

#[test]
fn owner_query_preserves_and_extends_both_registers_independently() {
    let conn = db();
    let founder = Dev::new(1);
    let owner = Dev::new(2);
    let (account, genesis_bytes, genesis) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (add_bytes, owner_id) = op(
        account,
        &founder,
        1,
        Some(genesis),
        Some(OwnerId::from_bytes(genesis)),
        &device_add(&owner, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_bytes, NOW + 1).unwrap();
    let mut prev = None;
    let mut heads = Vec::new();
    for (seq, seed) in [(0, 10), (1, 11), (2, 12)] {
        let member = Dev::new(seed);
        let (bytes, hash) = op(
            account,
            &owner,
            seq,
            prev,
            Some(OwnerId::from_bytes(owner_id)),
            &device_add(&member, DeviceRole::Member),
        );
        account_ingest(&conn, &bytes, NOW + 2 + i64::try_from(seq).unwrap()).unwrap();
        prev = Some(hash);
        heads.push(hash);
    }
    let demote = AccountOp::OwnerDemote {
        device_fingerprint: owner.fp,
        owner_id: OwnerId::from_bytes(owner_id),
        control_cut: super::super::cut::Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes(heads[0]),
        },
        secrets_cut: super::super::cut::Cut::Empty,
        reason: "demote".to_string(),
    };
    let (demote_bytes, demote_hash) =
        op(account, &founder, 2, Some(owner_id), Some(OwnerId::from_bytes(genesis)), &demote);
    account_ingest(&conn, &demote_bytes, NOW + 5).unwrap();
    let (remove_bytes, remove_hash) = op(
        account,
        &founder,
        3,
        Some(demote_hash),
        Some(OwnerId::from_bytes(genesis)),
        &device_remove(&owner, super::super::cut::Cut::At {
            seq: 1,
            hash: AccountEntryHash::from_bytes(heads[1]),
        }),
    );
    account_ingest(&conn, &remove_bytes, NOW + 6).unwrap();
    let expected = |device_boundary, incarnation_boundary| {
        fold::AuthorityQuery::Effective(fold::OwnerChainAuthority {
            owner: fold::OwnerAuthority { device_fingerprint: owner.fp },
            device_boundary,
            incarnation_boundary,
        })
    };
    assert_eq!(
        owner_control_authority(&conn, account, OwnerId::from_bytes(owner_id), owner.fp).unwrap(),
        expected(
            fold::AuthorityBoundary::Cut { seq: 1, hash: AccountEntryHash::from_bytes(heads[1]) },
            fold::AuthorityBoundary::Cut { seq: 0, hash: AccountEntryHash::from_bytes(heads[0]) },
        ),
    );
    let extend_incarnation = AccountOp::CutExtend {
        chain_kind: super::super::ops::ChainKind::Ctrl,
        stream_id: None,
        incarnation_id: Some(AccountEntryHash::from_bytes(owner_id)),
        subject_account_id: account,
        device_fingerprint: owner.fp,
        new_seq: 2,
        new_entry_hash: AccountEntryHash::from_bytes(heads[2]),
    };
    let (bytes, extend_hash) = op(
        account,
        &founder,
        4,
        Some(remove_hash),
        Some(OwnerId::from_bytes(genesis)),
        &extend_incarnation,
    );
    account_ingest(&conn, &bytes, NOW + 7).unwrap();
    assert_eq!(
        owner_control_authority(&conn, account, OwnerId::from_bytes(owner_id), owner.fp).unwrap(),
        expected(
            fold::AuthorityBoundary::Cut { seq: 1, hash: AccountEntryHash::from_bytes(heads[1]) },
            fold::AuthorityBoundary::Cut { seq: 2, hash: AccountEntryHash::from_bytes(heads[2]) },
        ),
        "extending the incarnation register leaves the device register unchanged",
    );
    let (bytes, _) = op(
        account,
        &founder,
        5,
        Some(extend_hash),
        Some(OwnerId::from_bytes(genesis)),
        &cut_extend_ctrl(account, &owner, 2, AccountEntryHash::from_bytes(heads[2])),
    );
    account_ingest(&conn, &bytes, NOW + 8).unwrap();
    assert_eq!(
        owner_control_authority(&conn, account, OwnerId::from_bytes(owner_id), owner.fp).unwrap(),
        expected(
            fold::AuthorityBoundary::Cut { seq: 2, hash: AccountEntryHash::from_bytes(heads[2]) },
            fold::AuthorityBoundary::Cut { seq: 2, hash: AccountEntryHash::from_bytes(heads[2]) },
        ),
        "extending the device register leaves the incarnation register unchanged",
    );
}

#[test]
fn an_equivocation_accepts_one_head_and_forks_the_other() {
    // The founder equivocates: two DIFFERENT seq-1 entries on its own chain. Both fold
    // effective, but only one can occupy the (device, seq) slot (I10a) — the smaller
    // entry_hash wins, the other is `forked`. The partial unique index would blow up if
    // refold accepted both.
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();
    let (b, c) = (Dev::new(2), Dev::new(3));
    let (b_bytes, b_hash) = op(
        acct,
        &founder,
        1,
        Some(gh),
        Some(OwnerId::from_bytes(gh)),
        &device_add(&b, DeviceRole::Member),
    );
    let (c_bytes, c_hash) = op(
        acct,
        &founder,
        1,
        Some(gh),
        Some(OwnerId::from_bytes(gh)),
        &device_add(&c, DeviceRole::Member),
    );
    account_ingest(&conn, &b_bytes, NOW).unwrap();
    account_ingest(&conn, &c_bytes, NOW).unwrap();

    let (winner, loser) = if b_hash < c_hash { (b_hash, c_hash) } else { (c_hash, b_hash) };
    assert_eq!(status(&conn, &winner).as_deref(), Some("accepted"), "smaller hash wins the slot");
    assert_eq!(status(&conn, &loser).as_deref(), Some("forked"), "the equivocation loser forks");
    // I10a: exactly one accepted entry at the (founder, seq 1) slot.
    let accepted_at_slot: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_entries
                 WHERE account_id = ?1 AND device_fingerprint = ?2 AND seq = 1 AND accepted = 1",
            params![acct.to_bytes().as_slice(), founder.fp.to_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(accepted_at_slot, 1, "one accepted head per slot (I10a)");
}

#[test]
fn losing_mint_authority_is_pruned_transitively_in_every_arrival_order() {
    // The founder equivocates between minting B and W. Whichever mint loses by hash must not
    // authorize an independent B chain: B mints C, then C mints D. Content-addressed key
    // discovery deliberately knows all three keys, so signature verification alone cannot
    // hide an authority-closure bug. The winning device also authors a child to prove pruning
    // is scoped to the losing incarnation rather than all newly minted devices.
    for rotation in 0..4 {
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();

        let (b, w, c, d, survivor_child) =
            (Dev::new(2), Dev::new(3), Dev::new(4), Dev::new(5), Dev::new(6));
        let (add_b_bytes, add_b) = op(
            acct,
            &founder,
            1,
            Some(gh),
            Some(OwnerId::from_bytes(gh)),
            &device_add(&b, DeviceRole::Owner),
        );
        let (add_w_bytes, add_w) = op(
            acct,
            &founder,
            1,
            Some(gh),
            Some(OwnerId::from_bytes(gh)),
            &device_add(&w, DeviceRole::Owner),
        );

        let (loser, loser_add_bytes, loser_add, winner, winner_add_bytes, winner_add) =
            if add_b < add_w {
                (&w, &add_w_bytes, add_w, &b, &add_b_bytes, add_b)
            } else {
                (&b, &add_b_bytes, add_b, &w, &add_w_bytes, add_w)
            };
        let (add_c_bytes, add_c) = op(
            acct,
            loser,
            0,
            None,
            Some(OwnerId::from_bytes(loser_add)),
            &device_add(&c, DeviceRole::Owner),
        );
        let (add_d_bytes, add_d) = op(
            acct,
            &c,
            0,
            None,
            Some(OwnerId::from_bytes(add_c)),
            &device_add(&d, DeviceRole::Member),
        );
        let (winner_child_bytes, winner_child) = op(
            acct,
            winner,
            0,
            None,
            Some(OwnerId::from_bytes(winner_add)),
            &device_add(&survivor_child, DeviceRole::Member),
        );

        let mut arrivals = [loser_add_bytes, winner_add_bytes, &add_c_bytes, &add_d_bytes];
        arrivals.rotate_left(rotation);
        for bytes in arrivals {
            account_ingest(&conn, bytes, NOW).unwrap();
        }
        account_ingest(&conn, &winner_child_bytes, NOW).unwrap();

        assert_eq!(status(&conn, &winner_add).as_deref(), Some("accepted"));
        assert_eq!(status(&conn, &winner_child).as_deref(), Some("accepted"));
        assert_eq!(status(&conn, &loser_add).as_deref(), Some("forked"));
        assert_eq!(status(&conn, &add_c).as_deref(), Some("forked"));
        assert_eq!(status(&conn, &add_d).as_deref(), Some("forked"));
    }
}

#[test]
fn roster_and_owner_projection_tracks_role_changes_and_closures() {
    let conn = db();
    let founder = Dev::new(1);
    let member = Dev::new(2);
    let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();

    let (add_bytes, add) = op(
        account_id,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&member, DeviceRole::Member),
    );
    account_ingest(&conn, &add_bytes, NOW + 1).unwrap();
    let (promote_bytes, promote) = op(
        account_id,
        &founder,
        2,
        Some(add),
        Some(OwnerId::from_bytes(genesis_hash)),
        &AccountOp::OwnerPromote { device_fingerprint: member.fp },
    );
    account_ingest(&conn, &promote_bytes, NOW + 2).unwrap();
    assert_eq!(
        roster_ref_effective(&conn, account_id, RosterRef::from_bytes(add), member.fp).unwrap(),
        fold::AuthorityQuery::Effective(fold::RosterAuthority {
            device_fingerprint: member.fp,
            current_role: DeviceRole::Owner,
        }),
    );

    let (demote_bytes, demote) = op(
        account_id,
        &founder,
        3,
        Some(promote),
        Some(OwnerId::from_bytes(genesis_hash)),
        &owner_demote(&member, OwnerId::from_bytes(promote)),
    );
    account_ingest(&conn, &demote_bytes, NOW + 3).unwrap();
    assert_eq!(
        roster_ref_effective(&conn, account_id, RosterRef::from_bytes(add), member.fp).unwrap(),
        fold::AuthorityQuery::Effective(fold::RosterAuthority {
            device_fingerprint: member.fp,
            current_role: DeviceRole::Member,
        }),
    );
    assert_eq!(
        owner_incarnation_effective(&conn, account_id, OwnerId::from_bytes(promote), member.fp)
            .unwrap(),
        fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::ReferencedEntryNotEffective,),
    );

    let (remove_bytes, _) = op(
        account_id,
        &founder,
        4,
        Some(demote),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_remove(&member, super::super::cut::Cut::Empty),
    );
    account_ingest(&conn, &remove_bytes, NOW + 4).unwrap();
    assert_eq!(
        roster_ref_effective(&conn, account_id, RosterRef::from_bytes(add), member.fp).unwrap(),
        fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::ReferencedEntryNotEffective,),
    );
}

#[test]
fn final_fold_does_not_keep_control_effects_from_a_losing_mint() {
    // The losing founder branch mints owner B. A demotion of B sits on the WINNING founder
    // branch, so the first all-candidate fold can resolve B's owner_id and treat the demotion
    // as effective. After branch elimination, the final fold must run without the
    // losing mint: the demotion can no longer resolve its target and must park without leaving
    // a control register or an accepted status behind. A post-hoc accepted-set filter would
    // fail this assertion.
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();
    let b = Dev::new(2);
    let (add_b_bytes, add_b) = op(
        acct,
        &founder,
        1,
        Some(gh),
        Some(OwnerId::from_bytes(gh)),
        &device_add(&b, DeviceRole::Owner),
    );
    // Find a deterministic rival whose hash is smaller, making the owner mint the losing
    // branch without depending on an assumed ordering of test keys.
    let (winner_bytes, winner_hash) = (3u8..=u8::MAX)
        .find_map(|seed| {
            let rival = Dev::new(seed);
            let candidate = op(
                acct,
                &founder,
                1,
                Some(gh),
                Some(OwnerId::from_bytes(gh)),
                &device_add(&rival, DeviceRole::Member),
            );
            (candidate.1 < add_b).then_some(candidate)
        })
        .expect("the deterministic fixture set contains a lower-hash rival");
    let (demote_bytes, demote) = op(
        acct,
        &founder,
        2,
        Some(winner_hash),
        Some(OwnerId::from_bytes(gh)),
        &owner_demote(&b, OwnerId::from_bytes(add_b)),
    );

    account_ingest(&conn, &add_b_bytes, NOW).unwrap();
    account_ingest(&conn, &winner_bytes, NOW).unwrap();
    account_ingest(&conn, &demote_bytes, NOW).unwrap();

    assert_eq!(status(&conn, &add_b).as_deref(), Some("forked"));
    assert_eq!(status(&conn, &winner_hash).as_deref(), Some("accepted"));
    assert_eq!(status(&conn, &demote).as_deref(), Some("parked"));
}

#[test]
fn an_entry_that_chains_from_a_forked_head_is_not_accepted() {
    // B equivocates at seq 0 (b0a/b0b), then authors a seq-1 entry whose prev_hash chains from
    // the LOSING head. The fold marks that seq-1 entry effective (its own authority is valid)
    // and it is the ONLY candidate at (B, seq 1) — yet accepting it would leave an accepted
    // entry whose parent is `forked`, a broken chain. Branch selection must fork the descendant
    // of a losing head, not just the losing head itself.
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();
    let b = Dev::new(2);
    let (add_b_bytes, add_b) = op(
        acct,
        &founder,
        1,
        Some(gh),
        Some(OwnerId::from_bytes(gh)),
        &device_add(&b, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_b_bytes, NOW).unwrap();
    // B's two equivocating seq-0 heads (each adds a distinct throwaway member so they differ).
    let (t8, t9) = (Dev::new(8), Dev::new(9));
    let (b0a_bytes, b0a) = op(
        acct,
        &b,
        0,
        None,
        Some(OwnerId::from_bytes(add_b)),
        &device_add(&t8, DeviceRole::Member),
    );
    let (b0b_bytes, b0b) = op(
        acct,
        &b,
        0,
        None,
        Some(OwnerId::from_bytes(add_b)),
        &device_add(&t9, DeviceRole::Member),
    );
    let ((win, win_bytes), (lose, lose_bytes)) = if b0a < b0b {
        ((b0a, &b0a_bytes), (b0b, &b0b_bytes))
    } else {
        ((b0b, &b0b_bytes), (b0a, &b0a_bytes))
    };
    // B continues at seq 1 from the LOSING head.
    let t7 = Dev::new(7);
    let (b1_bytes, b1) = op(
        acct,
        &b,
        1,
        Some(lose),
        Some(OwnerId::from_bytes(add_b)),
        &device_add(&t7, DeviceRole::Member),
    );

    account_ingest(&conn, win_bytes, NOW).unwrap();
    account_ingest(&conn, lose_bytes, NOW).unwrap();
    account_ingest(&conn, &b1_bytes, NOW).unwrap();

    assert_eq!(status(&conn, &win).as_deref(), Some("accepted"), "min-hash seq-0 head wins");
    assert_eq!(status(&conn, &lose).as_deref(), Some("forked"), "the losing seq-0 head forks");
    assert_eq!(
        status(&conn, &b1).as_deref(),
        Some("forked"),
        "a seq-1 entry chaining from the forked head is off-branch, not accepted",
    );
    // No accepted entry has a forked parent: nothing is accepted on B's dead branch at seq 1.
    let accepted_seq1: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_entries
                 WHERE account_id = ?1 AND device_fingerprint = ?2 AND seq = 1 AND accepted = 1",
            params![acct.to_bytes().as_slice(), b.fp.to_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(accepted_seq1, 0, "no accepted seq-1 entry on B's forked branch");
}

#[test]
fn a_malformed_op_payload_is_rejected_not_stored() {
    // A founder-signed DEVICE_ADD whose plaintext op payload is not decodable CBOR. The
    // envelope + signature verify (the payload rides opaque inside the envelope), but the op is
    // garbage — a structural reject that must never enter the grow-only DAG. Only ops::decode
    // catches it, so ingest must gate on it before storing.
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();
    let header = AccountEntryHeader {
        account_id: acct,
        log_id: 0,
        device_fingerprint: founder.fp,
        seq: 1,
        prev_hash: Some(AccountEntryHash::from_bytes(gh)),
        parent_ref: None,
        entry_type: ops::entry_type::DEVICE_ADD,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes(gh)),
    };
    let signed = sign_account_entry(&founder.secret, &header, &[0xff, 0xff, 0xff]).unwrap();
    let out = account_ingest(&conn, &signed.signed_bytes, NOW).unwrap();
    assert!(matches!(out, IngestOutcome::Rejected(_)), "a malformed op payload is rejected");
    assert_eq!(status(&conn, &signed.entry_hash.into()), None, "and is never stored");
}

#[test]
fn sealed_and_future_version_payloads_remain_opaque_through_ingest() {
    // C1 does not understand sealed ciphertext or future op versions. Both must remain valid
    // ancestry/watermark targets instead of being decoded with today's plaintext schema.
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();

    let opaque = [0xff, 0x00, 0xff]; // deliberately not a current DeviceAdd payload
    let sealed_header = AccountEntryHeader {
        account_id: acct,
        log_id: 0,
        device_fingerprint: founder.fp,
        seq: 1,
        prev_hash: Some(AccountEntryHash::from_bytes(gh)),
        parent_ref: None,
        entry_type: ops::entry_type::DEVICE_ADD,
        op_version: 1,
        crypto_suite: 1,
        auth_len: 1,
        key_id: Some([0x44; 32]),
        authority_ref: Some(OwnerId::from_bytes(gh)),
    };
    let sealed = sign_account_entry(&founder.secret, &sealed_header, &opaque).unwrap();
    assert_eq!(
        account_ingest(&conn, &sealed.signed_bytes, NOW).unwrap(),
        IngestOutcome::Ingested {
            status: "retained_unfolded".into(),
            account_promotions: PromotionOutcome::default(),
            content_promotions: content::ContentPromotionOutcome::default()
        },
    );

    let future_header = AccountEntryHeader {
        seq: 2,
        prev_hash: Some(sealed.entry_hash),
        op_version: 2,
        crypto_suite: 0,
        key_id: None,
        ..sealed_header
    };
    let future = sign_account_entry(&founder.secret, &future_header, &opaque).unwrap();
    assert_eq!(
        account_ingest(&conn, &future.signed_bytes, NOW).unwrap(),
        IngestOutcome::Ingested {
            status: "retained_unfolded".into(),
            account_promotions: PromotionOutcome::default(),
            content_promotions: content::ContentPromotionOutcome::default()
        },
    );
    assert_eq!(status(&conn, &sealed.entry_hash.into()).as_deref(), Some("retained_unfolded"));
    assert_eq!(status(&conn, &future.entry_hash.into()).as_deref(), Some("retained_unfolded"));
}

#[test]
fn malformed_payload_is_rejected_before_it_can_enter_pre_verify() {
    // Payload structure is independent of signer resolution. Reject it before parking so the
    // unauthenticated queue never contradicts the "structural rejects are never stored" rule.
    let conn = db();
    let founder = Dev::new(1);
    let (acct, _gbytes, gh) = genesis(&founder);
    let b = Dev::new(2);
    let malformed_header = AccountEntryHeader {
        account_id: acct,
        log_id: 0,
        device_fingerprint: b.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: ops::entry_type::DEVICE_ADD,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes(gh)),
    };
    let malformed = sign_account_entry(&b.secret, &malformed_header, &[0xff, 0xff, 0xff]).unwrap();
    assert!(matches!(
        account_ingest(&conn, &malformed.signed_bytes, NOW).unwrap(),
        IngestOutcome::Rejected(_)
    ));
    assert_eq!(status(&conn, &malformed.entry_hash.into()), None);
    let pending: i64 =
        conn.query_row("SELECT COUNT(*) FROM account_pre_verify", [], |row| row.get(0)).unwrap();
    assert_eq!(pending, 0, "malformed bytes are never parked");
}

#[test]
fn branch_selection_requires_contiguous_sequence_numbers() {
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();
    let b = Dev::new(2);
    let payload = ops::encode(&device_add(&b, DeviceRole::Member)).unwrap();
    let gap_header = AccountEntryHeader {
        account_id: acct,
        log_id: 0,
        device_fingerprint: founder.fp,
        seq: 2,
        prev_hash: Some(AccountEntryHash::from_bytes(gh)),
        parent_ref: None,
        entry_type: ops::entry_type::DEVICE_ADD,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes(gh)),
    };
    let gap = sign_account_entry(&founder.secret, &gap_header, &payload).unwrap();
    account_ingest(&conn, &gap.signed_bytes, NOW).unwrap();
    assert_eq!(status(&conn, &gap.entry_hash.into()).as_deref(), Some("forked"));

    let c = Dev::new(3);
    let (next_bytes, next) = op(
        acct,
        &founder,
        1,
        Some(gh),
        Some(OwnerId::from_bytes(gh)),
        &device_add(&c, DeviceRole::Member),
    );
    account_ingest(&conn, &next_bytes, NOW).unwrap();
    assert_eq!(status(&conn, &next).as_deref(), Some("accepted"));
    assert_eq!(
        status(&conn, &gap.entry_hash.into()).as_deref(),
        Some("forked"),
        "a seq-2 sibling of seq-1 cannot extend the accepted chain",
    );
}

#[test]
fn sequence_values_outside_sqlite_integer_are_rejected() {
    let conn = db();
    let founder = Dev::new(1);
    let (acct, _gbytes, gh) = genesis(&founder);
    let b = Dev::new(2);
    let payload = ops::encode(&device_add(&b, DeviceRole::Member)).unwrap();
    let header = AccountEntryHeader {
        account_id: acct,
        log_id: 0,
        device_fingerprint: founder.fp,
        seq: i64::MAX as u64 + 1,
        prev_hash: Some(AccountEntryHash::from_bytes(gh)),
        parent_ref: None,
        entry_type: ops::entry_type::DEVICE_ADD,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes(gh)),
    };
    let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
    assert_eq!(
        account_ingest(&conn, &signed.signed_bytes, NOW).unwrap(),
        IngestOutcome::Rejected("account seq exceeds SQLite INTEGER range".into()),
    );
}

#[test]
fn competing_pre_verify_signatures_are_retained_until_one_verifies() {
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    let b = Dev::new(2);
    let (valid_bytes, entry_hash) = op(
        acct,
        &founder,
        1,
        Some(gh),
        Some(OwnerId::from_bytes(gh)),
        &device_add(&b, DeviceRole::Owner),
    );
    let mut bad_bytes = valid_bytes.clone();
    *bad_bytes.last_mut().unwrap() ^= 1; // signature byte; body/entry_hash stays identical

    assert_eq!(account_ingest(&conn, &bad_bytes, NOW).unwrap(), IngestOutcome::PreVerify);
    assert_eq!(account_ingest(&conn, &valid_bytes, NOW).unwrap(), IngestOutcome::PreVerify);
    let parked: i64 =
        conn.query_row("SELECT COUNT(*) FROM account_pre_verify", [], |row| row.get(0)).unwrap();
    assert_eq!(parked, 2, "both signed envelopes for one entry body are retained");

    account_ingest(&conn, &gbytes, NOW).unwrap();
    assert_eq!(status(&conn, &entry_hash).as_deref(), Some("accepted"));
    let parked: i64 =
        conn.query_row("SELECT COUNT(*) FROM account_pre_verify", [], |row| row.get(0)).unwrap();
    assert_eq!(parked, 0, "both the refuted and promoted envelopes are drained");
}

#[test]
fn forged_genesis_is_rejected_before_pre_verify() {
    let conn = db();
    let founder = Dev::new(1);
    let (acct, real_bytes, _gh) = genesis(&founder);
    let attacker = Dev::new(9);
    let genesis_payload = envelope::decode_account_signed(&real_bytes).unwrap().payload;
    let forged_header = AccountEntryHeader {
        account_id: acct,
        log_id: 0,
        device_fingerprint: attacker.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: ops::entry_type::ACCOUNT_GENESIS,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 0,
        key_id: None,
        authority_ref: None,
    };
    let forged = sign_account_entry(&attacker.secret, &forged_header, &genesis_payload).unwrap();
    assert!(matches!(
        account_ingest(&conn, &forged.signed_bytes, NOW).unwrap(),
        IngestOutcome::Rejected(_)
    ));
    assert_eq!(
        status(&conn, &forged.entry_hash.into()),
        None,
        "founder binding refutes the forgery"
    );
    let parked: i64 =
        conn.query_row("SELECT COUNT(*) FROM account_pre_verify", [], |row| row.get(0)).unwrap();
    assert_eq!(parked, 0);
}

#[test]
fn opaque_device_add_cannot_self_resolve_or_seed_key_discovery() {
    let conn = db();
    let founder = Dev::new(1);
    let (acct, _gbytes, gh) = genesis(&founder);
    let b = Dev::new(2);
    let payload = ops::encode(&device_add(&b, DeviceRole::Owner)).unwrap();
    let header = AccountEntryHeader {
        account_id: acct,
        log_id: 0,
        device_fingerprint: b.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: ops::entry_type::DEVICE_ADD,
        op_version: 2,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes(gh)),
    };
    let opaque = sign_account_entry(&b.secret, &header, &payload).unwrap();
    assert_eq!(account_ingest(&conn, &opaque.signed_bytes, NOW).unwrap(), IngestOutcome::PreVerify);
    assert_eq!(status(&conn, &opaque.entry_hash.into()), None);
}

#[test]
fn non_control_payload_is_retained_without_control_schema_decode() {
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();
    // A log-1 entry carrying the control STREAM_OWN tag: on the SECRETS log that number is an
    // unknown secrets tag, so the control STREAM_OWN schema is never applied. C4.2b's secrets
    // twin DOES validate log-1 plaintext at ingest, but only as one canonical CBOR item (the
    // same opaque-retention rule as an unknown control tag) — a canonical payload is retained
    // `retained_unfolded`, never decoded against any op schema.
    let header = AccountEntryHeader {
        account_id: acct,
        log_id: 1,
        device_fingerprint: founder.fp,
        seq: 1,
        prev_hash: Some(AccountEntryHash::from_bytes(gh)),
        parent_ref: None,
        entry_type: ops::entry_type::STREAM_OWN,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes(gh)),
    };
    let signed = sign_account_entry(&founder.secret, &header, &[0x80]).unwrap();
    assert_eq!(
        account_ingest(&conn, &signed.signed_bytes, NOW).unwrap(),
        IngestOutcome::Ingested {
            status: "retained_unfolded".into(),
            account_promotions: PromotionOutcome::default(),
            content_promotions: content::ContentPromotionOutcome::default()
        },
    );
}

#[test]
fn a_non_canonical_secrets_plaintext_payload_is_rejected_at_ingest() {
    // The secrets twin (C4.2b) rejects a non-canonical log-1 plaintext payload at ingest,
    // exactly as an unknown control tag with a non-canonical payload is rejected — a
    // cross-version consensus split on a signed log is the hazard the canonicity rule
    // closes.
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();
    let header = AccountEntryHeader {
        account_id: acct,
        log_id: 1,
        device_fingerprint: founder.fp,
        seq: 1,
        prev_hash: Some(AccountEntryHash::from_bytes(gh)),
        parent_ref: None,
        entry_type: ops::entry_type::DEVICE_ADD,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes(gh)),
    };
    let signed = sign_account_entry(&founder.secret, &header, &[0xff, 0x00]).unwrap();
    assert!(
        matches!(
            account_ingest(&conn, &signed.signed_bytes, NOW).unwrap(),
            IngestOutcome::Rejected(_)
        ),
        "a non-canonical secrets plaintext payload is a structural reject",
    );
    assert_eq!(status(&conn, &signed.entry_hash.into()), None, "a rejected entry is not stored");
}

#[test]
fn refold_is_idempotent_and_preserves_the_selected_branch() {
    let conn = db();
    let founder = Dev::new(1);
    let (acct, gbytes, gh) = genesis(&founder);
    account_ingest(&conn, &gbytes, NOW).unwrap();
    let b = Dev::new(2);
    let (add_bytes, add_hash) = op(
        acct,
        &founder,
        1,
        Some(gh),
        Some(OwnerId::from_bytes(gh)),
        &device_add(&b, DeviceRole::Owner),
    );
    account_ingest(&conn, &add_bytes, NOW).unwrap();

    refold_account(&conn, acct).unwrap();
    refold_account(&conn, acct).unwrap();

    assert_eq!(status(&conn, &gh).as_deref(), Some("accepted"));
    assert_eq!(status(&conn, &add_hash).as_deref(), Some("accepted"));
    let accepted: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_entries WHERE account_id = ?1 AND accepted = 1",
            params![acct.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(accepted, 2, "repeated clear-and-set refolds keep the same accepted chain");
}

#[test]
fn removed_roster_citation_keeps_only_its_per_stream_prefix() {
    let conn = db();
    let founder = Dev::new(1);
    let member = Dev::new(2);
    let (account, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (add_bytes, roster_ref) = op(
        account,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&member, DeviceRole::Member),
    );
    account_ingest(&conn, &add_bytes, NOW + 1).unwrap();
    let listed = StreamId::from_bytes([0x41; 32]);
    let unlisted = StreamId::from_bytes([0x42; 32]);
    let remove = AccountOp::DeviceRemove {
        device_fingerprint: member.fp,
        control_cut: super::super::cut::Cut::Empty,
        secrets_cut: super::super::cut::Cut::Empty,
        content_cuts: vec![ContentCut {
            stream_id: listed,
            seq: u64::MAX,
            hash: AccountEntryHash::from_bytes([0xa5; 32]),
        }],
        reason: "revoked".to_string(),
    };
    let (remove_bytes, _) = op(
        account,
        &founder,
        2,
        Some(roster_ref),
        Some(OwnerId::from_bytes(genesis_hash)),
        &remove,
    );
    account_ingest(&conn, &remove_bytes, NOW + 2).unwrap();

    assert_eq!(
        roster_content_authority(
            &conn,
            account,
            RosterRef::from_bytes(roster_ref),
            member.fp,
            listed
        )
        .unwrap(),
        fold::AuthorityQuery::Effective(fold::RosterContentAuthority {
            device_fingerprint: member.fp,
            role: DeviceRole::Member,
            boundary: fold::AuthorityBoundary::Cut {
                seq: u64::MAX,
                hash: AccountEntryHash::from_bytes([0xa5; 32])
            },
        }),
    );
    assert_eq!(
        roster_content_authority(
            &conn,
            account,
            RosterRef::from_bytes(roster_ref),
            member.fp,
            unlisted
        )
        .unwrap(),
        fold::AuthorityQuery::Effective(fold::RosterContentAuthority {
            device_fingerprint: member.fp,
            role: DeviceRole::Member,
            boundary: fold::AuthorityBoundary::Closed,
        }),
        "an omitted content chain is the empty cut, never open",
    );
}

#[test]
fn roster_content_authority_carries_the_read_only_role() {
    // A ReadOnly device folds onto the roster (read is role-blind), and the storage reader that
    // the fold consults must surface its role — the thread the content gate rejects on. If this
    // regressed to dropping the role, `authority_verdict` would silently admit read-only
    // content.
    let conn = db();
    let founder = Dev::new(1);
    let reader = Dev::new(2);
    let (account, genesis_bytes, genesis_hash) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    let (add_bytes, roster_ref) = op(
        account,
        &founder,
        1,
        Some(genesis_hash),
        Some(OwnerId::from_bytes(genesis_hash)),
        &device_add(&reader, DeviceRole::ReadOnly),
    );
    account_ingest(&conn, &add_bytes, NOW + 1).unwrap();
    let stream = StreamId::from_bytes([0x41; 32]);
    match roster_content_authority(
        &conn,
        account,
        RosterRef::from_bytes(roster_ref),
        reader.fp,
        stream,
    )
    .unwrap()
    {
        fold::AuthorityQuery::Effective(fact) => {
            assert_eq!(fact.device_fingerprint, reader.fp);
            assert_eq!(
                fact.role,
                DeviceRole::ReadOnly,
                "the read-only role must reach the content-authority fact",
            );
        },
        other => panic!("expected an effective read-only roster fact, got {other:?}"),
    }
}

#[test]
fn malformed_persisted_owner_boundary_fails_closed() {
    let conn = db();
    let founder = Dev::new(1);
    let (account, genesis_bytes, owner_id) = genesis(&founder);
    account_ingest(&conn, &genesis_bytes, NOW).unwrap();
    conn.execute(
        "UPDATE account_owner_incarnations
             SET control_boundary = 'cut', control_seq = NULL, control_hash = NULL
             WHERE owner_id = ?1",
        [owner_id.as_slice()],
    )
    .unwrap();
    assert!(
        owner_control_authority(&conn, account, OwnerId::from_bytes(owner_id), founder.fp).is_err(),
        "a partial cut tuple must never become open authority",
    );
    conn.execute(
        "UPDATE account_owner_incarnations
             SET control_boundary = 'open', effective_at = -1
             WHERE owner_id = ?1",
        [owner_id.as_slice()],
    )
    .unwrap();
    assert!(
        owner_control_authority(&conn, account, OwnerId::from_bytes(owner_id), founder.fp).is_err(),
        "negative fact epochs fail closed",
    );
    conn.execute(
        "UPDATE account_owner_incarnations SET effective_at = 0, closed_at = 2
             WHERE owner_id = ?1",
        [owner_id.as_slice()],
    )
    .unwrap();
    conn.execute("UPDATE account_roster_history SET closed_at = 2 WHERE roster_ref = ?1", [
        owner_id.as_slice(),
    ])
    .unwrap();
    assert!(
        owner_control_authority(&conn, account, OwnerId::from_bytes(owner_id), founder.fp).is_err(),
        "a closed roster and owner cannot jointly retain Open/Open authority",
    );
}
