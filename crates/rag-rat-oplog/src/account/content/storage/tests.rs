use rag_rat_db::schema;

const NOW: i64 = 1_700_000_000_000;

use super::super::super::envelope::{AccountEntryHeader, sign_account_entry};
use super::super::super::id::account_id_from_genesis_payload;
use super::super::ContentEntryHeader;
use super::*;
use crate::account::ops::entry_type;
use crate::device::{DeviceSecret, DeviceX25519Secret};
use crate::stream::StreamId;

#[test]
fn fixed_reports_the_content_wrong_length_wording() {
    let err = fixed::<32>(&[0; 1]).unwrap_err();
    assert_eq!(err.to_string(), "expected 32 bytes, got 1");
}

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::test_hooks()).unwrap();
    conn
}

#[test]
fn pending_refold_guard_is_false_before_the_queue_table_exists() {
    let conn = Connection::open_in_memory().unwrap();
    assert!(!content_stream_has_pending_refold(&conn, StreamId::from_bytes([0x41; 32])).unwrap());
}

fn roster(
    conn: &Connection,
    secret: &DeviceSecret,
) -> (super::super::super::AccountId, AccountEntryHash) {
    let (account_id, signed) = signed_roster(secret);
    conn.execute(
        "INSERT INTO account_entries(entry_hash, account_id, log_id, device_fingerprint, seq,
             prev_hash, parent_ref, authority_ref, entry_type, accepted, signed_bytes, \
         received_at_ms)
             VALUES(?1, ?2, 0, ?3, 0, NULL, NULL, NULL, 0, 1, ?4, 1)",
        params![
            signed.entry_hash.as_slice(),
            account_id.to_bytes().as_slice(),
            secret.public().fingerprint().to_bytes().as_slice(),
            signed.signed_bytes,
        ],
    )
    .unwrap();
    (account_id, signed.entry_hash)
}

fn signed_roster(
    secret: &DeviceSecret,
) -> (super::super::super::AccountId, super::super::super::envelope::SignedAccountEntry) {
    let x = DeviceX25519Secret::from_seed(&[0x81; 32]).public().to_bytes();
    let op = AccountOp::AccountGenesis {
        ed25519_pubkey: secret.public().to_bytes(),
        x25519_pubkey: x,
        nonce16: [0; 16],
        created_at_ms: 1,
        label: None,
    };
    let payload = ops::encode(&op).unwrap();
    let account_id = account_id_from_genesis_payload(&payload);
    let header = AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: secret.public().fingerprint(),
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: entry_type::ACCOUNT_GENESIS,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 0,
        key_id: None,
        authority_ref: None,
    };
    let signed = sign_account_entry(secret, &header, &payload).unwrap();
    (account_id, signed)
}

fn signed_device_add(
    founder: &DeviceSecret,
    member: &DeviceSecret,
    account_id: super::super::super::AccountId,
    genesis_hash: AccountEntryHash,
) -> super::super::super::envelope::SignedAccountEntry {
    signed_device_add_at(founder, member, account_id, 1, genesis_hash, genesis_hash, 1)
}

fn signed_device_add_at(
    founder: &DeviceSecret,
    member: &DeviceSecret,
    account_id: super::super::super::AccountId,
    seq: u64,
    previous: AccountEntryHash,
    genesis_hash: AccountEntryHash,
    auth_len: u64,
) -> super::super::super::envelope::SignedAccountEntry {
    let op = AccountOp::DeviceAdd {
        device_fingerprint: member.public().fingerprint(),
        ed25519_pubkey: member.public().to_bytes(),
        x25519_pubkey: DeviceX25519Secret::from_seed(&[0x82; 32]).public().to_bytes(),
        role: crate::account::DeviceRole::Member,
        label: None,
    };
    let payload = ops::encode(&op).unwrap();
    let header = AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: founder.public().fingerprint(),
        seq,
        prev_hash: Some(previous),
        parent_ref: None,
        entry_type: entry_type::DEVICE_ADD,
        op_version: 1,
        crypto_suite: 0,
        auth_len,
        key_id: None,
        authority_ref: Some(genesis_hash.into()),
    };
    sign_account_entry(founder, &header, &payload).unwrap()
}

fn content(
    secret: &DeviceSecret,
    account_id: super::super::super::AccountId,
    roster_ref: RosterRef,
    seq: u64,
    previous: Option<AccountEntryHash>,
) -> SignedContentEntry {
    let header = ContentEntryHeader {
        stream_id: StreamId::from_bytes([0x44; 32]),
        author_account_id: account_id,
        device_fingerprint: secret.public().fingerprint(),
        seq,
        lamport: seq.saturating_add(1),
        prev_hash: previous,
        grant_id: None,
        roster_ref,
        owner_auth_len: u64::MAX,
        author_auth_len: u64::MAX,
        crypto_suite: 0,
        key_id: None,
    };
    envelope::sign_content_entry(secret, &header, &[0xf6]).unwrap()
}

fn seed_content_candidates(
    conn: &Connection,
    author: super::super::super::AccountId,
    device_fingerprint: [u8; 32],
    namespace: u64,
    count: usize,
    signed_bytes_len: usize,
) {
    let raw = vec![0_u8; signed_bytes_len];
    for ordinal in 0..count {
        let hash = cbor::sha256(
            &[namespace.to_be_bytes().as_slice(), (ordinal as u64).to_be_bytes().as_slice()]
                .concat(),
        );
        conn.execute(
            "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                     prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                     accepted, signed_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?8, 0, ?9, 0)",
            params![
                hash.as_slice(),
                [1_u8; 32].as_slice(),
                author.to_bytes().as_slice(),
                device_fingerprint.as_slice(),
                (ordinal as u64).to_be_bytes().as_slice(),
                [3_u8; 32].as_slice(),
                [0_u8; 8].as_slice(),
                [0_u8; 8].as_slice(),
                raw.as_slice(),
            ],
        )
        .unwrap();
    }
}

/// A device fingerprint distinct from any real local device — the pre-#652 default seed used
/// for candidates that stand in for FOREIGN, remotely-signed rows in the capacity tests.
const FOREIGN_FP: [u8; 32] = [2_u8; 32];

#[test]
fn enrollment_mint_ignores_non_fatal_content_promotions() {
    let conn = db();
    let account = super::super::super::bootstrap::local_account(&conn, 1).unwrap();
    seed_content_candidates(
        &conn,
        account,
        FOREIGN_FP,
        0x945,
        CANDIDATES_PER_AUTHOR_MAX as usize,
        1,
    );
    conn.execute(
        "INSERT INTO content_pre_verify(
                 signed_hash, entry_hash, claimed_stream_id, claimed_author_account_id,
                 claimed_fingerprint, roster_ref, raw_bytes, received_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, X'00', 1)",
        params![
            [0xd1u8; 32].as_slice(),
            [0xd2u8; 32].as_slice(),
            [0xd3u8; 32].as_slice(),
            account.to_bytes().as_slice(),
            [0xd4u8; 32].as_slice(),
            [0xd5u8; 32].as_slice(),
        ],
    )
    .unwrap();

    super::super::super::authoring::enrollment_authoring_fits(
        &conn,
        account,
        &[],
        crate::account::DeviceRole::Member,
        None,
    )
    .expect("opaque parked content is not part of mandatory enrollment authoring");
}

#[test]
fn exact_roster_ref_verifies_and_full_u64_counters_persist() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[1; 32]);
    let (account, roster_ref) = roster(&conn, &secret);
    let signed = content(&secret, account, roster_ref.into(), 0, None);
    assert_eq!(
        content_ingest(&conn, &signed.signed_bytes, 2).unwrap(),
        ContentIngestOutcome::Ingested { status: "retained_unfolded".into() }
    );
    let (seq, owner, author, accepted): (Vec<u8>, Vec<u8>, Vec<u8>, i64) = conn
        .query_row(
            "SELECT seq, owner_auth_len, author_auth_len, accepted FROM content_entries",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(seq, 0u64.to_be_bytes());
    assert_eq!(owner, u64::MAX.to_be_bytes());
    assert_eq!(author, u64::MAX.to_be_bytes());
    assert_eq!(accepted, 0, "C2 never manufactures authority acceptance");
}

#[test]
fn missing_predecessor_heals_when_dense_parent_arrives() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[2; 32]);
    let (account, roster_ref) = roster(&conn, &secret);
    let genesis = content(&secret, account, roster_ref.into(), 0, None);
    let child = content(&secret, account, roster_ref.into(), 1, Some(genesis.entry_hash));
    assert_eq!(
        content_ingest(&conn, &child.signed_bytes, 2).unwrap(),
        ContentIngestOutcome::Ingested { status: "parked{missing_predecessor}".into() }
    );
    content_ingest(&conn, &genesis.signed_bytes, 3).unwrap();
    assert_eq!(
        status_for(&conn.unchecked_transaction().unwrap(), &child.entry_hash).unwrap(),
        Some("retained_unfolded".into())
    );
}

#[test]
fn unknown_roster_parks_and_wrong_coordinate_is_order_independently_retained() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[3; 32]);
    let account = super::super::super::AccountId::from_bytes([0x55; 32]);
    let signed = content(&secret, account, RosterRef::from_bytes([0x66; 32]), 0, None);
    assert_eq!(
        content_ingest(&conn, &signed.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::PreVerify
    );

    let (account, roster_ref) = roster(&conn, &secret);
    let genesis = content(&secret, account, roster_ref.into(), 0, None);
    content_ingest(&conn, &genesis.signed_bytes, 2).unwrap();
    let mut wrong = content(&secret, account, roster_ref.into(), 1, Some(genesis.entry_hash));
    wrong.header.stream_id = StreamId::from_bytes([0x77; 32]);
    wrong = envelope::sign_content_entry(&secret, &wrong.header, &[0xf6]).unwrap();
    assert_eq!(
        content_ingest(&conn, &wrong.signed_bytes, 3).unwrap(),
        ContentIngestOutcome::Ingested { status: "parked{missing_predecessor}".into() }
    );

    let reverse = db();
    let (reverse_account, reverse_roster) = roster(&reverse, &secret);
    let reverse_genesis = content(&secret, reverse_account, reverse_roster.into(), 0, None);
    let mut reverse_wrong = content(
        &secret,
        reverse_account,
        reverse_roster.into(),
        1,
        Some(reverse_genesis.entry_hash),
    );
    reverse_wrong.header.stream_id = StreamId::from_bytes([0x77; 32]);
    reverse_wrong = envelope::sign_content_entry(&secret, &reverse_wrong.header, &[0xf6]).unwrap();
    content_ingest(&reverse, &reverse_wrong.signed_bytes, 2).unwrap();
    content_ingest(&reverse, &reverse_genesis.signed_bytes, 3).unwrap();
    assert_eq!(
        reverse
            .query_row("SELECT count(*) FROM content_entries", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        reverse
            .query_row(
                "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
                [reverse_wrong.entry_hash.as_slice()],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "parked{missing_predecessor}"
    );
}

#[test]
fn account_roster_arrival_promotes_parked_content_in_the_same_transaction() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[4; 32]);
    let (account, roster) = signed_roster(&secret);
    let signed = content(&secret, account, roster.entry_hash.into(), 0, None);
    assert_eq!(
        content_ingest(&conn, &signed.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::PreVerify
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );

    super::super::super::storage::account_ingest(&conn, &roster.signed_bytes, 2).unwrap();

    let (parked, stored, status, accepted): (i64, i64, String, i64) = conn
        .query_row(
            "SELECT (SELECT count(*) FROM content_pre_verify),
                        (SELECT count(*) FROM content_entries), s.status, e.accepted
                 FROM content_entries e JOIN content_entry_status s USING(entry_hash)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((parked, stored), (0, 1));
    assert_eq!(status, "retained_unfolded");
    assert_eq!(accepted, 0, "promotion cannot cross the C2/C3 authority boundary");
}

#[test]
fn promoting_pre_verify_content_marks_the_stream_for_settle() {
    // A pre-verify PARK does not mark the stream (the entry is not a candidate yet, and
    // `content_ingest` returns before its mark). When the roster key later arrives and
    // `account_ingest` PROMOTES the parked entry into a candidate, the promotion path must mark
    // the stream — otherwise a promoted entry the account fold later accepts would carry no
    // queue row, settle would skip it, and its /3 projection would stay stale (#652/#699).
    let conn = db();
    let secret = DeviceSecret::from_seed(&[7; 32]);
    let (account, roster) = signed_roster(&secret);
    let signed = content(&secret, account, roster.entry_hash.into(), 0, None);

    assert_eq!(
        content_ingest(&conn, &signed.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::PreVerify,
    );
    assert_eq!(pending_refold_count(&conn), 0, "a pre-verify park does not mark the stream");

    super::super::super::storage::account_ingest(&conn, &roster.signed_bytes, 2).unwrap();
    assert_eq!(
        pending_refold_count(&conn),
        1,
        "promoting the parked entry marks its stream so a later settle refolds + reprojects it",
    );
}

/// A PARKED content entry (roster key not yet resolvable) is part of what a peer must be
/// offered for sync (#406): a peer holding the roster material promotes it, and omitting it
/// would let a session complete with the dependent memory silently missing.
/// `content_entries_for_sync` offers both held candidates and parked rows, with the exact
/// bytes so a peer re-ingests the real entry.
#[test]
fn content_entries_for_sync_offers_a_parked_entry() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[7; 32]);
    let (account, roster) = signed_roster(&secret);
    // The roster is deliberately NOT ingested, so the content's roster key cannot resolve and
    // it parks in pre-verify rather than becoming a stored candidate.
    let parked = content(&secret, account, roster.entry_hash.into(), 0, None);
    assert_eq!(
        content_ingest(&conn, &parked.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::PreVerify,
    );

    let offered = content_entries_for_sync(&conn, account).unwrap();
    let row = offered
        .iter()
        .find(|e| e.entry_hash == parked.entry_hash)
        .expect("the parked content entry is offered, not hidden");
    assert_eq!(row.signed_bytes, parked.signed_bytes, "the exact parked bytes are offered");
}

/// The public-serve variant (#407 E2b) offers the authenticated `content_entries` rows but NOT
/// the parked `content_pre_verify` candidates — a public server must never relay
/// unauthenticated, attacker-settable bytes to an anonymous reader.
#[test]
fn content_entries_for_public_sync_omits_parked_candidates() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[7; 32]);
    let (account, roster) = signed_roster(&secret);
    // A parked candidate: the roster is NOT ingested, so its key cannot resolve and it lands in
    // content_pre_verify rather than content_entries.
    let parked = content(&secret, account, roster.entry_hash.into(), 0, None);
    assert_eq!(
        content_ingest(&conn, &parked.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::PreVerify,
    );
    // An authenticated held row, straight into content_entries.
    seed_content_candidates(&conn, account, [9_u8; 32], 77, 1, 64);

    let all = content_entries_for_sync(&conn, account).unwrap();
    assert!(
        all.iter().any(|e| e.entry_hash == parked.entry_hash),
        "the whole-account serve includes the parked candidate",
    );

    let public = content_entries_for_public_sync(&conn, account).unwrap();
    assert!(
        !public.iter().any(|e| e.entry_hash == parked.entry_hash),
        "the public serve omits the parked candidate",
    );
    // And it omits the authenticated row too — for a DIFFERENT reason. Every row is filtered by
    // its own stream's access mode, and this fixture's stream has no folded ownership fact, so
    // it is unattributable and fails closed. That per-stream filter is what stops a streamless
    // contributor (#1164) holding grants on a public AND a private stream from serving its
    // private-stream contributions to anonymous readers: one public grant makes the ACCOUNT
    // servable, so only a per-stream check can withhold the private rows.
    assert!(
        public.is_empty(),
        "rows on a stream that does not resolve PublicRead are withheld, whatever the account",
    );
    assert_eq!(all.len(), 2, "both rows are still served on the whole-account (Full) path");
}

/// An owner relays held content on its streams only from an author it granted that stream, and
/// only when a device of that author's roster signed it — accepted or not, so a condemned entry
/// a cut names still travels, while a candidate forged under a granted author's name, or
/// written by an account never granted, does not (#1280).
#[test]
fn relayed_content_is_the_granted_authors_rows_signed_by_their_roster() {
    let conn = db();
    let owner = AccountId::from_bytes([0x11; 32]);
    let granted = AccountId::from_bytes([0x22; 32]);
    let stranger = AccountId::from_bytes([0x33; 32]);
    let (member, forger, outsider) = ([0x71; 32], [0x72; 32], [0x73; 32]);
    seed_ownership(&conn, owner);
    seed_grant(&conn, GrantId::from_bytes([0x55; 32]), owner, granted, "writer");
    for (account, device) in [(granted, member), (stranger, outsider)] {
        conn.execute(
            "INSERT INTO account_roster_history(
                     roster_ref, account_id, device_fingerprint, role, effective_at, closed_at)
                 VALUES(?1, ?2, ?3, 'owner', 1, NULL)",
            params![device.as_slice(), account.to_bytes().as_slice(), device.as_slice()],
        )
        .unwrap();
    }
    let row = |hash: u8, author: AccountId, device: [u8; 32], seq: u64, accepted: bool| {
        conn.execute(
            "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                     prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                     accepted, signed_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?7, ?8, x'00', 0)",
            params![
                [hash; 32].as_slice(),
                STREAM.as_slice(),
                author.to_bytes().as_slice(),
                device.as_slice(),
                seq.to_be_bytes().as_slice(),
                [3_u8; 32].as_slice(),
                [0_u8; 8].as_slice(),
                accepted,
            ],
        )
        .unwrap();
    };
    row(1, granted, member, 0, true);
    row(2, granted, member, 1, false); // condemned by a cut, still evidence
    row(3, granted, forger, 0, false); // claims the granted author, signed outside its roster
    row(4, stranger, outsider, 0, false); // never granted this stream
    row(5, owner, member, 0, true); // the owner's own rows are served on their own

    let relayed: Vec<AccountEntryHash> = relayed_content_entries(&conn, owner)
        .unwrap()
        .into_iter()
        .map(|entry| entry.entry_hash)
        .collect();
    assert_eq!(relayed, vec![
        AccountEntryHash::from_bytes([1; 32]),
        AccountEntryHash::from_bytes([2; 32])
    ]);
}

/// `content_signed_entry_exists` is signed-envelope precise, not entry_hash precise, and
/// `content_signed_hash` gives distinct wire dedup keys to distinct envelopes — so a competing
/// signature of the same body is never suppressed as already-held (the reason the pre-verify
/// table keys on the signed hash).
#[test]
fn content_signed_entry_existence_is_by_exact_envelope() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[7; 32]);
    let (account, roster) = signed_roster(&secret);
    super::super::super::storage::account_ingest(&conn, &roster.signed_bytes, 1).unwrap();
    let entry = content(&secret, account, roster.entry_hash.into(), 0, None);
    content_ingest(&conn, &entry.signed_bytes, 2).unwrap();

    assert!(
        content_signed_entry_exists(&conn, account, &entry.signed_bytes).unwrap(),
        "the exact stored envelope is held",
    );
    // A corrupted-signature variant of the same body is a DISTINCT envelope: not held, and a
    // distinct wire dedup key.
    let mut variant = entry.signed_bytes.clone();
    *variant.last_mut().unwrap() ^= 0x01;
    assert_ne!(variant, entry.signed_bytes);
    assert!(
        !content_signed_entry_exists(&conn, account, &variant).unwrap(),
        "a distinct signed envelope is not suppressed as already-held",
    );
    assert_ne!(
        content_signed_hash(&entry.signed_bytes),
        content_signed_hash(&variant),
        "distinct envelopes get distinct wire dedup keys",
    );
}

#[test]
fn device_add_exact_roster_promotes_then_verifies_non_founder_content() {
    let conn = db();
    let founder = DeviceSecret::from_seed(&[13; 32]);
    let member = DeviceSecret::from_seed(&[14; 32]);
    let (account, genesis) = signed_roster(&founder);
    super::super::super::storage::account_ingest(&conn, &genesis.signed_bytes, 1).unwrap();
    let add = signed_device_add(&founder, &member, account, genesis.entry_hash);
    let first = content(&member, account, add.entry_hash.into(), 0, None);
    assert_eq!(
        content_ingest(&conn, &first.signed_bytes, 2).unwrap(),
        ContentIngestOutcome::PreVerify
    );

    super::super::super::storage::account_ingest(&conn, &add.signed_bytes, 3).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let second = content(&member, account, add.entry_hash.into(), 1, Some(first.entry_hash));
    assert_eq!(
        content_ingest(&conn, &second.signed_bytes, 4).unwrap(),
        ContentIngestOutcome::Ingested { status: "retained_unfolded".into() }
    );

    let outsider = DeviceSecret::from_seed(&[15; 32]);
    let wrong = content(&outsider, account, add.entry_hash.into(), 0, None);
    assert!(matches!(
        content_ingest(&conn, &wrong.signed_bytes, 5).unwrap(),
        ContentIngestOutcome::Rejected(_)
    ));
}

#[test]
fn duplicate_pre_verify_is_idempotent_and_does_not_consume_queue_budget() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[5; 32]);
    let account = super::super::super::AccountId::from_bytes([0x65; 32]);
    let signed = content(&secret, account, RosterRef::from_bytes([0x75; 32]), 0, None);
    for now in 1..=3 {
        assert_eq!(
            content_ingest(&conn, &signed.signed_bytes, now).unwrap(),
            ContentIngestOutcome::PreVerify
        );
    }
    assert_eq!(
        conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn bad_signature_and_wrong_exact_roster_are_rejected_without_storage() {
    let conn = db();
    let enrolled = DeviceSecret::from_seed(&[6; 32]);
    let attacker = DeviceSecret::from_seed(&[7; 32]);
    let (account, roster_ref) = roster(&conn, &enrolled);

    let wrong_device = content(&attacker, account, roster_ref.into(), 0, None);
    assert!(matches!(
        content_ingest(&conn, &wrong_device.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::Rejected(_)
    ));

    let mut bad_signature = content(&enrolled, account, roster_ref.into(), 0, None).signed_bytes;
    let last = bad_signature.last_mut().unwrap();
    *last ^= 1;
    assert!(matches!(
        content_ingest(&conn, &bad_signature, 2).unwrap(),
        ContentIngestOutcome::Rejected(_)
    ));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn equivocations_remain_first_class_but_unaccepted() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[8; 32]);
    let (account, roster_ref) = roster(&conn, &secret);
    let first = content(&secret, account, roster_ref.into(), 0, None);
    let mut second_header = first.header.clone();
    second_header.lamport += 1;
    let second = envelope::sign_content_entry(&secret, &second_header, &[0xf6]).unwrap();
    let first_child = content(&secret, account, roster_ref.into(), 1, Some(first.entry_hash));
    let second_child = content(&secret, account, roster_ref.into(), 1, Some(second.entry_hash));
    for (received, signed) in [&second_child, &first_child, &second, &first].into_iter().enumerate()
    {
        content_ingest(&conn, &signed.signed_bytes, received as i64).unwrap();
    }
    assert_eq!(
        conn.query_row("SELECT count(*), sum(accepted) FROM content_entries", [], |row| Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?
        )),)
            .unwrap(),
        (4, 0)
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM content_entry_status WHERE status = 'retained_unfolded'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        4
    );
}

#[test]
fn pre_verify_evicts_oldest_per_author_and_keeps_newest() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[9; 32]);
    let account = super::super::super::AccountId::from_bytes([0x79; 32]);
    let mut first_hash = None;
    let mut newest_hash = None;
    for seq in 0..=PRE_VERIFY_PER_AUTHOR_MAX as u64 {
        let previous = (seq > 0).then_some([seq as u8; 32]);
        let signed = content(
            &secret,
            account,
            RosterRef::from_bytes([seq as u8; 32]),
            seq,
            previous.map(Into::into),
        );
        let hash = cbor::sha256(&signed.signed_bytes);
        first_hash.get_or_insert(hash);
        newest_hash = Some(hash);
        let outcome = content_ingest(&conn, &signed.signed_bytes, seq as i64).unwrap();
        if seq == PRE_VERIFY_PER_AUTHOR_MAX as u64 {
            assert_eq!(outcome, ContentIngestOutcome::PreVerifyWithEviction {
                scopes: vec![ContentCapacityScope::PreVerifyAuthor]
            });
        }
    }
    assert_eq!(
        conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        PRE_VERIFY_PER_AUTHOR_MAX as i64
    );
    assert!(!PRE_VERIFY.contains(&conn, &first_hash.unwrap().into()).unwrap());
    assert!(PRE_VERIFY.contains(&conn, &newest_hash.unwrap().into()).unwrap());
}

#[test]
fn content_pre_verify_author_cap_protects_other_authors_below_global_cap() {
    // The per-author pre-verify cap must stop ONE claimed author from evicting ANOTHER author's
    // parked rows while the global queue (PRE_VERIFY_GLOBAL_MAX) still has headroom. Park one
    // older row under author B, then flood author A with PRE_VERIFY_PER_AUTHOR_MAX + 1 rows at
    // strictly newer received_at_ms; the total (PER_AUTHOR_MAX + 1) stays far under the global
    // cap, so ONLY the per-author eviction can fire. If the per-author eviction DELETE were
    // made global-scoped (dropping `WHERE claimed_author_account_id = ?1`), it would
    // evict the globally-oldest row — author B's — instead of author A's own oldest,
    // and this test fails. The single-author test above cannot catch that regression;
    // the account layer's `pre_verify_budget_evicts_oldest_per_account_and_globally` is
    // the sibling guard.
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x5b; 32]);
    let author_a = super::super::super::AccountId::from_bytes([0x0a; 32]);
    let author_b = super::super::super::AccountId::from_bytes([0x0b; 32]);

    // Author B parks one row FIRST, at the oldest received_at_ms — the globally-oldest row, so
    // a global-scoped eviction would target exactly this one.
    let b_entry = content(&secret, author_b, RosterRef::from_bytes([0xee; 32]), 0, None);
    let b_hash = cbor::sha256(&b_entry.signed_bytes);
    assert_eq!(
        content_ingest(&conn, &b_entry.signed_bytes, 0).unwrap(),
        ContentIngestOutcome::PreVerify,
    );

    // Author A floods PER_AUTHOR_MAX + 1 rows, each strictly newer than author B's. Only the
    // (MAX+1)th trips the per-author cap, evicting author A's OWN oldest row.
    for seq in 0..=PRE_VERIFY_PER_AUTHOR_MAX as u64 {
        // A garbage but non-null predecessor for seq > 0 (prev_hash must be null iff seq == 0);
        // it keeps every row a distinct pre-verify entry and never resolves (still parked).
        let previous = (seq > 0).then_some([seq as u8; 32]);
        let a_entry = content(
            &secret,
            author_a,
            RosterRef::from_bytes([0xee; 32]),
            seq,
            previous.map(Into::into),
        );
        let received_at_ms = seq as i64 + 1; // strictly newer than author B's 0
        let outcome = content_ingest(&conn, &a_entry.signed_bytes, received_at_ms).unwrap();
        if seq == PRE_VERIFY_PER_AUTHOR_MAX as u64 {
            assert_eq!(outcome, ContentIngestOutcome::PreVerifyWithEviction {
                scopes: vec![ContentCapacityScope::PreVerifyAuthor],
            });
        } else {
            assert_eq!(outcome, ContentIngestOutcome::PreVerify);
        }
    }

    // Author B's older row survives — author A's per-author eviction must not reach across it.
    assert!(
        PRE_VERIFY.contains(&conn, &b_hash.into()).unwrap(),
        "author A's flood must NOT evict author B's parked row",
    );
    // Author A is held to EXACTLY the per-author cap.
    let a_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_pre_verify WHERE claimed_author_account_id = ?1",
            [author_a.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        a_count, PRE_VERIFY_PER_AUTHOR_MAX as i64,
        "author A is capped at the per-author max"
    );
    // The global total is PER_AUTHOR_MAX + 1 (author A's cap + author B's surviving row), well
    // under the global cap — proving the global eviction never fired.
    let total: i64 =
        conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row.get(0)).unwrap();
    assert_eq!(total, PRE_VERIFY_PER_AUTHOR_MAX as i64 + 1);
}

#[test]
fn reverse_delivery_heals_a_long_dense_chain_without_recursion() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[10; 32]);
    let (account, roster_ref) = roster(&conn, &secret);
    let mut entries = Vec::new();
    let mut previous = None;
    for seq in 0..256 {
        let signed = content(&secret, account, roster_ref.into(), seq, previous);
        previous = Some(signed.entry_hash);
        entries.push(signed);
    }
    for (received, signed) in entries.iter().rev().enumerate() {
        content_ingest(&conn, &signed.signed_bytes, received as i64).unwrap();
    }
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM content_entry_status WHERE status = 'retained_unfolded'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        256
    );
}

#[test]
fn maximum_sequence_with_a_missing_predecessor_is_stored_without_overflow() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[11; 32]);
    let (account, roster_ref) = roster(&conn, &secret);
    // An explicit in-ceiling lamport: the `content` helper's `seq + 1` mint would saturate to
    // `u64::MAX` here and trip the ingest ceiling — this test is about SEQ overflow, not the
    // lamport clamp.
    let signed = authored(&secret, account, roster_ref.into(), ContentSpec {
        seq: u64::MAX,
        previous: Some(AccountEntryHash::from_bytes([0xaa; 32])),
        lamport: Some(1),
        ..ContentSpec::default()
    });
    assert_eq!(
        content_ingest(&conn, &signed.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::Ingested { status: "parked{missing_predecessor}".into() }
    );
    assert_eq!(
        conn.query_row("SELECT seq FROM content_entries", [], |row| row.get::<_, Vec<u8>>(0))
            .unwrap(),
        u64::MAX.to_be_bytes()
    );
}

#[test]
fn terminal_promotion_capacity_is_reported_and_clears_authenticated_queue_work() {
    let mut conn = db();
    let secret = DeviceSecret::from_seed(&[12; 32]);
    let (account, roster) = signed_roster(&secret);
    let signed = content(&secret, account, roster.entry_hash.into(), 0, None);
    assert_eq!(
        content_ingest(&conn, &signed.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::PreVerify
    );
    let tx = conn.transaction().unwrap();
    for index in 0..CANDIDATES_PER_AUTHOR_MAX {
        let mut hash = [0_u8; 32];
        hash[24..].copy_from_slice(&(index as u64).to_be_bytes());
        tx.execute(
            "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                     prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                     accepted, signed_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?8, 0, ?9, 0)",
            params![
                hash.as_slice(),
                [1_u8; 32].as_slice(),
                account.to_bytes().as_slice(),
                [2_u8; 32].as_slice(),
                (index as u64).to_be_bytes().as_slice(),
                [3_u8; 32].as_slice(),
                [0_u8; 8].as_slice(),
                [0_u8; 8].as_slice(),
                [0_u8],
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();

    let outcome =
        super::super::super::storage::account_ingest(&conn, &roster.signed_bytes, 2).unwrap();
    assert_eq!(outcome, super::super::super::storage::IngestOutcome::Ingested {
        status: "accepted".into(),
        account_promotions: Default::default(),
        content_promotions: ContentPromotionOutcome {
            scope: Some(ContentCapacityScope::CandidateAuthor),
            entry_hashes: vec![signed.entry_hash],
        },
    });
    assert_eq!(
        conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM content_entries WHERE entry_hash = ?1",
            [signed.entry_hash.as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
}

#[test]
fn candidate_admission_reports_author_and_global_count_and_byte_scopes() {
    let secret = DeviceSecret::from_seed(&[16; 32]);

    let author_count = db();
    let (account, roster_ref) = roster(&author_count, &secret);
    let signed = content(&secret, account, roster_ref.into(), 0, None);
    let verified = envelope::verify_content_signed(&signed.signed_bytes, &secret.public()).unwrap();
    seed_content_candidates(
        &author_count,
        account,
        FOREIGN_FP,
        1,
        CANDIDATES_PER_AUTHOR_MAX as usize,
        1,
    );
    let tx = author_count.unchecked_transaction().unwrap();
    assert_eq!(
        candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
        Some(ContentCapacityScope::CandidateAuthor)
    );

    let author_bytes = db();
    let (account, roster_ref) = roster(&author_bytes, &secret);
    let signed = content(&secret, account, roster_ref.into(), 0, None);
    let verified = envelope::verify_content_signed(&signed.signed_bytes, &secret.public()).unwrap();
    seed_content_candidates(
        &author_bytes,
        account,
        FOREIGN_FP,
        2,
        1,
        CANDIDATE_BYTES_PER_AUTHOR_MAX as usize,
    );
    let tx = author_bytes.unchecked_transaction().unwrap();
    assert_eq!(
        candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
        Some(ContentCapacityScope::CandidateAuthorBytes)
    );

    let global_count = db();
    for author in 0..4_u8 {
        seed_content_candidates(
            &global_count,
            super::super::super::AccountId::from_bytes([author; 32]),
            FOREIGN_FP,
            10 + u64::from(author),
            CANDIDATES_PER_AUTHOR_MAX as usize,
            1,
        );
    }
    let (account, roster_ref) = roster(&global_count, &secret);
    let signed = content(&secret, account, roster_ref.into(), 0, None);
    let verified = envelope::verify_content_signed(&signed.signed_bytes, &secret.public()).unwrap();
    let tx = global_count.unchecked_transaction().unwrap();
    assert_eq!(
        candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
        Some(ContentCapacityScope::CandidateGlobal)
    );

    let global_bytes = db();
    for author in 0..5_u8 {
        seed_content_candidates(
            &global_bytes,
            super::super::super::AccountId::from_bytes([author; 32]),
            FOREIGN_FP,
            20 + u64::from(author),
            1,
            13 * 1024 * 1024,
        );
    }
    let (account, roster_ref) = roster(&global_bytes, &secret);
    let signed = content(&secret, account, roster_ref.into(), 0, None);
    let verified = envelope::verify_content_signed(&signed.signed_bytes, &secret.public()).unwrap();
    let tx = global_bytes.unchecked_transaction().unwrap();
    assert_eq!(
        candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
        Some(ContentCapacityScope::CandidateGlobalBytes)
    );
}

/// The candidate budgets bound UNRESOLVED candidates. Accepted history is authorized content,
/// so an author with a full budget of it — and a store with a full global budget of it — still
/// admits the author's next entry.
#[test]
fn accepted_history_does_not_consume_candidate_capacity() {
    let secret = DeviceSecret::from_seed(&[18; 32]);
    let conn = db();
    let (account, roster_ref) = roster(&conn, &secret);
    seed_content_candidates(&conn, account, FOREIGN_FP, 1, CANDIDATES_PER_AUTHOR_MAX as usize, 1);
    // Another device of the same author, so the accepted rows do not share a chain position.
    seed_content_candidates(
        &conn,
        account,
        [0x77; 32],
        2,
        1,
        CANDIDATE_BYTES_PER_AUTHOR_MAX as usize,
    );
    for author in 0..5_u8 {
        seed_content_candidates(
            &conn,
            super::super::super::AccountId::from_bytes([author; 32]),
            FOREIGN_FP,
            10 + u64::from(author),
            CANDIDATES_PER_AUTHOR_MAX as usize,
            13 * 1024,
        );
    }
    conn.execute("UPDATE content_entries SET accepted = 1", []).unwrap();

    let signed = content(&secret, account, roster_ref.into(), 0, None);
    let verified = envelope::verify_content_signed(&signed.signed_bytes, &secret.public()).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    assert_eq!(candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(), None);
}

#[test]
fn global_pre_verify_evicts_oldest_and_exact_candidate_replay_bypasses_capacity() {
    let conn = db();
    for ordinal in 0..PRE_VERIFY_GLOBAL_MAX as i64 {
        let signed_hash = cbor::sha256(&ordinal.to_be_bytes());
        conn.execute(
            "INSERT INTO content_pre_verify(
                     signed_hash, entry_hash, claimed_stream_id, claimed_author_account_id,
                     claimed_fingerprint, roster_ref, raw_bytes, received_at_ms)
                 VALUES(?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                signed_hash.as_slice(),
                [1_u8; 32].as_slice(),
                [ordinal as u8; 32].as_slice(),
                [2_u8; 32].as_slice(),
                [3_u8; 32].as_slice(),
                [0_u8],
                ordinal,
            ],
        )
        .unwrap();
    }
    let oldest = cbor::sha256(&0_i64.to_be_bytes());
    let secret = DeviceSecret::from_seed(&[17; 32]);
    let unknown_account = super::super::super::AccountId::from_bytes([0xf1; 32]);
    let parked = content(&secret, unknown_account, RosterRef::from_bytes([0xf2; 32]), 0, None);
    assert_eq!(
        content_ingest(&conn, &parked.signed_bytes, PRE_VERIFY_GLOBAL_MAX as i64 + 1).unwrap(),
        ContentIngestOutcome::PreVerifyWithEviction {
            scopes: vec![ContentCapacityScope::PreVerifyGlobal]
        }
    );
    assert!(!PRE_VERIFY.contains(&conn, &oldest.into()).unwrap());

    let replay = db();
    let (account, roster_ref) = roster(&replay, &secret);
    let signed = content(&secret, account, roster_ref.into(), 0, None);
    let expected = content_ingest(&replay, &signed.signed_bytes, 1).unwrap();
    seed_content_candidates(
        &replay,
        account,
        FOREIGN_FP,
        99,
        CANDIDATES_PER_AUTHOR_MAX as usize - 1,
        1,
    );
    assert_eq!(content_ingest(&replay, &signed.signed_bytes, 2).unwrap(), expected);
    assert_eq!(
        replay
            .query_row(
                "SELECT count(*) FROM content_entries WHERE author_account_id = ?1",
                [account.to_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        CANDIDATES_PER_AUTHOR_MAX
    );
}

// ---- C3.3: the acceptance refold wired into ingest ----

const STREAM: [u8; 32] = [0x44; 32];

/// The parts of a content entry that vary per test. Defaults are the common case: a seq-0 chain
/// root, no grant, `auth_len` 0 (freshness `CurrentOrBehind` — the frozen `content` helper pins
/// it at `u64::MAX`, always `Ahead`, which would park every accept path), body `0xf6`.
#[derive(Clone, Copy)]
struct ContentSpec {
    grant_id: Option<GrantId>,
    seq: u64,
    previous: Option<AccountEntryHash>,
    auth_len: u64,
    body: u8,
    /// `None` mints the honest clock (`seq + 1`); `Some` forges an arbitrary header lamport,
    /// the attacker-controlled input the `/3` lamport clamp exists for.
    lamport: Option<u64>,
}

impl Default for ContentSpec {
    fn default() -> Self {
        Self { grant_id: None, seq: 0, previous: None, auth_len: 0, body: 0xf6, lamport: None }
    }
}

/// Sign a content entry. `roster_ref` must name the author's real account genesis so the
/// signing device resolves (see [`resolve_roster_key`]).
fn authored(
    secret: &DeviceSecret,
    author: AccountId,
    roster_ref: RosterRef,
    spec: ContentSpec,
) -> SignedContentEntry {
    let header = ContentEntryHeader {
        stream_id: StreamId::from_bytes(STREAM),
        author_account_id: author,
        device_fingerprint: secret.public().fingerprint(),
        seq: spec.seq,
        lamport: spec.lamport.unwrap_or(spec.seq.saturating_add(1)),
        prev_hash: spec.previous,
        grant_id: spec.grant_id,
        roster_ref,
        owner_auth_len: spec.auth_len,
        author_auth_len: spec.auth_len,
        crypto_suite: 0,
        key_id: None,
    };
    envelope::sign_content_entry(secret, &header, &[spec.body]).unwrap()
}

/// Sign a content entry carrying a real memory-op body, so the accepted-`/3` projection has
/// something to fold — the `authored` helper's single-byte body never decodes to an op.
fn authored_op(
    secret: &DeviceSecret,
    author: AccountId,
    roster_ref: RosterRef,
    spec: ContentSpec,
    memory_op: &crate::op::MemoryOp,
) -> SignedContentEntry {
    let header = ContentEntryHeader {
        stream_id: StreamId::from_bytes(STREAM),
        author_account_id: author,
        device_fingerprint: secret.public().fingerprint(),
        seq: spec.seq,
        lamport: spec.lamport.unwrap_or(spec.seq.saturating_add(1)),
        prev_hash: spec.previous,
        grant_id: spec.grant_id,
        roster_ref,
        owner_auth_len: spec.auth_len,
        author_auth_len: spec.auth_len,
        crypto_suite: 0,
        key_id: None,
    };
    envelope::sign_content_entry(secret, &header, &crate::op::encode(memory_op)).unwrap()
}

fn node_create(id: &str) -> crate::op::MemoryOp {
    crate::op::MemoryOp::NodeCreate {
        node_id: crate::op::NodeId::from(id),
        content: crate::op::NodeContent {
            kind: "Invariant".to_string(),
            title: id.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            source: "agent".to_string(),
            tags: Vec::new(),
            payload: None,
        },
    }
}

/// The node ids the accepted-`/3` projection holds for [`STREAM`], sorted.
fn projected_node_ids(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT node_id FROM content_projected_nodes WHERE stream_id = ?1 ORDER BY node_id",
        )
        .unwrap();
    stmt.query_map([STREAM.as_slice()], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn seed_ownership(conn: &Connection, owner: AccountId) {
    conn.execute(
        "INSERT INTO account_stream_ownership(stream_id, account_id, own_id, effective_at)
             VALUES(?1, ?2, ?3, 1)",
        params![STREAM.as_slice(), owner.to_bytes().as_slice(), [0x66_u8; 32].as_slice()],
    )
    .unwrap();
}

fn seed_roster_fact(
    conn: &Connection,
    roster_ref: RosterRef,
    account: AccountId,
    device: &DeviceSecret,
    role: &str,
) {
    conn.execute(
        "INSERT INTO account_roster_history(
                 roster_ref, account_id, device_fingerprint, role, effective_at, closed_at)
             VALUES(?1, ?2, ?3, ?4, 1, NULL)",
        params![
            roster_ref.as_slice(),
            account.to_bytes().as_slice(),
            device.public().fingerprint().to_bytes().as_slice(),
            role,
        ],
    )
    .unwrap();
}

fn seed_roster_content_cut(
    conn: &Connection,
    roster_ref: RosterRef,
    account: AccountId,
    seq: u64,
    watermark: AccountEntryHash,
) {
    conn.execute(
        "INSERT INTO account_roster_content_boundaries(
                 roster_ref, account_id, stream_id, seq, entry_hash)
             VALUES(?1, ?2, ?3, ?4, ?5)",
        params![
            roster_ref.as_slice(),
            account.to_bytes().as_slice(),
            STREAM.as_slice(),
            seq.to_be_bytes().as_slice(),
            watermark.as_slice(),
        ],
    )
    .unwrap();
}

fn seed_grant(
    conn: &Connection,
    grant_id: GrantId,
    owner: AccountId,
    grantee: AccountId,
    role: &str,
) {
    conn.execute(
        "INSERT INTO account_stream_grants(
                 grant_id, owner_account_id, stream_id, grantee_account_id, role,
                 effective_at, closed_at)
             VALUES(?1, ?2, ?3, ?4, ?5, 1, NULL)",
        params![
            grant_id.as_slice(),
            owner.to_bytes().as_slice(),
            STREAM.as_slice(),
            grantee.to_bytes().as_slice(),
            role,
        ],
    )
    .unwrap();
}

/// A revoked (closed) grant plus a device cut, so `grant_effective_for_device_in_snapshot`
/// resolves it to a `Cut` boundary — the shape whose watermark could be misused as a branch
/// pin.
fn seed_closed_grant_with_cut(
    conn: &Connection,
    grant_id: GrantId,
    owner: AccountId,
    grantee: AccountId,
    role: &str,
    device: &DeviceSecret,
    watermark: AccountEntryHash,
) {
    conn.execute(
        "INSERT INTO account_stream_grants(
                 grant_id, owner_account_id, stream_id, grantee_account_id, role,
                 effective_at, closed_at)
             VALUES(?1, ?2, ?3, ?4, ?5, 1, 2)",
        params![
            grant_id.as_slice(),
            owner.to_bytes().as_slice(),
            STREAM.as_slice(),
            grantee.to_bytes().as_slice(),
            role,
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO account_stream_grant_cuts(
                 grant_id, owner_account_id, device_fingerprint, seq, entry_hash)
             VALUES(?1, ?2, ?3, ?4, ?5)",
        params![
            grant_id.as_slice(),
            owner.to_bytes().as_slice(),
            device.public().fingerprint().to_bytes().as_slice(),
            0_u64.to_be_bytes().as_slice(),
            watermark.as_slice(),
        ],
    )
    .unwrap();
}

fn seed_contested(conn: &Connection, account: AccountId) {
    conn.execute(
        "INSERT INTO account_auth_state(
                 account_id, classification, contested_depth, successor_account_id, \
         effective_count)
             VALUES(?1, 'contested', 1, NULL, 3)",
        [account.to_bytes().as_slice()],
    )
    .unwrap();
}

fn verdict(conn: &Connection, entry_hash: &AccountEntryHash) -> (String, i64) {
    conn.query_row(
        "SELECT s.status, e.accepted FROM content_entries e
             JOIN content_entry_status s ON s.entry_hash = e.entry_hash
             WHERE e.entry_hash = ?1",
        [entry_hash.as_slice()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )
    .unwrap()
}

/// Ingest then SETTLE, and read the resulting acceptance verdict. `content_ingest` now defers
/// the acceptance fold (#652), so a test that wants the folded verdict must settle first —
/// this helper is the "ingest and observe acceptance" shorthand the acceptance tests are
/// written against.
fn verdict_after_ingest(conn: &Connection, entry: &SignedContentEntry) -> (String, i64) {
    content_ingest(conn, &entry.signed_bytes, 1).unwrap();
    settle_all(conn);
    verdict(conn, &entry.entry_hash)
}

#[test]
fn an_owner_authored_entry_accepts_when_authority_and_branch_are_clear() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x21; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    let entry = authored(&secret, owner, genesis.into(), ContentSpec::default());
    // Ingest DEFERS the acceptance fold (#652), so it returns the STRUCTURAL status; the
    // acceptance verdict appears once the stream is settled.
    assert_eq!(
        content_ingest(&conn, &entry.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::Ingested { status: "retained_unfolded".into() },
    );
    assert_eq!(settle_all(&conn).settled_streams, 1);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
}

#[test]
fn a_contributor_with_a_writer_grant_accepts() {
    let conn = db();
    let owner_secret = DeviceSecret::from_seed(&[0x31; 32]);
    let author_secret = DeviceSecret::from_seed(&[0x32; 32]);
    let owner = roster(&conn, &owner_secret).0;
    let (author, author_genesis) = roster(&conn, &author_secret);
    let grant_id = [0x67; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, author, "writer");

    let entry = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(grant_id)),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &entry), ("accepted".into(), 1));
}

// ---- the /3 lamport clamp: protocol ceiling at ingest, bounded advance at the fold ----

#[test]
fn a_ceiling_lamport_is_rejected_at_ingest_before_the_pre_verify_park() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x21; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // At the ceiling, a resolvable author is rejected outright.
    let poison = authored(&secret, owner, genesis.into(), ContentSpec {
        lamport: Some(crate::entry::MAX_ENTRY_LAMPORT),
        ..ContentSpec::default()
    });
    let ContentIngestOutcome::Rejected(reason) =
        content_ingest(&conn, &poison.signed_bytes, 1).unwrap()
    else {
        panic!("a ceiling lamport must reject at ingest");
    };
    assert!(reason.contains("protocol ceiling"), "{reason}");

    // The park bypass: an UNKNOWN author would normally park pre-verify, and promotion
    // re-inserts parked bytes without re-entering `content_ingest` — so the ceiling must
    // reject BEFORE parking, or an attacker parks a poison entry behind a withheld roster.
    let stranger = DeviceSecret::from_seed(&[0x99; 32]);
    let strange_account = AccountId::from_bytes([0x99; 32]);
    let parked_poison =
        authored(&stranger, strange_account, RosterRef::from_bytes([0x98; 32]), ContentSpec {
            lamport: Some(u64::MAX),
            ..ContentSpec::default()
        });
    assert!(matches!(
        content_ingest(&conn, &parked_poison.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::Rejected(_)
    ));
    let pre_verify: i64 =
        conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row.get(0)).unwrap();
    assert_eq!(pre_verify, 0, "a ceiling entry never reaches the pre-verify park");
}

#[test]
fn promotion_drops_pre_verify_rows_that_violate_the_lamport_gates() {
    // Pre-verify rows parked by a binary predating the ingest gates: promotion inserts
    // candidates without re-entering `content_ingest`, so it must re-apply both the ceiling
    // and the bounded advance, or the legacy poison becomes a durable, relayable candidate.
    // The author's roster is present, proving the gates — not an unresolvable key — are what
    // drop the rows.
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x21; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");
    let over_ceiling = authored(&secret, owner, genesis.into(), ContentSpec {
        lamport: Some(u64::MAX),
        ..ContentSpec::default()
    });
    let over_advance = authored(&secret, owner, genesis.into(), ContentSpec {
        lamport: Some(1 << 33),
        ..ContentSpec::default()
    });
    for poison in [&over_ceiling, &over_advance] {
        conn.execute(
            "INSERT INTO content_pre_verify(
                     signed_hash, entry_hash, claimed_stream_id, claimed_author_account_id,
                     claimed_fingerprint, roster_ref, raw_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)",
            params![
                cbor::sha256(&poison.signed_bytes).as_slice(),
                poison.entry_hash.as_slice(),
                STREAM.as_slice(),
                owner.to_bytes().as_slice(),
                secret.public().fingerprint().to_bytes().as_slice(),
                genesis.as_slice(),
                poison.signed_bytes.as_slice(),
            ],
        )
        .unwrap();
    }

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    promote_pre_verify_for_account(&tx, owner, 1).unwrap();
    tx.commit().unwrap();
    let candidates: i64 =
        conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(candidates, 0, "neither violating row is promoted to a candidate");
    let parked: i64 =
        conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row.get(0)).unwrap();
    assert_eq!(parked, 0, "the violating rows are dropped, not retried forever");
}

/// Store a candidate row directly, as a pre-clamp binary would have stored (and possibly
/// accepted) it, and queue its stream for a refold. The ingest gates refuse these envelopes
/// now, so tests of the fold clamp's and the upgrade purge's handling of LEGACY state cannot
/// route them through `content_ingest`.
fn plant_legacy_candidate(conn: &Connection, entry: &SignedContentEntry, accepted: bool) {
    conn.execute(
        "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, lamport, accepted,
                 signed_bytes, received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 1)",
        params![
            entry.entry_hash.as_slice(),
            entry.header.stream_id.to_bytes().as_slice(),
            entry.header.author_account_id.to_bytes().as_slice(),
            entry.header.device_fingerprint.to_bytes().as_slice(),
            entry.header.seq.to_be_bytes().as_slice(),
            entry.header.prev_hash.as_ref().map(AccountEntryHash::as_slice),
            entry.header.grant_id.as_ref().map(GrantId::as_slice),
            entry.header.roster_ref.as_slice(),
            entry.header.owner_auth_len.to_be_bytes().as_slice(),
            entry.header.author_auth_len.to_be_bytes().as_slice(),
            stored_lamport(entry.header.lamport),
            accepted,
            entry.signed_bytes.as_slice(),
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO content_streams_pending_refold(
                 stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms)
             VALUES(?1, 1, 0, 0)
             ON CONFLICT(stream_id) DO UPDATE SET
                 reason_mask = content_streams_pending_refold.reason_mask | 1",
        [entry.header.stream_id.to_bytes().as_slice()],
    )
    .unwrap();
}

#[test]
fn a_bounded_advance_jump_is_dropped_at_ingest_before_storage() {
    let conn = db();
    let owner_secret = DeviceSecret::from_seed(&[0x31; 32]);
    let author_secret = DeviceSecret::from_seed(&[0x32; 32]);
    let (owner, owner_genesis) = roster(&conn, &owner_secret);
    let (author, author_genesis) = roster(&conn, &author_secret);
    let grant_id = [0x67; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, owner_genesis.into(), owner, &owner_secret, "owner");
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, author, "writer");

    let honest = authored_op(
        &owner_secret,
        owner,
        owner_genesis.into(),
        ContentSpec::default(),
        &node_create("honest"),
    );
    assert_eq!(verdict_after_ingest(&conn, &honest), ("accepted".into(), 1));

    // Sub-ceiling, but jumping the accepted stream clock past the bounded advance: dropped
    // BEFORE storage. This is the resend gate — a not-yet-upgraded peer re-offering a purged
    // poison (or a purged honest tail) must not re-park it as a wedging candidate chain tail.
    let poison = authored_op(
        &author_secret,
        author,
        author_genesis.into(),
        ContentSpec {
            grant_id: Some(GrantId::from_bytes(grant_id)),
            lamport: Some(2 + crate::entry::MAX_LAMPORT_ADVANCE),
            ..ContentSpec::default()
        },
        &node_create("poison"),
    );
    let ContentIngestOutcome::Rejected(reason) =
        content_ingest(&conn, &poison.signed_bytes, 1).unwrap()
    else {
        panic!("a bounded-advance jump must be dropped at ingest");
    };
    assert!(reason.contains("past the accepted stream clock"), "{reason}");
    let stored: i64 =
        conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(stored, 1, "the drop stores nothing");
    assert_eq!(projected_node_ids(&conn), vec!["honest".to_string()]);

    // The stream is untouched: the owner's next honest tick still folds accepted.
    let next = authored_op(
        &owner_secret,
        owner,
        owner_genesis.into(),
        ContentSpec { seq: 1, previous: Some(honest.entry_hash), ..ContentSpec::default() },
        &node_create("next"),
    );
    assert_eq!(verdict_after_ingest(&conn, &next), ("accepted".into(), 1));
}

#[test]
fn an_unauthenticated_high_lamport_envelope_parks_pre_verify_instead_of_rejecting() {
    // The bounded-advance gate runs only AFTER signature verification, so an envelope whose
    // roster is unknown — the unauthenticated case — parks pre-verify like any other, capacity
    // bounded, without buying the O(stream) accepted-clock scan. Promotion re-applies the
    // gate when the roster arrives (`promotion_drops_pre_verify_rows_that_violate_the_lamport
    // _gates`), so parking is not admission.
    let conn = db();
    let stranger = DeviceSecret::from_seed(&[0x99; 32]);
    let strange_account = AccountId::from_bytes([0x99; 32]);
    let jump =
        authored(&stranger, strange_account, RosterRef::from_bytes([0x98; 32]), ContentSpec {
            lamport: Some(1 << 33),
            ..ContentSpec::default()
        });
    assert_eq!(
        content_ingest(&conn, &jump.signed_bytes, 1).unwrap(),
        ContentIngestOutcome::PreVerify
    );
}

#[test]
fn the_upgrade_purge_retires_a_poisoned_clock_and_unwedges_the_dependent_tail() {
    let conn = db();
    let owner_secret = DeviceSecret::from_seed(&[0x31; 32]);
    let author_secret = DeviceSecret::from_seed(&[0x32; 32]);
    let (owner, owner_genesis) = roster(&conn, &owner_secret);
    let (author, author_genesis) = roster(&conn, &author_secret);
    let grant_id = [0x67; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, owner_genesis.into(), owner, &owner_secret, "owner");
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, author, "writer");

    // The pre-clamp wreck, planted as the old binary left it: the grantee's poison was
    // ACCEPTED, and the owner then honestly minted `poison + 1` — both over-advance now, and
    // the owner's high entry is its chain tail, so merely parking them wedges every future
    // owner write (each continuation ticks backwards from the parked tail).
    let honest = authored(&owner_secret, owner, owner_genesis.into(), ContentSpec::default());
    assert_eq!(verdict_after_ingest(&conn, &honest), ("accepted".into(), 1));
    let poison = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(grant_id)),
        lamport: Some(1 << 33),
        ..ContentSpec::default()
    });
    let inherited = authored(&owner_secret, owner, owner_genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(honest.entry_hash),
        lamport: Some((1 << 33) + 1),
        ..ContentSpec::default()
    });
    plant_legacy_candidate(&conn, &poison, true);
    plant_legacy_candidate(&conn, &inherited, true);

    purge_legacy_lamport_violators(&conn).unwrap();
    let remaining: Vec<Vec<u8>> = conn
        .prepare("SELECT entry_hash FROM content_entries")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        remaining,
        vec![honest.entry_hash.as_slice().to_vec()],
        "only the sane prefix survives"
    );
    let orphaned_status: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_entry_status WHERE entry_hash != ?1",
            [honest.entry_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(orphaned_status, 0, "deleted candidates leave no status rows behind");

    // The repair the purge exists for: with the chain re-rooted at the surviving prefix, an
    // honestly-clocked continuation folds ACCEPTED — parked-in-place tails would have forced
    // this to park as a backwards tick forever.
    let continuation = authored(&owner_secret, owner, owner_genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(honest.entry_hash),
        lamport: Some(2),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &continuation), ("accepted".into(), 1));
}

#[test]
fn an_over_ceiling_candidate_is_purged_even_when_never_accepted() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x21; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");
    let honest = authored(&secret, owner, genesis.into(), ContentSpec::default());
    assert_eq!(verdict_after_ingest(&conn, &honest), ("accepted".into(), 1));

    // A pre-clamp over-ceiling envelope the old fold REJECTED (never accepted): excluded from
    // the purge's clock basis, but still protocol-invalid and still advertised to peers that
    // refuse it before storage — it must be purged unconditionally, and its stored chain
    // suffix with it (density).
    let stranger = DeviceSecret::from_seed(&[0x99; 32]);
    let strange_account = AccountId::from_bytes([0x99; 32]);
    let over_ceiling =
        authored(&stranger, strange_account, RosterRef::from_bytes([0x98; 32]), ContentSpec {
            lamport: Some(u64::MAX),
            ..ContentSpec::default()
        });
    let suffix =
        authored(&stranger, strange_account, RosterRef::from_bytes([0x98; 32]), ContentSpec {
            seq: 1,
            previous: Some(over_ceiling.entry_hash),
            lamport: Some(3),
            ..ContentSpec::default()
        });
    plant_legacy_candidate(&conn, &over_ceiling, false);
    plant_legacy_candidate(&conn, &suffix, false);

    purge_legacy_lamport_violators(&conn).unwrap();
    let remaining: Vec<Vec<u8>> = conn
        .prepare("SELECT entry_hash FROM content_entries")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        remaining,
        vec![honest.entry_hash.as_slice().to_vec()],
        "the never-accepted over-ceiling row and its chain suffix are purged"
    );
}

#[test]
fn revoking_the_writer_that_set_the_clock_basis_does_not_wedge_the_owners_chain() {
    let conn = db();
    let owner_secret = DeviceSecret::from_seed(&[0x31; 32]);
    let author_secret = DeviceSecret::from_seed(&[0x32; 32]);
    let (owner, owner_genesis) = roster(&conn, &owner_secret);
    let (author, author_genesis) = roster(&conn, &author_secret);
    let grant_id = [0x67; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, owner_genesis.into(), owner, &owner_secret, "owner");
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, author, "writer");

    // The granted writer legitimately pushes the clock to the edge of the bound, and the
    // owner then honestly ticks past it.
    let advance = crate::entry::MAX_LAMPORT_ADVANCE;
    let g0 = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(grant_id)),
        ..ContentSpec::default()
    });
    let basis = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(grant_id)),
        seq: 1,
        previous: Some(g0.entry_hash),
        lamport: Some(advance + 1),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &g0), ("accepted".into(), 1));
    assert_eq!(verdict_after_ingest(&conn, &basis), ("accepted".into(), 1));
    let dependent = authored(&owner_secret, owner, owner_genesis.into(), ContentSpec {
        lamport: Some(advance + 2),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &dependent), ("accepted".into(), 1));

    // Revoke the writer: close the grant with a chain cut below the basis, condemning it.
    conn.execute("UPDATE account_stream_grants SET closed_at = 2 WHERE grant_id = ?1", [
        grant_id.as_slice()
    ])
    .unwrap();
    conn.execute(
        "INSERT INTO account_stream_grant_cuts(
                 grant_id, owner_account_id, device_fingerprint, seq, entry_hash)
             VALUES(?1, ?2, ?3, ?4, ?5)",
        params![
            grant_id.as_slice(),
            owner.to_bytes().as_slice(),
            author_secret.public().fingerprint().to_bytes().as_slice(),
            0_u64.to_be_bytes().as_slice(),
            g0.entry_hash.as_slice(),
        ],
    )
    .unwrap();

    // The owner keeps authoring after the revocation: the condemned basis props the clock
    // floor, so the dependent tick stays accepted and its continuation folds accepted —
    // revocation repairs the stream, it must not wedge it.
    let continuation = authored(&owner_secret, owner, owner_genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(dependent.entry_hash),
        lamport: Some(advance + 3),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &continuation), ("accepted".into(), 1));
    assert_eq!(verdict(&conn, &basis.entry_hash), ("condemned{beyond_cut}".into(), 0));
    assert_eq!(verdict(&conn, &g0.entry_hash), ("accepted".into(), 1));
    assert_eq!(verdict(&conn, &dependent.entry_hash), ("accepted".into(), 1));
}

#[test]
fn a_valid_fork_sibling_survives_the_purge_of_its_violating_rival() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x21; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // A fork at seq 1: the ACCEPTED winner is honest; the loser is over-ceiling and carries a
    // stored child. Deletion keyed by (chain, seq) would sweep the honest winner with the
    // loser — irreversible accepted-content loss — so the purge must follow the loser's hash
    // branch only.
    let root = authored(&secret, owner, genesis.into(), ContentSpec::default());
    let winner = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(root.entry_hash),
        ..ContentSpec::default()
    });
    let loser = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(root.entry_hash),
        lamport: Some(u64::MAX),
        ..ContentSpec::default()
    });
    let loser_child = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 2,
        previous: Some(loser.entry_hash),
        lamport: Some(3),
        ..ContentSpec::default()
    });
    plant_legacy_candidate(&conn, &root, true);
    plant_legacy_candidate(&conn, &winner, true);
    plant_legacy_candidate(&conn, &loser, false);
    plant_legacy_candidate(&conn, &loser_child, false);

    purge_legacy_lamport_violators(&conn).unwrap();
    let mut remaining: Vec<Vec<u8>> = conn
        .prepare("SELECT entry_hash FROM content_entries")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    remaining.sort();
    let mut expected =
        vec![root.entry_hash.as_slice().to_vec(), winner.entry_hash.as_slice().to_vec()];
    expected.sort();
    assert_eq!(
        remaining, expected,
        "the violating fork branch retires; the accepted winner at the same seq survives"
    );
}

#[test]
fn a_backwards_descendant_cannot_shield_its_poisoned_ancestor_from_the_purge() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x21; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // A crafted pre-clamp accepted chain whose descendant ticks BACKWARDS: the ascending
    // walk meets the descendant first, and without the monotonicity cuts its lamport would
    // advance the running max far enough to make the poisoned ancestor look in-bounds —
    // leaving the whole chain stored, parked at refold, and wedging future authoring.
    let poison = authored(&secret, owner, genesis.into(), ContentSpec {
        lamport: Some(2 * crate::entry::MAX_LAMPORT_ADVANCE),
        ..ContentSpec::default()
    });
    let backwards = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(poison.entry_hash),
        lamport: Some(crate::entry::MAX_LAMPORT_ADVANCE),
        ..ContentSpec::default()
    });
    plant_legacy_candidate(&conn, &poison, true);
    plant_legacy_candidate(&conn, &backwards, true);

    purge_legacy_lamport_violators(&conn).unwrap();
    let remaining: i64 =
        conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(remaining, 0, "the poisoned chain retires whole; nothing shields it");
}

#[test]
fn the_lamport_backfill_fills_null_columns_from_the_envelopes_and_skips_junk() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x21; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    // A legacy row: stored without the denormalized column (as a pre-V114 binary left it).
    let entry = authored(&secret, owner, genesis.into(), ContentSpec {
        lamport: Some(7),
        ..ContentSpec::default()
    });
    conn.execute(
        "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?7, 1, ?8, 1)",
        params![
            entry.entry_hash.as_slice(),
            STREAM.as_slice(),
            owner.to_bytes().as_slice(),
            secret.public().fingerprint().to_bytes().as_slice(),
            0_u64.to_be_bytes().as_slice(),
            genesis.as_slice(),
            0_u64.to_be_bytes().as_slice(),
            entry.signed_bytes.as_slice(),
        ],
    )
    .unwrap();
    // An undecodable blob keeps NULL — invisible to MAX, as the decoding scan treated it.
    conn.execute(
        "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?7, 1, x'00', 1)",
        params![
            [0xEE_u8; 32].as_slice(),
            STREAM.as_slice(),
            owner.to_bytes().as_slice(),
            secret.public().fingerprint().to_bytes().as_slice(),
            1_u64.to_be_bytes().as_slice(),
            genesis.as_slice(),
            0_u64.to_be_bytes().as_slice(),
        ],
    )
    .unwrap();
    backfill_content_lamport(&conn).unwrap();
    let filled: Option<i64> = conn
        .query_row(
            "SELECT lamport FROM content_entries WHERE entry_hash = ?1",
            [entry.entry_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(filled, Some(7), "the decodable row is backfilled from its envelope");
    let junk: Option<i64> = conn
        .query_row(
            "SELECT lamport FROM content_entries WHERE entry_hash = ?1",
            [[0xEE_u8; 32].as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(junk, None, "an undecodable blob stays NULL");
}

#[test]
fn the_upgrade_purge_drops_over_ceiling_pre_verify_rows_and_keeps_sane_ones() {
    let conn = db();
    let stranger = DeviceSecret::from_seed(&[0x99; 32]);
    let strange_account = AccountId::from_bytes([0x99; 32]);
    // Both rows have an unresolvable roster (the legacy park state); only the lamport differs.
    let over_ceiling =
        authored(&stranger, strange_account, RosterRef::from_bytes([0x98; 32]), ContentSpec {
            lamport: Some(u64::MAX),
            ..ContentSpec::default()
        });
    let sane = authored(
        &stranger,
        strange_account,
        RosterRef::from_bytes([0x98; 32]),
        ContentSpec::default(),
    );
    for entry in [&over_ceiling, &sane] {
        conn.execute(
            "INSERT INTO content_pre_verify(
                     signed_hash, entry_hash, claimed_stream_id, claimed_author_account_id,
                     claimed_fingerprint, roster_ref, raw_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)",
            params![
                cbor::sha256(&entry.signed_bytes).as_slice(),
                entry.entry_hash.as_slice(),
                STREAM.as_slice(),
                strange_account.to_bytes().as_slice(),
                stranger.public().fingerprint().to_bytes().as_slice(),
                [0x98_u8; 32].as_slice(),
                entry.signed_bytes.as_slice(),
            ],
        )
        .unwrap();
    }
    purge_legacy_lamport_violators(&conn).unwrap();
    let kept: Vec<Vec<u8>> = conn
        .prepare("SELECT entry_hash FROM content_pre_verify")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        kept,
        vec![sane.entry_hash.as_slice().to_vec()],
        "only the over-ceiling row is dropped"
    );
}

#[test]
fn a_lamport_jump_past_the_bounded_advance_parks_instead_of_winning_lww() {
    let conn = db();
    let owner_secret = DeviceSecret::from_seed(&[0x31; 32]);
    let author_secret = DeviceSecret::from_seed(&[0x32; 32]);
    let (owner, owner_genesis) = roster(&conn, &owner_secret);
    let (author, author_genesis) = roster(&conn, &author_secret);
    let grant_id = [0x67; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, owner_genesis.into(), owner, &owner_secret, "owner");
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, author, "writer");

    let honest = authored_op(
        &owner_secret,
        owner,
        owner_genesis.into(),
        ContentSpec::default(),
        &node_create("honest"),
    );
    assert_eq!(verdict_after_ingest(&conn, &honest), ("accepted".into(), 1));

    // A stored jump past the bounded advance — legacy state, or an entry that slipped the
    // advisory ingest gate through a clock race: the authoritative fold parks it instead of
    // letting it dominate LWW and poison the authoring clock.
    let poison = authored_op(
        &author_secret,
        author,
        author_genesis.into(),
        ContentSpec {
            grant_id: Some(GrantId::from_bytes(grant_id)),
            lamport: Some(2 + crate::entry::MAX_LAMPORT_ADVANCE),
            ..ContentSpec::default()
        },
        &node_create("poison"),
    );
    plant_legacy_candidate(&conn, &poison, false);
    settle_all(&conn);
    assert_eq!(verdict(&conn, &poison.entry_hash), ("parked{lamport_ahead}".into(), 0));
    assert_eq!(projected_node_ids(&conn), vec!["honest".to_string()]);

    // The stream is not bricked: the accepted clock ignored the parked jump, so the owner's
    // next honest tick still folds accepted.
    let next = authored_op(
        &owner_secret,
        owner,
        owner_genesis.into(),
        ContentSpec { seq: 1, previous: Some(honest.entry_hash), ..ContentSpec::default() },
        &node_create("next"),
    );
    assert_eq!(verdict_after_ingest(&conn, &next), ("accepted".into(), 1));
}

#[test]
fn a_lamport_violators_chain_descendants_park_with_it() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x21; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // seq 0 jumps past the bound; seq 1 carries a modest lamport of its own. Accepting the
    // descendant over its parked ancestor would break the dense-prefix invariant, so the
    // chain truncates at the violation. Planted, since ingest refuses the jump outright.
    let violator = authored(&secret, owner, genesis.into(), ContentSpec {
        lamport: Some(2 * crate::entry::MAX_LAMPORT_ADVANCE),
        ..ContentSpec::default()
    });
    let descendant = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(violator.entry_hash),
        lamport: Some(2),
        ..ContentSpec::default()
    });
    plant_legacy_candidate(&conn, &violator, false);
    plant_legacy_candidate(&conn, &descendant, false);
    settle_all(&conn);
    assert_eq!(verdict(&conn, &violator.entry_hash), ("parked{lamport_ahead}".into(), 0));
    assert_eq!(verdict(&conn, &descendant.entry_hash), ("parked{lamport_ahead}".into(), 0));
}

#[test]
fn junk_the_fold_never_accepted_cannot_shield_a_poison_from_the_purge() {
    let conn = db();
    let owner_secret = DeviceSecret::from_seed(&[0x31; 32]);
    let author_secret = DeviceSecret::from_seed(&[0x32; 32]);
    let stranger = DeviceSecret::from_seed(&[0x99; 32]);
    let strange_account = AccountId::from_bytes([0x99; 32]);
    let (owner, owner_genesis) = roster(&conn, &owner_secret);
    let (author, author_genesis) = roster(&conn, &author_secret);
    let grant_id = [0x67; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, owner_genesis.into(), owner, &owner_secret, "owner");
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, author, "writer");

    let honest = authored(&owner_secret, owner, owner_genesis.into(), ContentSpec::default());
    assert_eq!(verdict_after_ingest(&conn, &honest), ("accepted".into(), 1));
    // An UNGRANTED author's stored-but-never-accepted candidate, its lamport sitting exactly
    // one bound above zero. If the purge clock ranged over every stored row, this row would
    // advance the running max far enough to legitimize the accepted poison below — which the
    // queued refold (judging accepted work only) would then park as a wedging chain tail.
    let shield =
        authored(&stranger, strange_account, RosterRef::from_bytes([0x98; 32]), ContentSpec {
            lamport: Some(crate::entry::MAX_LAMPORT_ADVANCE),
            ..ContentSpec::default()
        });
    let poison = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(grant_id)),
        lamport: Some(2 * crate::entry::MAX_LAMPORT_ADVANCE),
        ..ContentSpec::default()
    });
    let inherited = authored(&owner_secret, owner, owner_genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(honest.entry_hash),
        lamport: Some(2 * crate::entry::MAX_LAMPORT_ADVANCE + 1),
        ..ContentSpec::default()
    });
    plant_legacy_candidate(&conn, &shield, false);
    plant_legacy_candidate(&conn, &poison, true);
    plant_legacy_candidate(&conn, &inherited, true);

    purge_legacy_lamport_violators(&conn).unwrap();
    let mut remaining: Vec<Vec<u8>> = conn
        .prepare("SELECT entry_hash FROM content_entries")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    remaining.sort();
    let mut expected =
        vec![honest.entry_hash.as_slice().to_vec(), shield.entry_hash.as_slice().to_vec()];
    expected.sort();
    assert_eq!(
        remaining, expected,
        "the accepted poison and its dependent tail are deleted; the never-accepted shield \
         neither survives them nor is itself purged"
    );
}

#[test]
fn a_chain_whose_lamport_ticks_backwards_truncates_at_the_first_non_increase() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x21; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // Both lamports sit comfortably inside the bounded advance — what demotes seq 1 is the
    // chain ticking backwards, which honest authoring (`max accepted + 1` per entry) never
    // produces. The prefix below the violation keeps its verdict.
    let first = authored(&secret, owner, genesis.into(), ContentSpec {
        lamport: Some(5),
        ..ContentSpec::default()
    });
    let backwards = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(first.entry_hash),
        lamport: Some(3),
        ..ContentSpec::default()
    });
    content_ingest(&conn, &first.signed_bytes, 1).unwrap();
    content_ingest(&conn, &backwards.signed_bytes, 1).unwrap();
    settle_all(&conn);
    assert_eq!(verdict(&conn, &first.entry_hash), ("accepted".into(), 1));
    assert_eq!(verdict(&conn, &backwards.entry_hash), ("parked{lamport_ahead}".into(), 0));
}

#[test]
fn an_honest_partitioned_backlog_folds_accepted_in_one_settle() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x21; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // A partition's worth of catch-up: each entry ticks the clock by one, so every step stays
    // inside the bounded advance and the whole backlog accepts in a single fold — the clamp
    // must never falsely reject an honest writer that was merely offline.
    let mut previous = None;
    let mut entries = Vec::new();
    for seq in 0..5_u64 {
        let entry = authored(&secret, owner, genesis.into(), ContentSpec {
            seq,
            previous,
            ..ContentSpec::default()
        });
        previous = Some(entry.entry_hash);
        entries.push(entry);
    }
    for entry in &entries {
        content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
    }
    settle_all(&conn);
    for entry in &entries {
        assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
    }
}

/// A live fold for `account` with no control-log rows held. Freshness measures a cited length
/// against held rows, so the consistent effective count here is zero (#1282).
fn seed_auth_state_live(conn: &Connection, account: AccountId) {
    conn.execute(
        "INSERT INTO account_auth_state(
                 account_id, classification, contested_depth, successor_account_id, \
         effective_count)
             VALUES(?1, 'live', NULL, NULL, 0)",
        params![account.to_bytes().as_slice()],
    )
    .unwrap();
}

#[test]
fn a_granted_contributor_authors_accepted_content_on_the_owner_stream() {
    // The grantee is THIS store's own (local) identity; the owner is a separate account whose
    // ownership + Writer grant this store has synced. Driving the real
    // `author_grantee_content_batch_in_tx` seam proves a contributor's locally-authored entry
    // folds ACCEPTED on the owner's stream (not just that the acceptance evaluator would admit
    // a hand-crafted one).
    let conn = db();
    let stream = StreamId::from_bytes(STREAM);
    let grantee = crate::account::local_account(&conn, NOW).unwrap();
    let owner = AccountId::from_bytes([0x51; 32]);
    assert_ne!(owner, grantee, "the owner is a separate identity from the contributor");
    seed_ownership(&conn, owner);
    seed_auth_state_live(&conn, owner);
    let grant_id = [0x71; 32];
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, grantee, "writer");

    // The contributor can find its own grant on the owner's stream.
    assert_eq!(
        crate::account::effective_writer_grant(&conn, owner, stream, grantee).unwrap(),
        Some(Into::into(grant_id)),
        "the reverse resolver finds the contributor's effective writer grant",
    );

    let hashes = {
        let tx =
            rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
        let h = crate::account::author_grantee_content_batch_in_tx(
            &tx,
            stream,
            owner,
            GrantId::from_bytes(grant_id),
            &[node_create("g1")],
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
        h
    };
    assert_eq!(hashes.len(), 1);
    assert_eq!(
        verdict(&conn, &hashes[0]),
        ("accepted".into(), 1),
        "the contributor's granted content folds accepted on the owner's stream",
    );
    assert!(
        projected_node_ids(&conn).contains(&"g1".to_string()),
        "and materializes into the owner-stream projection",
    );
}

/// The projection records the account that AUTHORED the winning anchor set — a grantee's, when
/// a contributor publishes it onto the owner's stream — never the stream owner. That difference
/// is the whole point of the column: the owner's devices must converge the grantee's set,
/// because the grantee's `anchors/1` never reaches them.
#[test]
fn a_grantee_authored_anchor_set_projects_the_grantee_as_its_author() {
    let conn = db();
    let stream = StreamId::from_bytes(STREAM);
    let grantee = crate::account::local_account(&conn, NOW).unwrap();
    let owner = AccountId::from_bytes([0x51; 32]);
    seed_ownership(&conn, owner);
    seed_auth_state_live(&conn, owner);
    let grant_id = [0x71; 32];
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, grantee, "writer");

    let anchors = vec![crate::op::PortableAnchor {
        binding_kind: "symbol".to_string(),
        binding_id: "src/lib.rs::run".to_string(),
        path: Some("src/lib.rs".to_string()),
        start_line: Some(1),
        end_line: Some(2),
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        created_at_ms: 7,
        symbol_kind: None,
        signature_hash: None,
        moniker_tool: None,
        moniker_tool_version: None,
    }];
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    crate::account::author_grantee_content_batch_in_tx(
        &tx,
        stream,
        owner,
        GrantId::from_bytes(grant_id),
        &[node_create("g1"), crate::op::MemoryOp::NodeAnchors {
            node_id: crate::op::NodeId::from("g1"),
            anchors,
        }],
        NOW,
    )
    .unwrap();
    tx.commit().unwrap();

    let nodes = crate::content_projection::list_projected_content_nodes(&conn, stream).unwrap();
    let node = nodes.iter().find(|node| node.node_id == "g1").expect("the node projects");
    assert_eq!(node.anchors_author, Some(grantee), "the grantee authored it, not the owner");
}

#[test]
fn a_contributor_whose_grant_only_reads_is_rejected() {
    let conn = db();
    let owner_secret = DeviceSecret::from_seed(&[0x41; 32]);
    let author_secret = DeviceSecret::from_seed(&[0x42; 32]);
    let owner = roster(&conn, &owner_secret).0;
    let (author, author_genesis) = roster(&conn, &author_secret);
    let grant_id = [0x68; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, author, "reader");

    let entry = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(grant_id)),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &entry), ("rejected{grant_not_writer}".into(), 0));
}

#[test]
fn an_equivocating_fork_accepts_the_smaller_hash_and_forks_the_loser() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x51; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // Two seq-0 entries on one coordinate — an equivocation. Both are authority-eligible, so
    // branch selection resolves the unforced fork by the smaller entry_hash; the loser is
    // terminal `forked`, never accepted.
    let a = authored(&secret, owner, genesis.into(), ContentSpec::default());
    let b = authored(&secret, owner, genesis.into(), ContentSpec {
        body: 0xf7,
        ..ContentSpec::default()
    });
    content_ingest(&conn, &a.signed_bytes, 1).unwrap();
    content_ingest(&conn, &b.signed_bytes, 2).unwrap();
    settle_all(&conn);

    let (winner, loser) = if a.entry_hash < b.entry_hash { (&a, &b) } else { (&b, &a) };
    assert_eq!(verdict(&conn, &winner.entry_hash), ("accepted".into(), 1));
    assert_eq!(verdict(&conn, &loser.entry_hash), ("forked".into(), 0));
}

#[test]
fn a_register_cut_condemns_an_entry_beyond_its_bound_watermark() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x61; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // A real seq-0 entry, then a roster content cut bounding this coordinate AT that held
    // watermark. seq 0 is on the cut (accepted); seq 1 is beyond it → condemned.
    let s0 = authored(&secret, owner, genesis.into(), ContentSpec::default());
    content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
    seed_roster_content_cut(&conn, genesis.into(), owner, 0, s0.entry_hash);
    let s1 = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(s0.entry_hash),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &s1), ("condemned{beyond_cut}".into(), 0));
    assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1));
}

#[test]
fn a_beyond_cut_entry_is_condemned_even_when_the_watermark_is_withheld() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x62; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");
    // A cut bounds this coordinate at seq 3, but its watermark has not synced. I11: the `[seq]`
    // condemns a beyond-cut entry from seq alone even while the watermark is withheld — a
    // withheld watermark must NOT launder a back-dated forgery into a park (the divergence this
    // guards against; parity with the account fold's P10 beyond-cut verdict).
    seed_roster_content_cut(&conn, genesis.into(), owner, 3, [0xcc; 32].into());

    let entry = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 5,
        previous: Some(AccountEntryHash::from_bytes([0xaa; 32])),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &entry), ("condemned{beyond_cut}".into(), 0));
}

#[test]
fn an_under_cut_entry_parks_while_the_watermark_is_withheld() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x64; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");
    // The same withheld cut at seq 3, but the entry is UNDER the cut (seq 0). Its on/off-branch
    // placement can't be decided until the watermark syncs, so it PARKS as `unknown_cut_target`
    // — never silently accepted, never condemned (I11: a withheld watermark never flips a
    // verdict). This is the correct under-cut park the beyond-cut fix must preserve.
    seed_roster_content_cut(&conn, genesis.into(), owner, 3, [0xcc; 32].into());

    let entry = authored(&secret, owner, genesis.into(), ContentSpec::default());
    assert_eq!(verdict_after_ingest(&conn, &entry), ("parked{unknown_cut_target}".into(), 0));
}

#[test]
fn a_cut_whose_watermark_names_a_foreign_coordinate_is_ignored() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x63; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // A dense chain seq 0 → 1, both accepted. Then a cut claims to bound seq 0 but names the
    // seq-1 entry as its watermark — a coordinate/seq mismatch. A malformed cut must not
    // condemn honest content (the §11.3 laundering guard); both stay accepted.
    let s0 = authored(&secret, owner, genesis.into(), ContentSpec::default());
    content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
    let s1 = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(s0.entry_hash),
        ..ContentSpec::default()
    });
    content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
    seed_roster_content_cut(&conn, genesis.into(), owner, 0, s1.entry_hash);
    run_account_trigger(&conn, owner);

    assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1));
    assert_eq!(verdict(&conn, &s1.entry_hash), ("accepted".into(), 1));
}

#[test]
fn a_contested_author_parks_as_contested_subject() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x71; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");
    seed_contested(&conn, owner);

    let entry = authored(&secret, owner, genesis.into(), ContentSpec::default());
    assert_eq!(verdict_after_ingest(&conn, &entry), ("parked{contested_subject}".into(), 0));
}

#[test]
fn an_author_ahead_of_our_fold_parks_for_refetch_not_rejects() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x81; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // The author cites a control-fold length we have not reached (our effective_count is 0).
    // Freshness is the LAST axis (§13): with authority and branch otherwise clear, this parks
    // for refetch rather than hardening into a rejection we would later walk back.
    let entry = authored(&secret, owner, genesis.into(), ContentSpec {
        auth_len: 9,
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &entry), ("parked{auth_len_ahead}".into(), 0));
}

/// Run the account→content trigger the way an account fold does — one IMMEDIATE txn.
fn run_account_trigger(conn: &Connection, account: AccountId) {
    run_account_trigger_owning(conn, account, &[]);
}

/// The trigger with an explicit pre-rewrite owned-stream set (what the account fold captures
/// before it rewrites the projection).
fn run_account_trigger_owning(
    conn: &Connection,
    account: AccountId,
    previously_owned: &[[u8; 32]],
) {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
    let streams = affected_streams_for_account(&tx, account, previously_owned).unwrap();
    finalize_affected_streams(&tx, &streams, NOW).unwrap();
    tx.commit().unwrap();
}

#[test]
fn content_that_arrives_before_its_owner_fact_is_classified_when_the_trigger_runs() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0x91; 32]);
    let (owner, genesis) = roster(&conn, &secret);

    // No `StreamOwn` fact yet: the ingest-time refold cannot evaluate authority, so the entry
    // keeps its structural `retained_unfolded` status rather than being wrongly
    // parked/rejected.
    let entry = authored(&secret, owner, genesis.into(), ContentSpec::default());
    assert_eq!(verdict_after_ingest(&conn, &entry), ("retained_unfolded".into(), 0));

    // The owner's ownership + roster facts fold; the account→content trigger reclassifies the
    // stream in that account-refold txn, and the entry reaches its real verdict.
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");
    run_account_trigger(&conn, owner);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
}

#[test]
fn a_cut_folding_later_retro_condemns_already_accepted_content() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xa1; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // A dense chain seq 0 → 1, both accepted once the ingests settle.
    let s0 = authored(&secret, owner, genesis.into(), ContentSpec::default());
    content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
    let s1 = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(s0.entry_hash),
        ..ContentSpec::default()
    });
    content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
    settle_all(&conn);
    assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1));
    assert_eq!(verdict(&conn, &s1.entry_hash), ("accepted".into(), 1));

    // A revocation bounds the coordinate at seq 0 (watermark = s0). On the next account fold
    // the trigger retro-condemns seq 1 (beyond the cut) while seq 0 stays accepted —
    // the revocation takes effect without any new content arriving (L2 enforceable).
    seed_roster_content_cut(&conn, genesis.into(), owner, 0, s0.entry_hash);
    run_account_trigger(&conn, owner);
    assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1));
    assert_eq!(verdict(&conn, &s1.entry_hash), ("condemned{beyond_cut}".into(), 0));
}

#[test]
fn losing_ownership_declassifies_previously_accepted_contributor_content() {
    let conn = db();
    let owner_secret = DeviceSecret::from_seed(&[0xb1; 32]);
    let author_secret = DeviceSecret::from_seed(&[0xb2; 32]);
    let owner = roster(&conn, &owner_secret).0;
    let (author, author_genesis) = roster(&conn, &author_secret);
    let grant_id = [0x69; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, author, "writer");

    // A contributor's entry accepts. The owner neither authored it nor (after the next step)
    // owns the stream, so only the pre-rewrite owned set can rediscover it.
    let entry = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(grant_id)),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &entry), ("accepted".into(), 1));

    // The owner's `StreamOwn` fact is dropped (owner contested / branch reselection).
    conn.execute("DELETE FROM account_stream_ownership WHERE account_id = ?1", [owner
        .to_bytes()
        .as_slice()])
        .unwrap();

    // Without the pre-rewrite owned set the orphaned stream is invisible to the trigger (the
    // owner no longer owns it and never authored it), so the stale acceptance would survive —
    // exactly the hole the captured set closes.
    run_account_trigger_owning(&conn, owner, &[]);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));

    // With it, the stream is refolded, finds no owner, and declassifies the entry.
    run_account_trigger_owning(&conn, owner, &[StreamId::from_bytes(STREAM).to_bytes()]);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("retained_unfolded".into(), 0));
}

#[test]
fn a_contested_owner_parks_contributor_content_even_when_the_author_is_live() {
    let conn = db();
    let owner_secret = DeviceSecret::from_seed(&[0xc1; 32]);
    let author_secret = DeviceSecret::from_seed(&[0xc2; 32]);
    let owner = roster(&conn, &owner_secret).0;
    let (author, author_genesis) = roster(&conn, &author_secret);
    let grant_id = [0x6a; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, author, "writer");
    // The OWNER is contested while the contributor stays live. The writer grant lives in the
    // owner's log, so a compromised owner poisons it: the content must fail closed.
    seed_contested(&conn, owner);

    let entry = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(grant_id)),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &entry), ("parked{contested_subject}".into(), 0));
}

#[test]
fn a_candidate_whose_stored_bytes_go_corrupt_is_declassified_not_left_accepted() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xd1; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    let entry = authored(&secret, owner, genesis.into(), ContentSpec::default());
    assert_eq!(verdict_after_ingest(&conn, &entry), ("accepted".into(), 1));

    // The stored envelope is corrupted (a torn write / bad blob). The next refold cannot decode
    // it, so it must lose `accepted` and its status — a candidate with no readable authority
    // basis must never stay live.
    conn.execute("UPDATE content_entries SET signed_bytes = ?1 WHERE entry_hash = ?2", params![
        [0_u8].as_slice(),
        entry.entry_hash.as_slice(),
    ])
    .unwrap();
    run_account_trigger(&conn, owner);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("retained_unfolded".into(), 0));
}

#[test]
fn an_orphaned_stream_whose_only_candidate_is_corrupt_still_loses_acceptance() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xe1; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    let entry = authored(&secret, owner, genesis.into(), ContentSpec::default());
    assert_eq!(verdict_after_ingest(&conn, &entry), ("accepted".into(), 1));

    // Worst case: ownership disappears AND the sole stored envelope is corrupt, so the refold
    // resolves no owner and decodes no headers. The declassify path must STILL clear acceptance
    // — an empty header list must not early-return past the `accepted = 0` clear.
    conn.execute("DELETE FROM account_stream_ownership WHERE account_id = ?1", [owner
        .to_bytes()
        .as_slice()])
        .unwrap();
    conn.execute("UPDATE content_entries SET signed_bytes = ?1 WHERE entry_hash = ?2", params![
        [0_u8].as_slice(),
        entry.entry_hash.as_slice()
    ])
    .unwrap();
    run_account_trigger_owning(&conn, owner, &[StreamId::from_bytes(STREAM).to_bytes()]);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("retained_unfolded".into(), 0));
}

#[test]
fn a_row_whose_blob_was_swapped_for_a_different_valid_envelope_is_not_classified() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xf1; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    let a = authored(&secret, owner, genesis.into(), ContentSpec::default());
    assert_eq!(verdict_after_ingest(&conn, &a), ("accepted".into(), 1));

    // Replace A's stored blob with a DIFFERENT (still valid) envelope, under A's row key. The
    // refold must not classify A's row under B's header just because B decodes — the decoded
    // `entry_hash` no longer matches the key, so the row is treated as absent and declassified.
    let b = authored(&secret, owner, genesis.into(), ContentSpec {
        body: 0xf7,
        ..ContentSpec::default()
    });
    assert_ne!(a.entry_hash, b.entry_hash);
    conn.execute("UPDATE content_entries SET signed_bytes = ?1 WHERE entry_hash = ?2", params![
        b.signed_bytes.as_slice(),
        a.entry_hash.as_slice()
    ])
    .unwrap();
    run_account_trigger(&conn, owner);
    assert_eq!(verdict(&conn, &a.entry_hash), ("retained_unfolded".into(), 0));
}

#[test]
fn a_descendant_of_a_freshness_parked_entry_does_not_accept_out_of_prefix() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xd2; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // seq 0 cites a control-fold length ahead of ours (parks on freshness); seq 1 cites a
    // current length. An attacker varies `auth_len` DOWN the chain to try to slip seq 1 in as
    // accepted over a parked seq 0. The accepted set must stay a prefix from seq 0, so neither
    // accepts while seq 0 is parked.
    let s0 = authored(&secret, owner, genesis.into(), ContentSpec {
        auth_len: 9,
        ..ContentSpec::default()
    });
    content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
    let s1 = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(s0.entry_hash),
        auth_len: 0,
        ..ContentSpec::default()
    });
    content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
    settle_all(&conn);

    assert_eq!(verdict(&conn, &s0.entry_hash), ("parked{auth_len_ahead}".into(), 0));
    assert_eq!(verdict(&conn, &s1.entry_hash), ("parked{auth_len_ahead}".into(), 0));
}

#[test]
fn an_eligible_child_of_an_ineligible_parent_parks_rather_than_forks() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xd3; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // seq 0 is the owner citing a grant it must not have → rejected (ineligible). seq 1 is a
    // clean owner entry building on it: authority-eligible, but with no accepted parent to
    // extend. It is stranded, not a contest loser, so it parks (recoverable) — never `forked`.
    let s0 = authored(&secret, owner, genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes([0x6b; 32])),
        ..ContentSpec::default()
    });
    content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
    let s1 = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(s0.entry_hash),
        ..ContentSpec::default()
    });
    content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
    settle_all(&conn);

    assert_eq!(verdict(&conn, &s0.entry_hash), ("rejected{unexpected_grant}".into(), 0));
    assert_eq!(verdict(&conn, &s1.entry_hash), ("parked{missing_predecessor}".into(), 0));
}

#[test]
fn the_account_trigger_is_a_noop_before_the_content_tables_exist() {
    // Mid-migration: the V064/V065 authority backfill folds every existing account before the
    // `/3` tables are created. The account→content trigger must not query `content_entries`
    // (which does not exist yet) — otherwise a populated DB fails to upgrade.
    let conn = Connection::open_in_memory().unwrap();
    let owner = signed_roster(&DeviceSecret::from_seed(&[0xe2; 32])).0;
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let streams = affected_streams_for_account(&tx, owner, &[]).unwrap();
    finalize_affected_streams(&tx, &streams, NOW).unwrap();
    tx.commit().unwrap();
}

#[test]
fn the_account_trigger_skips_the_reproject_before_the_projected_tables_exist() {
    // The V064/V065 authority backfill can also fold accounts AFTER the `/3` candidate tables
    // exist but BEFORE V070 creates `content_projected_*`: a content refold runs (so
    // `content_entries_exists` passes) but the reproject targets absent tables. Simulate that
    // window by dropping the V070 tables; the trigger must still refold cleanly.
    let conn = db();
    conn.execute_batch("DROP TABLE content_projected_nodes; DROP TABLE content_projected_edges;")
        .unwrap();
    let secret = DeviceSecret::from_seed(&[0xe3; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    let entry = authored(&secret, owner, genesis.into(), ContentSpec::default());
    content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
    run_account_trigger(&conn, owner);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
}

#[test]
fn an_account_fold_that_retro_condemns_content_drops_it_from_the_projection() {
    // #683: the account→content trigger re-derives `accepted` — and a revocation can FLIP it.
    // The reconcile's anti-join trusts `content_projected_*` to mirror `accepted`, so the
    // trigger must reproject every stream it refolds.
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xe4; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // A dense chain seq 0 → 1 carrying real ops; both accept and project on settle.
    let s0 =
        authored_op(&secret, owner, genesis.into(), ContentSpec::default(), &node_create("mem_a"));
    content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
    let s1 = authored_op(
        &secret,
        owner,
        genesis.into(),
        ContentSpec { seq: 1, previous: Some(s0.entry_hash), ..ContentSpec::default() },
        &node_create("mem_b"),
    );
    content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
    settle_all(&conn);
    assert_eq!(verdict(&conn, &s1.entry_hash), ("accepted".into(), 1));
    assert_eq!(projected_node_ids(&conn), vec!["mem_a".to_string(), "mem_b".to_string()]);

    // A revocation bounds the coordinate at seq 0: the account fold retro-condemns seq 1, and
    // the same txn must drop its node from the projection — a stale row here would make the
    // reconcile's anti-join treat mem_b as still authored.
    seed_roster_content_cut(&conn, genesis.into(), owner, 0, s0.entry_hash);
    run_account_trigger(&conn, owner);
    assert_eq!(verdict(&conn, &s1.entry_hash), ("condemned{beyond_cut}".into(), 0));
    assert_eq!(
        projected_node_ids(&conn),
        vec!["mem_a".to_string()],
        "the retro-condemned entry's node leaves the projection in the refold txn",
    );
}

#[test]
fn an_account_fold_that_accepts_parked_content_projects_it() {
    // #683, the other direction: content arrives BEFORE its owner fact, so it cannot accept
    // yet; when the authority facts fold, the trigger flips it to accepted — and the node
    // must APPEAR in the projection in the same txn, or the reconcile would re-author it.
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xe5; 32]);
    let (owner, genesis) = roster(&conn, &secret);

    let entry =
        authored_op(&secret, owner, genesis.into(), ContentSpec::default(), &node_create("mem_a"));
    assert_eq!(verdict_after_ingest(&conn, &entry), ("retained_unfolded".into(), 0));
    assert_eq!(projected_node_ids(&conn), Vec::<String>::new());

    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");
    run_account_trigger(&conn, owner);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
    assert_eq!(
        projected_node_ids(&conn),
        vec!["mem_a".to_string()],
        "the newly-accepted entry's node enters the projection in the refold txn",
    );
}

#[test]
fn a_reader_grants_revoke_cut_cannot_steer_writer_content_branch_selection() {
    let conn = db();
    let owner_secret = DeviceSecret::from_seed(&[0xf3; 32]);
    let author_secret = DeviceSecret::from_seed(&[0xf4; 32]);
    let owner = roster(&conn, &owner_secret).0;
    let (author, author_genesis) = roster(&conn, &author_secret);
    let writer_grant = [0x71; 32];
    let reader_grant = [0x72; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(writer_grant), owner, author, "writer");

    // Two writer entries at seq 0 (equivocation): the smaller entry_hash wins the unforced
    // fork.
    let a = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(writer_grant)),
        body: 0xf6,
        ..ContentSpec::default()
    });
    let b = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(writer_grant)),
        body: 0xf7,
        ..ContentSpec::default()
    });
    content_ingest(&conn, &a.signed_bytes, 1).unwrap();
    content_ingest(&conn, &b.signed_bytes, 2).unwrap();
    let (winner, loser) = if a.entry_hash < b.entry_hash { (&a, &b) } else { (&b, &a) };

    // A revoked READER grant on the same coordinate carries a cut naming the hash-order LOSER.
    // A reader grant never authorizes a content write, so its cut must NOT pin selection — else
    // a peer could hijack the accepted branch by storing a rejected reader-grant entry.
    seed_closed_grant_with_cut(
        &conn,
        GrantId::from_bytes(reader_grant),
        owner,
        author,
        "reader",
        &author_secret,
        loser.entry_hash,
    );
    let reader_entry = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(reader_grant)),
        body: 0xf5,
        ..ContentSpec::default()
    });
    content_ingest(&conn, &reader_entry.signed_bytes, 3).unwrap();
    settle_all(&conn);

    assert_eq!(verdict(&conn, &reader_entry.entry_hash), ("rejected{grant_not_writer}".into(), 0));
    // Hash order still decides — the reader cut did not steer the writers' branch.
    assert_eq!(verdict(&conn, &winner.entry_hash), ("accepted".into(), 1));
    assert_eq!(verdict(&conn, &loser.entry_hash), ("forked".into(), 0));
}

// ---- #652: deferred/batch ingest refold + local-vs-global budget ----

#[derive(Clone, Copy, PartialEq)]
enum RefoldCadence {
    /// Settle after EVERY ingest — reproduces the pre-#652 per-entry refold cadence.
    PerEntry,
    /// Settle ONCE after the whole batch — the deferred/batch path this change introduces.
    Batch,
}

/// Ingest `entries` into a fresh DB seeded by `setup`, folding the deferred refold either after
/// every entry or once after the batch, and return the full sorted (entry_hash, status,
/// accepted) set. The acceptance fold is a pure function of the final candidate set, so both
/// cadences MUST produce a byte-identical result — this is what proves defer/batch changed only
/// WHEN the fold runs, not its outcome.
fn drive_ingest(
    entries: &[SignedContentEntry],
    setup: &dyn Fn(&Connection),
    cadence: RefoldCadence,
) -> Vec<(Vec<u8>, String, i64)> {
    let conn = db();
    setup(&conn);
    for entry in entries {
        content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
        if cadence == RefoldCadence::PerEntry {
            settle_all(&conn);
        }
    }
    // A trailing settle folds whatever is still deferred: the whole batch under `Batch`, and
    // nothing (an empty-queue no-op) under `PerEntry`.
    settle_all(&conn);
    all_verdicts(&conn)
}

fn all_verdicts(conn: &Connection) -> Vec<(Vec<u8>, String, i64)> {
    let mut stmt = conn
        .prepare(
            "SELECT e.entry_hash, s.status, e.accepted
                 FROM content_entries e JOIN content_entry_status s ON s.entry_hash = e.entry_hash
                 ORDER BY e.entry_hash",
        )
        .unwrap();
    stmt.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

fn pending_refold_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM content_streams_pending_refold", [], |row| row.get(0))
        .unwrap()
}

/// Settle with the UNBOUNDED budget — the behavior every pre-budget settle caller relied on
/// (drain the whole queue in one call). Tests written against that contract use this helper;
/// the budgeted tests pass an explicit [`ContentRefoldBudget`] instead.
fn settle_all(conn: &Connection) -> ContentSettleReport {
    settle_pending_content_refolds(conn, &ContentRefoldBudget::unbounded(), NOW).unwrap()
}

fn pending_refold_state(conn: &Connection) -> (i64, i64, i64) {
    conn.query_row(
        "SELECT reason_mask, first_enqueued_at_ms, last_enqueued_at_ms
             FROM content_streams_pending_refold",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

#[test]
fn deferred_settle_matches_per_entry_refold_on_an_honest_chain() {
    let secret = DeviceSecret::from_seed(&[0xa1; 32]);
    let (owner, genesis) = signed_roster(&secret);
    let genesis = genesis.entry_hash;

    // A dense honest chain seq 0 → 1 → 2, all fresh (auth_len 0) → all accept.
    let s0 = authored(&secret, owner, genesis.into(), ContentSpec::default());
    let s1 = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(s0.entry_hash),
        ..ContentSpec::default()
    });
    let s2 = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 2,
        previous: Some(s1.entry_hash),
        ..ContentSpec::default()
    });
    let entries = [s0, s1, s2];
    let setup = |conn: &Connection| {
        roster(conn, &secret);
        seed_ownership(conn, owner);
        seed_roster_fact(conn, genesis.into(), owner, &secret, "owner");
    };

    let per_entry = drive_ingest(&entries, &setup, RefoldCadence::PerEntry);
    let batch = drive_ingest(&entries, &setup, RefoldCadence::Batch);
    assert_eq!(per_entry, batch, "deferred batch settle equals per-entry refold, byte for byte");
    // Non-trivial: the fold actually accepted the chain (not two empty sets agreeing).
    assert!(
        batch.iter().all(|(_, status, accepted)| status == "accepted" && *accepted == 1),
        "the honest chain accepts end to end: {batch:?}",
    );
}

#[test]
fn deferred_settle_matches_per_entry_refold_on_an_adversarial_interleaving() {
    let secret = DeviceSecret::from_seed(&[0xa2; 32]);
    let (owner, genesis) = signed_roster(&secret);
    let genesis = genesis.entry_hash;

    // An equivocating fork at seq 0 (siblings a0/b0), plus a seq-1 descendant of a0 that cites
    // a DIFFERENT (ahead) auth_len — the varying-auth_len-down-the-chain attack.
    // Ingested OUT OF ORDER (descendant first, then one sibling, then the other) so the
    // per-entry cadence genuinely folds partial states mid-flight while the batch
    // cadence sees the whole set at once. Both must converge.
    let a0 = authored(&secret, owner, genesis.into(), ContentSpec {
        body: 0xf6,
        ..ContentSpec::default()
    });
    let b0 = authored(&secret, owner, genesis.into(), ContentSpec {
        body: 0xf7,
        ..ContentSpec::default()
    });
    let child = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(a0.entry_hash),
        auth_len: 9,
        ..ContentSpec::default()
    });
    let entries = [child, b0, a0];
    let setup = |conn: &Connection| {
        roster(conn, &secret);
        seed_ownership(conn, owner);
        seed_roster_fact(conn, genesis.into(), owner, &secret, "owner");
    };

    let per_entry = drive_ingest(&entries, &setup, RefoldCadence::PerEntry);
    let batch = drive_ingest(&entries, &setup, RefoldCadence::Batch);
    assert_eq!(
        per_entry, batch,
        "deferred batch settle equals per-entry refold across an out-of-order fork with varying \
         auth_len",
    );
    // Non-trivial: the fork resolved — one seq-0 sibling accepts, the other forks.
    let statuses: Vec<&str> = batch.iter().map(|(_, status, _)| status.as_str()).collect();
    assert!(statuses.contains(&"accepted"), "a seq-0 sibling accepts: {batch:?}");
    assert!(statuses.contains(&"forked"), "the losing seq-0 sibling forks: {batch:?}");
}

#[test]
fn deferred_settle_matches_per_entry_refold_across_a_cut() {
    let secret = DeviceSecret::from_seed(&[0xa3; 32]);
    let (owner, genesis) = signed_roster(&secret);
    let genesis = genesis.entry_hash;

    // seq 0 sits ON a roster content cut (accepted); seq 1 is BEYOND the bound watermark
    // (condemned). The cut exercises the condemn fold path under both cadences.
    let s0 = authored(&secret, owner, genesis.into(), ContentSpec::default());
    let s0_hash = s0.entry_hash;
    let s1 = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(s0_hash),
        ..ContentSpec::default()
    });
    let entries = [s0, s1];
    let setup = |conn: &Connection| {
        roster(conn, &secret);
        seed_ownership(conn, owner);
        seed_roster_fact(conn, genesis.into(), owner, &secret, "owner");
        seed_roster_content_cut(conn, genesis.into(), owner, 0, s0_hash);
    };

    let per_entry = drive_ingest(&entries, &setup, RefoldCadence::PerEntry);
    let batch = drive_ingest(&entries, &setup, RefoldCadence::Batch);
    assert_eq!(per_entry, batch, "deferred batch settle equals per-entry refold across a cut");
    let statuses: Vec<&str> = batch.iter().map(|(_, status, _)| status.as_str()).collect();
    assert!(statuses.contains(&"accepted"), "seq 0 on the cut accepts: {batch:?}");
    assert!(
        statuses.iter().any(|status| status.starts_with("condemned")),
        "seq 1 beyond the cut is condemned: {batch:?}",
    );
}

#[test]
fn ingest_defers_all_refolds_and_one_settle_folds_the_dirty_stream_once() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xb1; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    // Authority is present BEFORE ingest, so a refold — if one ran mid-ingest — WOULD accept
    // these entries. Observing them still structural after N ingests is the proof it did not.
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // A dense honest chain. Pre-#652 this ran one whole-stream refold PER entry (O(n) each,
    // O(n^2) cumulative under the writer lock).
    let mut entries = Vec::new();
    let mut previous = None;
    for seq in 0..6_u64 {
        let entry = authored(&secret, owner, genesis.into(), ContentSpec {
            seq,
            previous,
            ..ContentSpec::default()
        });
        previous = Some(entry.entry_hash);
        entries.push(entry);
    }
    for entry in &entries {
        content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
    }

    // BEFORE/AFTER measurement: the N ingests ran ZERO refolds. The refold is the only writer
    // of a non-structural verdict, so every entry sitting at its structural
    // `retained_unfolded` baseline — with authority a refold would have consumed to
    // accept it — proves none ran.
    for entry in &entries {
        assert_eq!(
            verdict(&conn, &entry.entry_hash),
            ("retained_unfolded".into(), 0),
            "no refold runs during ingest — the entry keeps its structural status",
        );
    }
    // N ingests to one stream deduped into ONE queued refold, not N.
    assert_eq!(pending_refold_count(&conn), 1, "N ingests to one stream queue one refold");

    // One settle folds exactly one dirty stream: O(dirty streams), NOT O(entries).
    assert_eq!(settle_all(&conn).settled_streams, 1, "one refold per dirty stream");
    assert_eq!(pending_refold_count(&conn), 0, "settle drains the queue");
    for entry in &entries {
        assert_eq!(
            verdict(&conn, &entry.entry_hash),
            ("accepted".into(), 1),
            "the single settle folds the whole chain to its acceptance verdict",
        );
    }
}

#[test]
fn remote_account_ingests_dedupe_account_change_and_defer_content_until_settle() {
    let conn = db();
    let founder = DeviceSecret::from_seed(&[0xb5; 32]);
    let member_a = DeviceSecret::from_seed(&[0xb6; 32]);
    let member_b = DeviceSecret::from_seed(&[0xb7; 32]);
    let (account, genesis) = roster(&conn, &founder);
    seed_ownership(&conn, account);
    seed_roster_fact(&conn, genesis.into(), account, &founder, "owner");

    let entry = authored(&founder, account, genesis.into(), ContentSpec::default());
    content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
    assert_eq!(settle_all(&conn).settled_streams, 1);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));

    let add_a = signed_device_add_at(&founder, &member_a, account, 1, genesis, genesis, 1);
    super::super::super::storage::account_ingest(&conn, &add_a.signed_bytes, 10).unwrap();
    let add_b = signed_device_add_at(&founder, &member_b, account, 2, add_a.entry_hash, genesis, 2);
    super::super::super::storage::account_ingest(&conn, &add_b.signed_bytes, 20).unwrap();

    assert_eq!(
        verdict(&conn, &entry.entry_hash),
        ("accepted".into(), 1),
        "remote account folds leave the last completed content verdict untouched",
    );
    assert_eq!(pending_refold_count(&conn), 1, "N account ingests dedupe per stream");
    assert_eq!(pending_refold_state(&conn), (PENDING_REFOLD_ACCOUNT_CHANGE, 10, 20));

    assert_eq!(settle_all(&conn).settled_streams, 1);
    assert_eq!(pending_refold_count(&conn), 0);
    assert_eq!(
        verdict(&conn, &entry.entry_hash),
        ("retained_unfolded".into(), 0),
        "one settle performs the deferred content fold once",
    );
}

#[test]
fn pending_refold_merges_content_and_account_reasons_without_moving_first_timestamp() {
    let conn = db();
    let founder = DeviceSecret::from_seed(&[0xb8; 32]);
    let member = DeviceSecret::from_seed(&[0xb9; 32]);
    let (account, genesis) = roster(&conn, &founder);
    seed_ownership(&conn, account);
    seed_roster_fact(&conn, genesis.into(), account, &founder, "owner");

    let entry = authored(&founder, account, genesis.into(), ContentSpec::default());
    content_ingest(&conn, &entry.signed_bytes, 5).unwrap();
    assert_eq!(pending_refold_state(&conn), (PENDING_REFOLD_CONTENT_CANDIDATE, 5, 5),);

    let add = signed_device_add(&founder, &member, account, genesis);
    super::super::super::storage::account_ingest(&conn, &add.signed_bytes, 11).unwrap();
    assert_eq!(
        pending_refold_state(&conn),
        (PENDING_REFOLD_CONTENT_CANDIDATE | PENDING_REFOLD_ACCOUNT_CHANGE, 5, 11),
        "reason bits OR, first enqueue stays stable, and last enqueue refreshes",
    );
}

#[test]
fn account_change_settle_reprojects_even_when_acceptance_is_unchanged() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xba; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");
    let entry = authored_op(
        &secret,
        owner,
        genesis.into(),
        ContentSpec::default(),
        &node_create("sealed-later"),
    );
    content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
    settle_all(&conn);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
    assert_eq!(projected_node_ids(&conn), vec!["sealed-later".to_string()]);

    // Model a body that was accepted while locally unprojectable, then became projectable when
    // account-side key material arrived. ACCOUNT_CHANGE must reproject even though acceptance
    // itself remains unchanged.
    conn.execute("DELETE FROM content_projected_nodes", []).unwrap();
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    queue_account_changed_streams(&tx, &[entry.header.stream_id], 2).unwrap();
    tx.commit().unwrap();

    assert_eq!(settle_all(&conn).settled_streams, 1);
    assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
    assert_eq!(projected_node_ids(&conn), vec!["sealed-later".to_string()]);
}

#[test]
fn a_trusted_account_fold_finalizes_and_clears_existing_queue_debt() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xb2; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // Two ingests to one stream leave exactly one queued refold (dedup), and it persists across
    // the separate ingest calls (crash-safety: the mark survives until a fold consumes it).
    let s0 = authored(&secret, owner, genesis.into(), ContentSpec::default());
    content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
    assert_eq!(pending_refold_count(&conn), 1, "the first ingest queues the stream");
    let s1 = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(s0.entry_hash),
        ..ContentSpec::default()
    });
    content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
    assert_eq!(pending_refold_count(&conn), 1, "a second ingest dedups onto the same queue row");

    // The trusted account-fold path finalizes the stream in this transaction: it updates
    // `accepted`, reprojects (#683/C5), and clears the already-satisfied queue debt only after
    // both duties succeed.
    run_account_trigger(&conn, owner);
    assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1), "it folds the stream");
    assert_eq!(
        pending_refold_count(&conn),
        0,
        "trusted finalization clears the queue after refold and reproject",
    );

    // The queue is already discharged, so settle has no duplicate work.
    assert_eq!(settle_all(&conn).settled_streams, 0, "settle does not repeat trusted finalization",);
}

#[test]
fn a_dense_continuation_of_a_settled_chain_is_retained_not_missing_predecessor() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xb4; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // Ingest seq 0 and SETTLE it — its status becomes `accepted`, no longer
    // `retained_unfolded`.
    let s0 = authored(&secret, owner, genesis.into(), ContentSpec::default());
    content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
    assert_eq!(settle_all(&conn).settled_streams, 1);
    assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1));

    // A dense continuation cites the now-SETTLED predecessor. Its STRUCTURAL classification
    // must see the predecessor as present (any status but missing-predecessor), so it
    // lands `retained_unfolded` — NOT wrongly `missing_predecessor` just because s0's
    // status moved off `retained_unfolded` at settle. The RETURNED status is the
    // contract, so assert it directly.
    let s1 = authored(&secret, owner, genesis.into(), ContentSpec {
        seq: 1,
        previous: Some(s0.entry_hash),
        ..ContentSpec::default()
    });
    assert_eq!(
        content_ingest(&conn, &s1.signed_bytes, 2).unwrap(),
        ContentIngestOutcome::Ingested { status: "retained_unfolded".into() },
        "a dense continuation of a settled chain reports the structural retained_unfolded status",
    );
    assert_eq!(verdict(&conn, &s1.entry_hash), ("retained_unfolded".into(), 0));
    // And it folds to accepted on settle, extending the settled prefix.
    assert_eq!(settle_all(&conn).settled_streams, 1);
    assert_eq!(verdict(&conn, &s1.entry_hash), ("accepted".into(), 1));
}

#[test]
fn settle_folds_each_dirty_stream_independently() {
    let conn = db();
    // Two distinct dirty streams. Each settles in its OWN txn: an ownerless stream declassifies
    // (and clears its mark), exercising the per-stream loop without cross-stream coupling.
    for stream in [[0x51_u8; 32], [0x52_u8; 32]] {
        conn.execute("INSERT INTO content_streams_pending_refold(stream_id) VALUES (?1)", [
            stream.as_slice()
        ])
        .unwrap();
    }
    assert_eq!(pending_refold_count(&conn), 2);
    assert_eq!(settle_all(&conn).settled_streams, 2, "each dirty stream settles exactly once",);
    assert_eq!(pending_refold_count(&conn), 0, "both marks cleared");
}

#[test]
fn local_device_history_is_excluded_from_the_global_cap_but_a_foreign_flood_still_trips() {
    let foreign_secret = DeviceSecret::from_seed(&[0xc1; 32]);

    // A LOCAL history the size of the whole global ceiling must NOT starve foreign ingest. The
    // exclusion keys on the local DEVICE FINGERPRINT (forge-proof — a row carries it only if
    // signed by the local key), NOT the attacker-settable author_account_id, so seed the
    // ceiling-sized history under the local device's own fingerprint.
    let excluded = db();
    let local_fp = crate::local_device(&excluded, 1).unwrap().fingerprint().to_bytes();
    seed_content_candidates(
        &excluded,
        AccountId::from_bytes([0xd2; 32]),
        local_fp,
        1,
        CANDIDATES_GLOBAL_MAX as usize,
        1,
    );
    let (foreign, foreign_roster) = roster(&excluded, &foreign_secret);
    let signed = content(&foreign_secret, foreign, foreign_roster.into(), 0, None);
    let verified =
        envelope::verify_content_signed(&signed.signed_bytes, &foreign_secret.public()).unwrap();
    {
        let tx = excluded.unchecked_transaction().unwrap();
        assert_eq!(
            candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
            None,
            "a global-cap-sized LOCAL-DEVICE history does not trip the foreign global cap",
        );
    }

    // The SAME volume of FOREIGN-signed entries (no local device fingerprint to exclude) still
    // trips the ceiling — the anti-flood budget is intact for genuine remote abuse.
    let flooded = db();
    seed_content_candidates(
        &flooded,
        AccountId::from_bytes([0xd1; 32]),
        FOREIGN_FP,
        1,
        CANDIDATES_GLOBAL_MAX as usize,
        1,
    );
    let (foreign, foreign_roster) = roster(&flooded, &foreign_secret);
    let signed = content(&foreign_secret, foreign, foreign_roster.into(), 0, None);
    let verified =
        envelope::verify_content_signed(&signed.signed_bytes, &foreign_secret.public()).unwrap();
    let tx = flooded.unchecked_transaction().unwrap();
    assert_eq!(
        candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
        Some(ContentCapacityScope::CandidateGlobal),
        "a genuine foreign flood past the global cap still trips CandidateGlobal",
    );
}

#[test]
fn settle_on_an_empty_queue_is_a_noop_and_a_second_settle_authors_nothing() {
    let conn = db();
    let secret = DeviceSecret::from_seed(&[0xb3; 32]);
    let (owner, genesis) = roster(&conn, &secret);
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");

    // Empty queue: settle folds nothing and returns 0.
    assert_eq!(settle_all(&conn).settled_streams, 0, "settle on an empty queue is a no-op",);

    // Ingest, then settle drains it; a SECOND settle finds the queue empty and changes nothing.
    let entry = authored(&secret, owner, genesis.into(), ContentSpec::default());
    content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
    assert_eq!(settle_all(&conn).settled_streams, 1, "the first settle folds the dirty stream",);
    let after_first = verdict(&conn, &entry.entry_hash);
    assert_eq!(settle_all(&conn).settled_streams, 0, "the second settle authors nothing new",);
    assert_eq!(
        verdict(&conn, &entry.entry_hash),
        after_first,
        "a redundant settle leaves the verdict unchanged",
    );
}

// ---- #698: budgeted, resumable deferred settlement ----

/// The V079 fold-cost unit for one synthetic candidate row: `length(signed_bytes) + 32`.
const SYNTHETIC_ROW_BYTES: u64 = 16 + 32;

/// Seed `count` synthetic candidates for `stream`. The bodies are deliberately NOT decodable
/// envelopes: the refold treats them as absent rows, so the stream settles through the
/// ownerless declassify path — enough to exercise queue admission, per-stream txns, and the
/// stats-trigger accounting without an authority fixture.
fn seed_synthetic_candidates(conn: &Connection, stream: [u8; 32], count: u64) {
    let first_ordinal = conn
        .query_row(
            "SELECT count(*) FROM content_entries WHERE stream_id = ?1",
            [stream.as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap() as u64;
    for ordinal in first_ordinal..first_ordinal + count {
        let entry_hash = cbor::sha256(&[&stream[..], &ordinal.to_be_bytes()[..]].concat());
        conn.execute(
            "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                     prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                     accepted, signed_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?7, 0, ?8, 1)",
            params![
                entry_hash.as_slice(),
                stream.as_slice(),
                [0xaa_u8; 32].as_slice(),
                [0xbb_u8; 32].as_slice(),
                ordinal.to_be_bytes().as_slice(),
                [0xcc_u8; 32].as_slice(),
                0_u64.to_be_bytes().as_slice(),
                vec![0xdd_u8; 16],
            ],
        )
        .unwrap();
    }
}

fn enqueue_refold(conn: &Connection, stream: [u8; 32], first_enqueued_at_ms: i64) {
    conn.execute(
        "INSERT INTO content_streams_pending_refold(
                 stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms)
             VALUES(?1, 1, ?2, ?2)",
        params![stream.as_slice(), first_enqueued_at_ms],
    )
    .unwrap();
}

fn queue_contains(conn: &Connection, stream: [u8; 32]) -> bool {
    conn.query_row(
        "SELECT count(*) FROM content_streams_pending_refold WHERE stream_id = ?1",
        [stream.as_slice()],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

fn stream_enqueued_at(conn: &Connection, stream: [u8; 32]) -> i64 {
    conn.query_row(
        "SELECT first_enqueued_at_ms FROM content_streams_pending_refold WHERE stream_id = ?1",
        [stream.as_slice()],
        |row| row.get(0),
    )
    .unwrap()
}

fn stream_stats(conn: &Connection, stream: [u8; 32]) -> (i64, i64) {
    conn.query_row(
        "SELECT candidate_count, candidate_bytes FROM content_stream_stats
             WHERE stream_id = ?1",
        [stream.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

fn budget(
    max_streams: u64,
    max_candidates: u64,
    max_candidate_bytes: u64,
    allow_one_oversize: bool,
) -> ContentRefoldBudget {
    ContentRefoldBudget { max_streams, max_candidates, max_candidate_bytes, allow_one_oversize }
}

#[test]
fn settle_budget_stops_at_the_stream_limit_and_resumes_oldest_first() {
    let conn = db();
    let oldest = [0x11_u8; 32];
    let middle = [0x12_u8; 32];
    let newest = [0x13_u8; 32];
    for (stream, enqueued_at) in [(oldest, 1), (middle, 2), (newest, 3)] {
        seed_synthetic_candidates(&conn, stream, 1);
        enqueue_refold(&conn, stream, enqueued_at);
    }
    let one_stream = budget(1, u64::MAX, u64::MAX, false);

    let first = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    assert_eq!(first.settled_streams, 1);
    assert_eq!(first.consumed_candidates, 1);
    assert_eq!(first.consumed_candidate_bytes, SYNTHETIC_ROW_BYTES);
    assert_eq!(first.deferred_budget, 2, "the rest of the queue fits a fresh budget");
    assert_eq!(first.deferred_oversize, 0);
    assert!(first.failures.is_empty());
    assert!(!first.queue_empty, "two streams remain queued");
    assert!(!queue_contains(&conn, oldest), "the oldest stream settled first");
    assert!(queue_contains(&conn, middle));
    assert!(queue_contains(&conn, newest));

    let second = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    assert_eq!(second.settled_streams, 1);
    assert!(!second.queue_empty, "the resume continues where the budget stopped");
    assert!(!queue_contains(&conn, middle));
    assert!(queue_contains(&conn, newest));

    let third = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    assert_eq!(third.settled_streams, 1);
    assert!(third.queue_empty);
    let drained = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    assert_eq!(drained.settled_streams, 0, "a drained queue is a no-op");
    assert!(drained.queue_empty);
}

#[test]
fn settle_budget_stops_at_the_candidate_and_byte_limits() {
    let conn = db();
    let big = [0x21_u8; 32];
    let small = [0x22_u8; 32];
    seed_synthetic_candidates(&conn, big, 3);
    seed_synthetic_candidates(&conn, small, 1);
    enqueue_refold(&conn, big, 1);
    enqueue_refold(&conn, small, 2);

    // Candidate limit: `big` exactly fills it, so `small` no longer fits the remainder.
    let report =
        settle_pending_content_refolds(&conn, &budget(u64::MAX, 3, u64::MAX, false), NOW).unwrap();
    assert_eq!(report.settled_streams, 1);
    assert_eq!(report.consumed_candidates, 3);
    assert_eq!(report.deferred_budget, 1);
    assert!(!queue_contains(&conn, big));
    assert!(queue_contains(&conn, small));

    // Byte limit on a fresh store: the second stream would exceed the remaining bytes.
    let conn = db();
    seed_synthetic_candidates(&conn, big, 2);
    seed_synthetic_candidates(&conn, small, 1);
    enqueue_refold(&conn, big, 1);
    enqueue_refold(&conn, small, 2);
    let byte_budget = budget(u64::MAX, u64::MAX, 2 * SYNTHETIC_ROW_BYTES, false);
    let report = settle_pending_content_refolds(&conn, &byte_budget, NOW).unwrap();
    assert_eq!(report.settled_streams, 1);
    assert_eq!(report.consumed_candidate_bytes, 2 * SYNTHETIC_ROW_BYTES);
    assert_eq!(report.deferred_budget, 1);
    assert!(queue_contains(&conn, small));
    // The resume settles the remainder with a fresh budget.
    let report = settle_pending_content_refolds(&conn, &byte_budget, NOW).unwrap();
    assert_eq!(report.settled_streams, 1);
    assert!(report.queue_empty);
}

#[test]
fn admission_charges_the_stats_aggregate_not_a_row_count() {
    let conn = db();
    let stream = [0x31_u8; 32];
    seed_synthetic_candidates(&conn, stream, 3);
    enqueue_refold(&conn, stream, 1);
    assert_eq!(
        stream_stats(&conn, stream),
        (3, (3 * SYNTHETIC_ROW_BYTES) as i64),
        "the stats triggers account counts and length(signed_bytes) + 32 per row",
    );

    // Lie about the aggregate: if admission ever COUNTed the stream's rows it would see 3 and
    // admit; reading the O(1) stats row it sees 10 and the eligibility filter excludes the
    // stream from discovery entirely.
    conn.execute("UPDATE content_stream_stats SET candidate_count = 10 WHERE stream_id = ?1", [
        stream.as_slice(),
    ])
    .unwrap();
    let report =
        settle_pending_content_refolds(&conn, &budget(u64::MAX, 5, u64::MAX, false), NOW).unwrap();
    assert_eq!(report.settled_streams, 0);
    assert_eq!(
        report.deferred_oversize, 0,
        "filtered from discovery by the stats row, not a COUNT(*): never listed, never counted",
    );
    assert!(!report.queue_empty, "the queue-empty probe still sees the queued stream");
    assert!(queue_contains(&conn, stream));

    // Restore the true aggregate and the same budget admits the stream.
    conn.execute("UPDATE content_stream_stats SET candidate_count = 3 WHERE stream_id = ?1", [
        stream.as_slice(),
    ])
    .unwrap();
    let report =
        settle_pending_content_refolds(&conn, &budget(u64::MAX, 5, u64::MAX, false), NOW).unwrap();
    assert_eq!(report.settled_streams, 1);
    assert_eq!(report.consumed_candidates, 3);
    assert!(report.queue_empty);
}

#[test]
fn admission_revalidates_cost_after_the_initial_listing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("revalidate-cost.sqlite");
    let conn = Connection::open(&path).unwrap();
    schema::apply(&conn, &crate::test_hooks()).unwrap();
    let concurrent = Connection::open(&path).unwrap();
    let stream = [0x32_u8; 32];
    seed_synthetic_candidates(&conn, stream, 1);
    enqueue_refold(&conn, stream, 1);

    let report = settle_pending_content_refolds_inner(
        &conn,
        &budget(u64::MAX, 2, u64::MAX, false),
        NOW,
        || seed_synthetic_candidates(&concurrent, stream, 2),
    )
    .unwrap();

    assert_eq!(report.settled_streams, 0);
    assert_eq!(report.consumed_candidates, 0, "a skipped stream was never admitted");
    assert_eq!(report.deferred_oversize, 1, "admission saw the current three-row cost");
    assert!(!report.queue_empty);
    assert!(queue_contains(&conn, stream));
    let status_rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_entry_status s
                 JOIN content_entries e ON e.entry_hash = s.entry_hash
                 WHERE e.stream_id = ?1",
            [stream.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status_rows, 0, "admission rollback made no fold writes");
}

#[test]
fn queue_empty_observes_a_different_stream_enqueued_during_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("current-remaining.sqlite");
    let conn = Connection::open(&path).unwrap();
    schema::apply(&conn, &crate::test_hooks()).unwrap();
    let concurrent = Connection::open(&path).unwrap();
    let listed = [0x33_u8; 32];
    let newly_enqueued = [0x34_u8; 32];
    seed_synthetic_candidates(&conn, listed, 1);
    enqueue_refold(&conn, listed, 1);

    let report =
        settle_pending_content_refolds_inner(&conn, &ContentRefoldBudget::unbounded(), NOW, || {
            seed_synthetic_candidates(&concurrent, newly_enqueued, 1);
            enqueue_refold(&concurrent, newly_enqueued, 2);
        })
        .unwrap();

    assert_eq!(report.settled_streams, 1);
    assert!(!report.queue_empty, "the final queue-empty probe sees the committed enqueue");
    assert!(!queue_contains(&conn, listed));
    assert!(queue_contains(&conn, newly_enqueued));
}

#[test]
fn a_queue_row_removed_after_listing_consumes_no_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vanished-queue-row.sqlite");
    let conn = Connection::open(&path).unwrap();
    schema::apply(&conn, &crate::test_hooks()).unwrap();
    let concurrent = Connection::open(&path).unwrap();
    let stream = [0x35_u8; 32];
    seed_synthetic_candidates(&conn, stream, 1);
    enqueue_refold(&conn, stream, 1);

    let report = settle_pending_content_refolds_inner(
        &conn,
        &budget(1, 1, SYNTHETIC_ROW_BYTES, false),
        NOW,
        || {
            concurrent
                .execute("DELETE FROM content_streams_pending_refold WHERE stream_id = ?1", [
                    stream.as_slice(),
                ])
                .unwrap();
        },
    )
    .unwrap();

    assert_eq!(report.settled_streams, 0);
    assert_eq!(report.consumed_candidates, 0);
    assert_eq!(report.consumed_candidate_bytes, 0);
    assert!(report.failures.is_empty());
    assert!(report.queue_empty);
}

#[test]
fn normal_mode_skips_an_oversize_stream_without_blocking_smaller_ones() {
    let conn = db();
    // The OLDEST stream is oversize: head-of-line blocking would stall the whole queue here.
    let oversize = [0x41_u8; 32];
    let small = [0x42_u8; 32];
    seed_synthetic_candidates(&conn, oversize, 5);
    seed_synthetic_candidates(&conn, small, 1);
    enqueue_refold(&conn, oversize, 1);
    enqueue_refold(&conn, small, 2);

    let report =
        settle_pending_content_refolds(&conn, &budget(u64::MAX, 2, u64::MAX, false), NOW).unwrap();
    assert_eq!(report.settled_streams, 1, "the smaller stream still settles");
    assert_eq!(report.consumed_candidates, 1);
    assert_eq!(report.deferred_budget, 0);
    assert_eq!(
        report.deferred_oversize, 0,
        "the oversize stream is filtered out of discovery, not listed and deferred",
    );
    assert!(!report.queue_empty);
    assert!(queue_contains(&conn, oversize), "the oversize stream stays queued");
    assert!(!queue_contains(&conn, small));

    // Normal mode NEVER starts it: repeated calls converge nothing further and never re-list
    // it.
    let stuck =
        settle_pending_content_refolds(&conn, &budget(u64::MAX, 2, u64::MAX, false), NOW).unwrap();
    assert_eq!(stuck.settled_streams, 0);
    assert_eq!(stuck.deferred_oversize, 0);
    assert!(!stuck.queue_empty);
}

#[test]
fn oversize_maintenance_mode_forces_one_oldest_oversize_stream_per_call() {
    let conn = db();
    let oldest_big = [0x51_u8; 32];
    let other_big = [0x52_u8; 32];
    let small = [0x53_u8; 32];
    seed_synthetic_candidates(&conn, oldest_big, 3);
    seed_synthetic_candidates(&conn, other_big, 3);
    seed_synthetic_candidates(&conn, small, 1);
    enqueue_refold(&conn, oldest_big, 1);
    enqueue_refold(&conn, other_big, 2);
    enqueue_refold(&conn, small, 3);

    let maintenance = budget(u64::MAX, 2, u64::MAX, true);
    // Call 1: normal discovery lists and settles the small stream, which EXHAUSTS the eligible
    // queue for this budget — so the oversize slot then fires for the OLDEST oversize stream,
    // an intentional exceedance visible in the charged counters. Exactly ONE oversize row is
    // admitted per call.
    let first = settle_pending_content_refolds(&conn, &maintenance, NOW).unwrap();
    assert_eq!(
        first.settled_streams, 2,
        "the eligible small stream settles, then the drained queue releases the oversize slot",
    );
    assert_eq!(first.consumed_candidates, 4, "1 small + the 3-candidate exceedance");
    assert!(first.consumed_candidates > maintenance.max_candidates);
    assert_eq!(
        first.deferred_oversize, 0,
        "the second oversize stream is filtered out of discovery, not listed and deferred"
    );
    assert!(!first.queue_empty);
    assert!(!queue_contains(&conn, small));
    assert!(!queue_contains(&conn, oldest_big), "the OLDEST oversize row goes first");
    assert!(queue_contains(&conn, other_big), "one oversize attempt per call");

    // Call 2: the scheduled maintenance loop converges — the second big stream claims this
    // call's oversize slot.
    let second = settle_pending_content_refolds(&conn, &maintenance, NOW).unwrap();
    assert_eq!(second.settled_streams, 1);
    assert_eq!(second.consumed_candidates, 3);
    assert!(!queue_contains(&conn, other_big));
    assert!(second.queue_empty);
}

#[test]
fn a_poisoned_stream_is_reported_and_retained_without_blocking_the_batch() {
    let conn = db();
    let poisoned = [0x61_u8; 32];
    let healthy = [0x62_u8; 32];
    seed_synthetic_candidates(&conn, poisoned, 1);
    seed_synthetic_candidates(&conn, healthy, 1);
    enqueue_refold(&conn, poisoned, 1);
    enqueue_refold(&conn, healthy, 2);
    // Fail only the poisoned stream's queue clear: its whole per-stream txn (refold writes
    // included) rolls back, so the queue row is deleted only after a COMMIT.
    let poison_hex: String = rag_rat_base::hash::hex_lower(&poisoned);
    conn.execute_batch(&format!(
        "CREATE TRIGGER poison_queue_clear
             BEFORE DELETE ON content_streams_pending_refold
             WHEN OLD.stream_id = X'{poison_hex}'
             BEGIN SELECT RAISE(ABORT, 'injected queue-clear failure'); END;"
    ))
    .unwrap();

    let report = settle_all(&conn);
    assert_eq!(report.settled_streams, 1, "the healthy stream still commits");
    assert_eq!(report.consumed_candidates, 2, "both admitted attempts are charged");
    assert_eq!(report.consumed_candidate_bytes, 2 * SYNTHETIC_ROW_BYTES);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].stream_id, StreamId::from_bytes(poisoned));
    assert!(report.failures[0].error.contains("injected queue-clear failure"));
    assert!(!report.queue_empty);
    assert!(queue_contains(&conn, poisoned), "the poisoned stream keeps its queue mark");
    assert!(!queue_contains(&conn, healthy));
    // The poisoned stream's txn rolled back: no declassify status write survived either.
    let status_rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_entry_status s
                 JOIN content_entries e ON e.entry_hash = s.entry_hash
                 WHERE e.stream_id = ?1",
            [poisoned.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status_rows, 0, "the failed stream's refold writes rolled back with its txn");
}

#[test]
fn poisoned_attempts_consume_every_budget_axis() {
    let conn = db();
    let first = [0x63_u8; 32];
    let second = [0x64_u8; 32];
    seed_synthetic_candidates(&conn, first, 2);
    seed_synthetic_candidates(&conn, second, 1);
    enqueue_refold(&conn, first, 1);
    enqueue_refold(&conn, second, 2);
    conn.execute_batch(
        "CREATE TRIGGER poison_every_queue_clear
             BEFORE DELETE ON content_streams_pending_refold
             BEGIN SELECT RAISE(ABORT, 'injected queue-clear failure'); END;",
    )
    .unwrap();

    let report =
        settle_pending_content_refolds(&conn, &budget(1, 2, 2 * SYNTHETIC_ROW_BYTES, false), NOW)
            .unwrap();

    assert_eq!(report.settled_streams, 0);
    assert_eq!(report.failures.len(), 1, "the stream budget permits one attempt");
    assert_eq!(report.failures[0].stream_id, StreamId::from_bytes(first));
    assert_eq!(report.consumed_candidates, 2, "failed admitted work consumes candidates");
    assert_eq!(report.consumed_candidate_bytes, 2 * SYNTHETIC_ROW_BYTES);
    assert_eq!(report.deferred_budget, 1);
    assert!(!report.queue_empty);
    assert!(queue_contains(&conn, first));
    assert!(queue_contains(&conn, second));
}

#[test]
fn oversize_mode_never_bypasses_the_stream_limit() {
    let conn = db();
    let oversize = [0x65_u8; 32];
    seed_synthetic_candidates(&conn, oversize, 3);
    enqueue_refold(&conn, oversize, 1);

    let zero_streams =
        settle_pending_content_refolds(&conn, &budget(0, 2, u64::MAX, true), NOW).unwrap();
    assert_eq!(zero_streams.settled_streams, 0);
    assert_eq!(zero_streams.consumed_candidates, 0);
    assert!(zero_streams.failures.is_empty());
    assert!(!zero_streams.queue_empty);
    assert!(queue_contains(&conn, oversize));

    let conn = db();
    let normal = [0x66_u8; 32];
    let oversize = [0x67_u8; 32];
    seed_synthetic_candidates(&conn, normal, 1);
    seed_synthetic_candidates(&conn, oversize, 3);
    enqueue_refold(&conn, normal, 1);
    enqueue_refold(&conn, oversize, 2);

    let one_stream =
        settle_pending_content_refolds(&conn, &budget(1, 2, u64::MAX, true), NOW).unwrap();
    assert_eq!(one_stream.settled_streams, 1);
    assert_eq!(one_stream.consumed_candidates, 1);
    assert_eq!(
        one_stream.deferred_oversize, 0,
        "the oversize stream is filtered out of discovery, not listed and deferred"
    );
    assert!(!one_stream.queue_empty);
    assert!(!queue_contains(&conn, normal));
    assert!(queue_contains(&conn, oversize), "the oversize attempt had no stream slot");
}

#[test]
fn settle_admits_oldest_first_then_breaks_ties_by_stream_id() {
    let conn = db();
    let older = [0xff_u8; 32];
    let tie_low = [0x71_u8; 32];
    let tie_mid = [0x72_u8; 32];
    let tie_high = [0x73_u8; 32];
    for stream in [older, tie_high, tie_low, tie_mid] {
        seed_synthetic_candidates(&conn, stream, 1);
    }
    // Enqueue out of order; `older` has the numerically largest stream id but the earliest
    // timestamp, so first_enqueued_at_ms dominates and stream_id only breaks ties.
    enqueue_refold(&conn, tie_high, 5);
    enqueue_refold(&conn, older, 1);
    enqueue_refold(&conn, tie_mid, 5);
    enqueue_refold(&conn, tie_low, 5);

    let one_stream = budget(1, u64::MAX, u64::MAX, false);
    let mut settled_order = Vec::new();
    for expected in [older, tie_low, tie_mid, tie_high] {
        let report = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
        assert_eq!(report.settled_streams, 1);
        assert!(!queue_contains(&conn, expected), "expected {expected:?} to settle next");
        settled_order.push(expected);
    }
    assert_eq!(settled_order, vec![older, tie_low, tie_mid, tie_high]);
    assert_eq!(pending_refold_count(&conn), 0);
}

// ---- #798 review: bounded progressive candidate discovery ----

fn reset_settle_work_counters() {
    SETTLE_LISTING_QUERIES.with(|c| c.set(0));
    SETTLE_ADMISSION_PROBES.with(|c| c.set(0));
    SETTLE_COMPLETION_PROBES.with(|c| c.set(0));
}

fn settle_work_counters() -> (usize, usize) {
    (
        SETTLE_LISTING_QUERIES.with(std::cell::Cell::get),
        SETTLE_ADMISSION_PROBES.with(std::cell::Cell::get),
    )
}

fn settle_completion_probes() -> usize {
    SETTLE_COMPLETION_PROBES.with(std::cell::Cell::get)
}

/// One deterministic stream id per backlog ordinal.
fn backlog_stream(ordinal: u64) -> [u8; 32] {
    let mut stream = [0x9d_u8; 32];
    stream[..8].copy_from_slice(&ordinal.to_be_bytes());
    stream
}

#[test]
fn settle_scan_work_is_bounded_by_the_budget_not_the_backlog() {
    let conn = db();
    const QUEUE: u64 = 16_384;
    {
        let tx = conn.unchecked_transaction().unwrap();
        for ordinal in 0..QUEUE {
            let stream = backlog_stream(ordinal);
            seed_synthetic_candidates(&tx, stream, 1);
            enqueue_refold(&tx, stream, i64::try_from(ordinal).unwrap() + 1);
        }
        tx.commit().unwrap();
    }
    let one_stream = budget(1, u64::MAX, u64::MAX, false);

    reset_settle_work_counters();
    let first = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    let (listings, probes) = settle_work_counters();
    assert_eq!(first.settled_streams, 1);
    assert!(
        !first.queue_empty,
        "the O(1) queue-empty EXISTS probe still observes the backlog is non-empty",
    );
    assert_eq!(
        pending_refold_count(&conn),
        i64::try_from(QUEUE - 1).unwrap(),
        "the whole 16k backlog minus the one settled stream is still queued",
    );
    assert_eq!(
        settle_completion_probes(),
        1,
        "completion is one O(1) EXISTS probe, never a COUNT(*) over the 16k backlog (#798)",
    );
    assert_eq!(listings, 1, "one bounded page listing, independent of the 16k backlog");
    assert_eq!(probes, 1, "only the admitted stream pays for an IMMEDIATE probe");
    assert_eq!(
        first.deferred_budget,
        settle_candidate_batch_size(&one_stream) - 1,
        "deferral counters classify only the discovered page, not the untouched backlog",
    );

    // A repeated call keeps the SAME per-call bound: draining stays linear in total, with no
    // per-call full scan.
    reset_settle_work_counters();
    let second = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    let (listings, probes) = settle_work_counters();
    assert_eq!(second.settled_streams, 1);
    assert!(!second.queue_empty);
    assert_eq!(pending_refold_count(&conn), i64::try_from(QUEUE - 2).unwrap());
    assert_eq!(
        settle_completion_probes(),
        1,
        "the resume also completes with a single O(1) EXISTS probe",
    );
    assert_eq!(listings, 1);
    assert_eq!(probes, 1);
}

#[test]
fn vanished_rows_trigger_progressive_paging_to_find_eligible_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paged-vanish.sqlite");
    let conn = Connection::open(&path).unwrap();
    schema::apply(&conn, &crate::test_hooks()).unwrap();
    let concurrent = Connection::open(&path).unwrap();
    const QUEUE: u64 = 100;
    const VANISHED: u64 = 12;
    {
        let tx = conn.unchecked_transaction().unwrap();
        for ordinal in 0..QUEUE {
            let stream = backlog_stream(ordinal);
            seed_synthetic_candidates(&tx, stream, 1);
            enqueue_refold(&tx, stream, i64::try_from(ordinal).unwrap() + 1);
        }
        tx.commit().unwrap();
    }
    let one_stream = budget(1, u64::MAX, u64::MAX, false);

    // Delete the 12 oldest queue rows AFTER the first page listing: page 1 probes nothing but
    // vanished rows, so discovery must page onward — without ever listing the rest of the
    // queue — to reach the first eligible stream.
    reset_settle_work_counters();
    let report = settle_pending_content_refolds_inner(&conn, &one_stream, NOW, || {
        for ordinal in 0..VANISHED {
            concurrent
                .execute("DELETE FROM content_streams_pending_refold WHERE stream_id = ?1", [
                    backlog_stream(ordinal).as_slice(),
                ])
                .unwrap();
        }
    })
    .unwrap();
    let (listings, probes) = settle_work_counters();

    assert_eq!(report.settled_streams, 1, "the first surviving stream settles");
    assert!(!queue_contains(&conn, backlog_stream(VANISHED)));
    assert!(!report.queue_empty, "the O(1) queue-empty probe still sees queued work");
    assert_eq!(
        pending_refold_count(&conn),
        i64::try_from(QUEUE - VANISHED - 1).unwrap(),
        "the whole-queue count minus the vanished rows and the one settled stream",
    );
    assert_eq!(listings, 2, "a full page of vanishes pages onward exactly once");
    assert_eq!(
        probes,
        settle_candidate_batch_size(&one_stream) + 1,
        "one full page of vanished probes plus the single admitted probe; no later queued row was \
         touched",
    );
}

#[test]
fn an_oversize_head_does_not_block_a_smaller_later_row_in_the_same_page() {
    let conn = db();
    let oversize = [0x81_u8; 32];
    let small = [0x82_u8; 32];
    seed_synthetic_candidates(&conn, oversize, 5);
    seed_synthetic_candidates(&conn, small, 1);
    enqueue_refold(&conn, oversize, 1);
    enqueue_refold(&conn, small, 2);
    // Backlog filler behind them proves discovery stops once the budget is spent.
    const FILLER: u64 = 50;
    {
        let tx = conn.unchecked_transaction().unwrap();
        for ordinal in 0..FILLER {
            let stream = backlog_stream(ordinal);
            seed_synthetic_candidates(&tx, stream, 1);
            enqueue_refold(&tx, stream, i64::try_from(ordinal).unwrap() + 3);
        }
        tx.commit().unwrap();
    }
    let one_stream = budget(1, 2, u64::MAX, false);

    reset_settle_work_counters();
    let report = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    let (listings, probes) = settle_work_counters();

    assert_eq!(report.settled_streams, 1, "the smaller later stream settles");
    assert_eq!(
        report.deferred_oversize, 0,
        "the oversize head is filtered out of discovery entirely",
    );
    assert_eq!(
        report.deferred_budget,
        settle_candidate_batch_size(&one_stream) - 1,
        "the discovered fillers defer on the spent stream slot",
    );
    assert!(queue_contains(&conn, oversize));
    assert!(!queue_contains(&conn, small));
    assert!(!report.queue_empty);
    assert_eq!(pending_refold_count(&conn), i64::try_from(FILLER + 1).unwrap());
    assert_eq!(listings, 1);
    assert_eq!(probes, 1, "the filtered-out oversize head costs no transaction");
}

#[test]
fn an_oversize_backlog_never_blocks_nor_relists_the_small_stream_behind_it() {
    let conn = db();
    // #798 Codex P1: eleven oversize streams ahead of one small stream, with a single stream
    // slot and caps every oversize stream exceeds. The old pager listed a full page of pure
    // oversize deferrals, stopped, and re-listed the SAME rows every call — the small stream
    // never settled. The eligibility-filtered listing skips them inside the query.
    const OVERSIZE: u64 = 11;
    for ordinal in 0..OVERSIZE {
        let stream = backlog_stream(ordinal);
        seed_synthetic_candidates(&conn, stream, 3);
        enqueue_refold(&conn, stream, i64::try_from(ordinal).unwrap() + 1);
    }
    let small = [0x91_u8; 32];
    seed_synthetic_candidates(&conn, small, 1);
    enqueue_refold(&conn, small, i64::try_from(OVERSIZE).unwrap() + 1);
    let one_stream = budget(1, 2, 2 * SYNTHETIC_ROW_BYTES, false);

    reset_settle_work_counters();
    let first = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    let (listings, probes) = settle_work_counters();
    assert_eq!(first.settled_streams, 1, "the small stream settles on the FIRST call");
    assert!(!queue_contains(&conn, small));
    assert!(!first.queue_empty);
    assert_eq!(pending_refold_count(&conn), i64::try_from(OVERSIZE).unwrap());
    assert_eq!(first.deferred_oversize, 0, "filtered rows are never discovered");
    assert_eq!(first.deferred_budget, 0);
    assert_eq!(listings, 1, "one eligibility-filtered page query");
    assert_eq!(probes, 1, "the oversize backlog costs zero admission probes");

    // A follow-up call over the pure-oversize queue discovers nothing and probes nothing.
    reset_settle_work_counters();
    let second = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    let (listings, probes) = settle_work_counters();
    assert_eq!(second.settled_streams, 0);
    assert!(!second.queue_empty);
    assert_eq!(pending_refold_count(&conn), i64::try_from(OVERSIZE).unwrap());
    assert_eq!(listings, 1, "one filtered listing, never a re-listed deferral page");
    assert_eq!(probes, 0);
}

#[test]
fn oversize_maintenance_admits_exactly_the_oldest_oversize_row() {
    let conn = db();
    let oldest = [0xa1_u8; 32];
    let newer = [0xa2_u8; 32];
    seed_synthetic_candidates(&conn, oldest, 3);
    seed_synthetic_candidates(&conn, newer, 4);
    enqueue_refold(&conn, oldest, 1);
    enqueue_refold(&conn, newer, 2);
    // One stream slot, so normal discovery can admit nothing; the oversize slot takes the
    // OLDEST row exceeding the caps via the single targeted query.
    let maintenance = budget(1, 2, u64::MAX, true);

    reset_settle_work_counters();
    let report = settle_pending_content_refolds(&conn, &maintenance, NOW).unwrap();
    let (listings, probes) = settle_work_counters();
    assert_eq!(report.settled_streams, 1);
    assert_eq!(report.consumed_candidates, 3, "the intentional exceedance is charged");
    assert!(!queue_contains(&conn, oldest));
    assert!(queue_contains(&conn, newer), "the second oversize row waits for a later call");
    assert!(!report.queue_empty);
    assert_eq!(listings, 2, "one filtered page plus the single targeted oversize query");
    assert_eq!(probes, 1, "only the admitted oversize row pays for a transaction");

    let report = settle_pending_content_refolds(&conn, &maintenance, NOW).unwrap();
    assert_eq!(report.settled_streams, 1);
    assert!(report.queue_empty);
}

#[test]
fn oversize_probe_is_skipped_while_the_budget_leaves_eligible_work_queued() {
    // #798 Codex review: the oversize `LIMIT 1` probe degenerates to a full queue scan when no
    // oversize row exists, so it must not run on a call that still has eligible work queued
    // behind it — otherwise every budgeted maintenance call is O(queue). The budget here is
    // exhausted by eligible rows with more still queued, so the oversize row must NOT settle.
    let conn = db();
    let oversize = [0xb2_u8; 32];
    seed_synthetic_candidates(&conn, oversize, 5);
    enqueue_refold(&conn, oversize, 99);
    for ordinal in 0..4_u64 {
        let stream = backlog_stream(ordinal);
        seed_synthetic_candidates(&conn, stream, 1);
        enqueue_refold(&conn, stream, i64::try_from(ordinal).unwrap() + 1);
    }
    // Two stream slots for four eligible rows: the budget runs out with eligible work queued.
    let maintenance = budget(2, 2, u64::MAX, true);

    let report = settle_pending_content_refolds(&conn, &maintenance, NOW).unwrap();
    assert_eq!(report.settled_streams, 2, "only the two budgeted eligible rows settle");
    assert!(
        queue_contains(&conn, oversize),
        "the oversize probe is skipped while the budget left eligible work queued",
    );
    assert_eq!(report.consumed_candidates, 2, "no intentional oversize exceedance was charged");
    assert!(!report.queue_empty);
}

#[test]
fn oversize_maintenance_converges_behind_a_persistently_failing_small_stream() {
    // #798 adversarial finding 1: gating the oversize probe on "the listing returned NOTHING"
    // starves maintenance forever whenever the queue keeps producing any listable row. A small
    // POISONED stream is listable on every call (it is eligible, it just always fails), so the
    // old gate never fired and the oversize row's acceptance/projection froze permanently.
    let conn = db();
    let poisoned = [0xe1_u8; 32];
    let oversize = [0xe2_u8; 32];
    seed_synthetic_candidates(&conn, poisoned, 1);
    seed_synthetic_candidates(&conn, oversize, 50);
    enqueue_refold(&conn, poisoned, 1);
    enqueue_refold(&conn, oversize, 2);
    let poison_hex: String = rag_rat_base::hash::hex_lower(&poisoned);
    conn.execute_batch(&format!(
        "CREATE TRIGGER poison_oversize_starvation
             BEFORE DELETE ON content_streams_pending_refold
             WHEN OLD.stream_id = X'{poison_hex}'
             BEGIN SELECT RAISE(ABORT, 'injected queue-clear failure'); END;"
    ))
    .unwrap();
    let maintenance = budget(4, 5, u64::MAX, true);

    let report = settle_pending_content_refolds(&conn, &maintenance, NOW).unwrap();
    assert_eq!(report.failures.len(), 1, "the poisoned row is attempted and fails");
    assert!(
        !queue_contains(&conn, oversize),
        "the oversize row converges even though a failing small row is listable every call",
    );
    assert!(queue_contains(&conn, poisoned), "the poisoned row is retained for retry");
}

#[test]
fn oversize_maintenance_converges_against_steady_eligible_arrivals() {
    // The same starvation without any poison: one fresh small stream arriving before each call
    // (ordinary remote ingest) kept the old `listed_any` gate true forever. Draining the
    // eligible work first is what lets maintenance reach the oversize row.
    let conn = db();
    let oversize = [0xe3_u8; 32];
    seed_synthetic_candidates(&conn, oversize, 50);
    enqueue_refold(&conn, oversize, 1);
    let maintenance = budget(4, 5, u64::MAX, true);

    let arrival = backlog_stream(0);
    seed_synthetic_candidates(&conn, arrival, 1);
    enqueue_refold(&conn, arrival, 2);

    let report = settle_pending_content_refolds(&conn, &maintenance, NOW).unwrap();
    assert!(
        !queue_contains(&conn, oversize),
        "a steady trickle of eligible rows no longer starves oversize maintenance",
    );
    assert!(report.settled_streams >= 2, "the arrival and the oversize row both settle");
}

#[test]
fn a_filtered_oversize_row_is_distinguishable_from_a_silent_no_op() {
    // #798 adversarial finding 6: an SQL-filtered oversize row reported all-zero counters with
    // `queue_empty == false`, identical to a poisoned stream or a listing that raced empty.
    // `discovered_rows == 0` is the signal that the remaining work is oversize.
    let conn = db();
    let oversize = [0xe4_u8; 32];
    seed_synthetic_candidates(&conn, oversize, 50);
    enqueue_refold(&conn, oversize, 1);

    let report =
        settle_pending_content_refolds(&conn, &budget(4, 5, u64::MAX, false), NOW).unwrap();
    assert_eq!(report.settled_streams, 0);
    assert_eq!(report.deferred_oversize, 0, "a listing-filtered row is never counted here");
    assert!(report.failures.is_empty());
    assert!(!report.queue_empty, "the oversize row is still queued");
    assert_eq!(
        report.discovered_rows, 0,
        "zero discovered rows with a non-empty queue means the remainder is oversize",
    );
}

#[test]
fn a_poisoned_stream_is_demoted_so_it_no_longer_head_of_line_blocks() {
    // #798 finding 3: a settle failure keeps the row QUEUED but DEMOTES it behind every
    // currently-queued row, so a persistently-poisoned oldest stream can no longer starve the
    // queue. With a one-stream budget the poisoned oldest row is admitted first each call; only
    // demote-on-failure lets the healthy stream behind it ever settle.
    let conn = db();
    let poisoned = [0xc1_u8; 32];
    let healthy = [0xc2_u8; 32];
    seed_synthetic_candidates(&conn, poisoned, 1);
    seed_synthetic_candidates(&conn, healthy, 1);
    enqueue_refold(&conn, poisoned, 1);
    enqueue_refold(&conn, healthy, 2);
    let poison_hex: String = rag_rat_base::hash::hex_lower(&poisoned);
    conn.execute_batch(&format!(
        "CREATE TRIGGER poison_demote_queue_clear
             BEFORE DELETE ON content_streams_pending_refold
             WHEN OLD.stream_id = X'{poison_hex}'
             BEGIN SELECT RAISE(ABORT, 'injected queue-clear failure'); END;"
    ))
    .unwrap();
    let one_stream = budget(1, u64::MAX, u64::MAX, false);

    // Call 1: the poisoned stream is oldest, admitted first, and fails; its one stream slot is
    // spent so the healthy stream is deferred. The failed row is demoted behind the healthy
    // row.
    let first = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    assert_eq!(first.settled_streams, 0, "the poisoned stream took the only stream slot");
    assert_eq!(first.failures.len(), 1);
    assert_eq!(first.failures[0].stream_id, StreamId::from_bytes(poisoned));
    assert!(queue_contains(&conn, poisoned), "a failed stream keeps its queue row");
    assert!(queue_contains(&conn, healthy));
    assert!(
        stream_enqueued_at(&conn, poisoned) > stream_enqueued_at(&conn, healthy),
        "the poisoned row is demoted behind the still-queued healthy row",
    );

    // Call 2: because the poisoned row was demoted, the healthy stream is now oldest and
    // settles — without demotion the poisoned oldest row would head-of-line block it
    // forever.
    let second = settle_pending_content_refolds(&conn, &one_stream, NOW).unwrap();
    assert_eq!(second.settled_streams, 1, "the healthy stream settles once the poison demoted");
    assert!(!queue_contains(&conn, healthy));
    assert!(queue_contains(&conn, poisoned), "the poisoned stream is still queued for retry");
    assert!(!second.queue_empty);
}

#[test]
fn a_failing_oldest_stream_is_folded_once_across_pages_and_demoted_after_the_loop() {
    // #798 adversarial finding F1: demoting a failed stream INSIDE the paging loop moves its
    // keyset position to `MAX(first_enqueued_at_ms) + 1` — AHEAD of the advancing cursor — so a
    // persistently-failing OLDEST stream re-lists and re-folds on every LATER page of the SAME
    // call (duplicate `failures`, redundant folds, premature budget consumption starving
    // healthy later-page rows). Applying the demotion once AFTER the loop keeps the failed row
    // BEHIND the cursor: it is attempted exactly once, the healthy later-page rows all settle,
    // and the row is bumped a single time. This is only latent because production callers use
    // max_streams=1 (never page); the unbounded budget here pages (batch caps at MAX_BATCH).
    let conn = db();
    let poisoned = [0xd1_u8; 32];
    // The unbounded batch size caps at MAX_BATCH (512), so > 512 queued rows force a second
    // page. The poisoned oldest sits at the head of page 1; every healthy row is newer.
    let unbounded = ContentRefoldBudget::unbounded();
    let batch = settle_candidate_batch_size(&unbounded);
    let healthy_count = batch + 88; // 600 for the 512 cap: spans two pages, no third empty page
    seed_synthetic_candidates(&conn, poisoned, 1);
    enqueue_refold(&conn, poisoned, 1); // strictly the oldest
    let mut healthy_streams = Vec::with_capacity(healthy_count);
    for ordinal in 0..healthy_count as u64 {
        let stream = backlog_stream(ordinal);
        seed_synthetic_candidates(&conn, stream, 1);
        // All newer than the poisoned oldest (first_enqueued_at_ms >= 2), ascending by ordinal.
        enqueue_refold(&conn, stream, i64::try_from(ordinal).unwrap() + 2);
        healthy_streams.push(stream);
    }
    // Fail only the poisoned stream's queue clear: its per-stream txn rolls back, the row is
    // retained, and it is collected for a single post-loop demotion.
    let poison_hex: String = rag_rat_base::hash::hex_lower(&poisoned);
    conn.execute_batch(&format!(
        "CREATE TRIGGER poison_paging_queue_clear
             BEFORE DELETE ON content_streams_pending_refold
             WHEN OLD.stream_id = X'{poison_hex}'
             BEGIN SELECT RAISE(ABORT, 'injected queue-clear failure'); END;"
    ))
    .unwrap();
    assert!(
        pending_refold_count(&conn) > i64::try_from(batch).unwrap(),
        "the queue must exceed one page so the settle actually pages",
    );

    reset_settle_work_counters();
    let report = settle_pending_content_refolds(&conn, &unbounded, NOW).unwrap();
    let (listings, probes) = settle_work_counters();

    // Fairness: every healthy row settles despite the failing oldest row on page 1.
    assert_eq!(report.settled_streams, healthy_count, "all healthy rows settle across pages");
    for stream in &healthy_streams {
        assert!(!queue_contains(&conn, *stream), "no healthy row is starved by the poison");
    }
    // The poisoned stream is attempted, folded, and reported EXACTLY ONCE — not once per page.
    assert_eq!(report.failures.len(), 1, "the failing oldest row fails exactly once per call");
    assert_eq!(report.failures[0].stream_id, StreamId::from_bytes(poisoned));
    assert!(queue_contains(&conn, poisoned), "a failed row keeps its queue mark for retry");
    assert!(!report.queue_empty, "a failed row still leaves the queue non-empty");
    // Bounded re-fold: one admission probe per healthy row plus ONE for the poisoned stream.
    // With the pre-fix in-loop demotion the poisoned row re-lists on the second page and this
    // would be `healthy_count + 2`.
    assert_eq!(
        probes,
        healthy_count + 1,
        "the poisoned stream is folded once, not re-folded on every later page",
    );
    assert_eq!(listings, 2, "the queue spans exactly two bounded pages");
    // Demoted EXACTLY once: the post-loop pass runs after every healthy row has been cleared,
    // so the queue holds only the poisoned row and `MAX + 1` bumps it from 1 to 2 a single
    // time. The pre-fix mid-loop bump would have moved it far past the whole backlog.
    assert_eq!(
        stream_enqueued_at(&conn, poisoned),
        2,
        "the failed row is demoted (bumped) exactly once, after the paging loop",
    );
}

/// Differential cut-binding parity harness (I11). The account control fold and the `/3` content
/// fold share the "deliberately identical" cut substrate (see `candidate.rs`) — a withheld
/// watermark condemns beyond-cut from seq alone and parks only the under-cut prefix, and a
/// misbound watermark neither condemns nor pins. The content fold once diverged (a withheld
/// watermark parked a beyond-cut entry the account fold condemns). This runs the SAME cut
/// scenarios — watermark held / withheld / misbound, against an entry beyond and under the cut
/// — through BOTH real folds and asserts the target entry reaches an IDENTICAL verdict, so
/// any future re-divergence of the whole class (not just this instance) fails here.
mod cut_binding_parity {
    use std::collections::HashMap;

    use super::*;
    use crate::account::cut::Cut;
    use crate::account::envelope::{
        AccountEntryHeader, VerifiedAccountEntry, sign_account_entry, verify_account_signed,
    };
    use crate::account::fold::{
        AccountAuthHistory, CondemnedReason, Outcome, ParkReason, fold_account,
    };
    use crate::account::id::account_id_from_genesis_payload;
    use crate::account::ops::{self as account_ops, AccountOp, entry_type};
    use crate::account::test_support::Dev;
    use crate::account::{AccountId, DeviceRole};
    use crate::device::DeviceSecret;

    /// The normalized verdict both folds must agree on — the effect the cut has on ONE target
    /// entry, projected out of each fold's own taxonomy.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CutParity {
        CondemnedBeyondCut,
        ParkedUnknownCutTarget,
        /// The cut did not bite this entry: `accepted` (content) / `effective` (account).
        Survives,
    }

    #[derive(Debug, Clone, Copy)]
    enum Watermark {
        Held,
        Withheld,
        Misbound,
    }

    #[derive(Debug, Clone, Copy)]
    enum Target {
        BeyondCut,
        UnderCut,
    }

    /// A minimal account-log authoring fixture: it threads each device's `(seq, prev)` chain
    /// and signs real entries so `fold_account` runs over verified input.
    struct AccountLog {
        account_id: AccountId,
        genesis_hash: AccountEntryHash,
        chains: HashMap<[u8; 32], (u64, Option<AccountEntryHash>)>,
        entries: Vec<VerifiedAccountEntry>,
    }

    impl AccountLog {
        fn genesis(founder: &Dev) -> Self {
            let op = AccountOp::AccountGenesis {
                ed25519_pubkey: founder.ed,
                x25519_pubkey: founder.x,
                nonce16: [0u8; 16],
                created_at_ms: 1_700_000_000_000,
                label: None,
            };
            let payload = account_ops::encode(&op).unwrap();
            let account_id = account_id_from_genesis_payload(&payload);
            let header = AccountEntryHeader {
                account_id,
                log_id: 0,
                device_fingerprint: founder.fp,
                seq: 0,
                prev_hash: None,
                parent_ref: None,
                entry_type: entry_type::ACCOUNT_GENESIS,
                op_version: 1,
                crypto_suite: 0,
                auth_len: 0,
                key_id: None,
                authority_ref: None,
            };
            let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
            let verified =
                verify_account_signed(&signed.signed_bytes, &founder.secret.public()).unwrap();
            let genesis_hash = verified.entry_hash;
            let mut chains = HashMap::new();
            chains.insert(founder.fp.to_bytes(), (1, Some(genesis_hash)));
            AccountLog { account_id, genesis_hash, chains, entries: vec![verified] }
        }

        fn author(
            &mut self,
            author: &Dev,
            authority_ref: Option<OwnerId>,
            op: &AccountOp,
        ) -> [u8; 32] {
            let payload = account_ops::encode(op).unwrap();
            let (seq, prev) = self.chains.get(&author.fp.to_bytes()).copied().unwrap_or((0, None));
            let header = AccountEntryHeader {
                account_id: self.account_id,
                log_id: 0,
                device_fingerprint: author.fp,
                seq,
                prev_hash: prev,
                parent_ref: Some(self.genesis_hash),
                entry_type: account_ops::entry_type_of(op),
                op_version: 1,
                auth_len: 1,
                crypto_suite: 0,
                key_id: None,
                authority_ref,
            };
            let signed = sign_account_entry(&author.secret, &header, &payload).unwrap();
            let verified =
                verify_account_signed(&signed.signed_bytes, &author.secret.public()).unwrap();
            let hash = verified.entry_hash;
            self.chains.insert(author.fp.to_bytes(), (seq + 1, Some(hash)));
            self.entries.push(verified);
            hash.into()
        }
    }

    fn member_add(dev: &Dev) -> AccountOp {
        AccountOp::DeviceAdd {
            device_fingerprint: dev.fp,
            ed25519_pubkey: dev.ed,
            x25519_pubkey: dev.x,
            role: DeviceRole::Member,
            label: None,
        }
    }

    /// Drive the account control fold: founder F (owner) enrolls owner B; B authors a dense
    /// control chain b0→b1→b2; F removes B with a cut bounding B's chain at seq 1 (watermark
    /// b1). B's own chain is the cut coordinate — the account analog of the content stream
    /// chain.
    fn account_verdict(target: Target, watermark: Watermark) -> CutParity {
        let founder = Dev::new(0xF1);
        let b = Dev::new(0xB1);
        let mut log = AccountLog::genesis(&founder);
        let add_b = log.author(&founder, Some(log.genesis_hash.into()), &AccountOp::DeviceAdd {
            device_fingerprint: b.fp,
            ed25519_pubkey: b.ed,
            x25519_pubkey: b.x,
            role: DeviceRole::Owner,
            label: None,
        });
        let b0 = log.author(&b, Some(OwnerId::from_bytes(add_b)), &member_add(&Dev::new(0xD1)));
        let b1 = log.author(&b, Some(OwnerId::from_bytes(add_b)), &member_add(&Dev::new(0xE1)));
        let b2 = log.author(&b, Some(OwnerId::from_bytes(add_b)), &member_add(&Dev::new(0x71)));
        let control_cut = match watermark {
            // A misbound watermark names the WRONG seq on B's chain (b0 is seq 0, the cut
            // claims seq 1): the §11.3 guard rejects the whole remove, so B
            // (and b0/b2) survives.
            Watermark::Misbound => Cut::At { seq: 1, hash: AccountEntryHash::from_bytes(b0) },
            Watermark::Held | Watermark::Withheld =>
                Cut::At { seq: 1, hash: AccountEntryHash::from_bytes(b1) },
        };
        log.author(&founder, Some(log.genesis_hash.into()), &AccountOp::DeviceRemove {
            device_fingerprint: b.fp,
            control_cut,
            secrets_cut: Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        });
        // A withheld watermark models b1 not yet synced — fold every entry EXCEPT b1.
        let history: AccountAuthHistory = match watermark {
            Watermark::Withheld => {
                let held: Vec<VerifiedAccountEntry> = log
                    .entries
                    .iter()
                    .filter(|e| e.entry_hash != AccountEntryHash::from_bytes(b1))
                    .cloned()
                    .collect();
                fold_account(&held)
            },
            _ => fold_account(&log.entries),
        };
        let target_hash = match target {
            Target::BeyondCut => b2,
            Target::UnderCut => b0,
        };
        match history.outcome(&target_hash.into()) {
            Some(Outcome::Condemned(CondemnedReason::BeyondCut)) => CutParity::CondemnedBeyondCut,
            Some(Outcome::Parked(ParkReason::UnknownCutTarget)) =>
                CutParity::ParkedUnknownCutTarget,
            Some(Outcome::Effective { .. }) => CutParity::Survives,
            other => panic!("account fold: unexpected {target:?}/{watermark:?} outcome {other:?}"),
        }
    }

    /// Drive the `/3` content fold over the analogous scenario: a dense stream chain s0→s1→s2
    /// on one coordinate, a roster content cut bounding it at seq 1 (watermark s1).
    fn content_verdict(target: Target, watermark: Watermark) -> CutParity {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xC0; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis.into(), owner, &secret, "owner");
        let s0 = authored(&secret, owner, genesis.into(), ContentSpec::default());
        let s1 = authored(&secret, owner, genesis.into(), ContentSpec {
            seq: 1,
            previous: Some(s0.entry_hash),
            ..ContentSpec::default()
        });
        let s2 = authored(&secret, owner, genesis.into(), ContentSpec {
            seq: 2,
            previous: Some(s1.entry_hash),
            ..ContentSpec::default()
        });
        let cut_watermark = match watermark {
            // Misbound: names s0 (seq 0) as the seq-1 watermark — a same-coordinate seq
            // mismatch.
            Watermark::Misbound => s0.entry_hash,
            Watermark::Held | Watermark::Withheld => s1.entry_hash,
        };
        seed_roster_content_cut(&conn, genesis.into(), owner, 1, cut_watermark);
        content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
        // A withheld watermark models s1 not yet ingested — the content analog of dropping b1.
        if !matches!(watermark, Watermark::Withheld) {
            content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
        }
        content_ingest(&conn, &s2.signed_bytes, 3).unwrap();
        settle_all(&conn);
        let target_hash = match target {
            Target::BeyondCut => s2.entry_hash,
            Target::UnderCut => s0.entry_hash,
        };
        match verdict(&conn, &target_hash).0.as_str() {
            "condemned{beyond_cut}" => CutParity::CondemnedBeyondCut,
            "parked{unknown_cut_target}" => CutParity::ParkedUnknownCutTarget,
            "accepted" => CutParity::Survives,
            other => panic!("content fold: unexpected {target:?}/{watermark:?} status {other}"),
        }
    }

    #[test]
    fn account_and_content_folds_agree_on_every_cut_binding() {
        for (target, watermark, expected) in [
            (Target::BeyondCut, Watermark::Held, CutParity::CondemnedBeyondCut),
            // The exact divergence this fix closes: a withheld watermark must still condemn.
            (Target::BeyondCut, Watermark::Withheld, CutParity::CondemnedBeyondCut),
            (Target::BeyondCut, Watermark::Misbound, CutParity::Survives),
            (Target::UnderCut, Watermark::Held, CutParity::Survives),
            (Target::UnderCut, Watermark::Withheld, CutParity::ParkedUnknownCutTarget),
            (Target::UnderCut, Watermark::Misbound, CutParity::Survives),
        ] {
            let account = account_verdict(target, watermark);
            let content = content_verdict(target, watermark);
            assert_eq!(
                account, content,
                "folds diverged for {target:?}/{watermark:?}: account={account:?} \
                 content={content:?}",
            );
            assert_eq!(
                account, expected,
                "the account fold verdict for {target:?}/{watermark:?} is not the intended one",
            );
        }
    }
}

/// TRIPWIRE (#798 adversarial finding 3): the V082 `content_stream_stats` triggers cannot see
/// `INSERT OR REPLACE`'s implicit row deletion — SQLite skips `AFTER DELETE` triggers for it
/// unless `PRAGMA recursive_triggers` is on, which this store never sets. A `REPLACE` into
/// `content_entries` would therefore fire the insert trigger alone and drift the aggregate
/// upward permanently, making the stream invisible to normal-mode settle forever. Upsert
/// (`ON CONFLICT ... DO UPDATE`) is the same hazard when it rewrites an accounted column.
///
/// The accounting invariant is enforced HERE rather than by hoping every future caller
/// remembers it. If this fires: use `INSERT OR IGNORE` (correctly inert on the ignore branch)
/// or a plain `INSERT`/`DELETE` pair, or make the aggregate maintenance explicit.
#[test]
fn no_writer_replaces_or_upserts_content_entries_behind_the_stats_triggers() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/<crate>")
        .join("crates");
    let mut sources = Vec::new();
    let mut stack = vec![workspace];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("readable crate dir") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                sources.push(path);
            }
        }
    }
    assert!(sources.len() > 100, "the source sweep found suspiciously few files to scan");

    let mut offenders = Vec::new();
    for path in sources {
        let text = std::fs::read_to_string(&path).expect("readable rust source");
        // SQL is written across several lines, so compare on a whitespace-normalized copy.
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        // Needles are assembled at runtime: spelled literally they would appear in this
        // file's own source and the tripwire would flag itself.
        let table = "content_entries";
        let replace_into = format!("{} INTO {table}", "REPLACE");
        let replaces = flat.contains(&replace_into);
        // An upsert is only a hazard when it can rewrite an accounted column, but the tripwire
        // is deliberately blunt: any `DO UPDATE` on this table wants a human decision.
        let insert_into = format!("{} INTO {table}", "INSERT");
        let upserts = flat.split(insert_into.as_str()).skip(1).any(|tail| {
            let statement = tail.split(';').next().unwrap_or(tail);
            statement.contains("ON CONFLICT") && statement.contains("DO UPDATE")
        });
        if replaces || upserts {
            offenders.push(path.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "content_entries must not be written with REPLACE/upsert semantics — the V082 stats \
         triggers cannot account for it (see the migration doc): {offenders:?}",
    );
}

/// A pin landing on an owner must not fail an UNRELATED contributor's later settlement (#1399).
///
/// The pin arrives AFTER the contributor's entry is accepted, because that is the only reachable
/// shape: ingest onto an already-pinned owner's stream is refused up front by
/// `require_supported_stream_control`. A later settle discharges
/// `refold_and_project_stream_in_tx`, whose last step refreshes enrollment reservations for the
/// stream's owner through an ungated ownership read. Under `ControlV2` that row survives — the
/// projection is rebuilt, not emptied — so the pinned owner's top-up runs inside the
/// contributor's fold, and the guard in `top_up_account_candidate_reservations_in_tx` is what
/// keeps it from touching the reservation.
///
/// The pin row is seeded directly: `install_pin` is private to `control_policy`, and V130's
/// triggers forbid only UPDATE/DELETE. That yields a `ControlV2` policy with no verified
/// checkpoint behind it — enough here, because the top-up keys on the policy and on the
/// ownership row and this seed reproduces both, but it exercises the guard, not a full pinned
/// fold.
#[test]
fn a_pin_on_the_owner_does_not_fail_an_unrelated_contributors_settlement() {
    let conn = db();
    // A persisted device identity, so the top-up can run to completion when the guard is NOT
    // there. Without one it dies on the enrollment-recovery preflight, and the guard would look
    // proven by an error that has nothing to do with the pin.
    crate::identity::local_device(&conn, NOW).unwrap();
    let owner_secret = DeviceSecret::from_seed(&[0xc1; 32]);
    let author_secret = DeviceSecret::from_seed(&[0xc2; 32]);
    let owner = roster(&conn, &owner_secret).0;
    let (author, author_genesis) = roster(&conn, &author_secret);
    let grant_id = [0x6a; 32];
    seed_ownership(&conn, owner);
    seed_roster_fact(&conn, author_genesis.into(), author, &author_secret, "member");
    seed_grant(&conn, GrantId::from_bytes(grant_id), owner, author, "writer");

    let entry = authored(&author_secret, author, author_genesis.into(), ContentSpec {
        grant_id: Some(GrantId::from_bytes(grant_id)),
        ..ContentSpec::default()
    });
    assert_eq!(verdict_after_ingest(&conn, &entry), ("accepted".into(), 1));

    // An outstanding invite on the owner: without one the top-up short-circuits on
    // `any_outstanding` and never reaches the pinned path at all.
    {
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        crate::upsert_account_candidate_reservation_in_tx(
            &tx,
            owner,
            [0x9c; 32],
            4,
            4096,
            2,
            rag_rat_base::time::now_ms() + 3_600_000,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    conn.execute(
        "INSERT INTO account_control_pins(account_id, checkpoint_digest, required_version, \
         certificate) VALUES (?1, ?2, 2, ?3)",
        params![owner.to_bytes().as_slice(), [0x7c_u8; 32].as_slice(), [0u8; 8].as_slice()],
    )
    .unwrap();

    // The contributor's stream settles again with the pin now in place. Two things are asserted,
    // one per half of the fix: the settle COMPLETES, because the reads it takes admit a folded
    // pin; and the reservation below is UNTOUCHED, because the guard skipped the top-up.
    run_account_trigger_owning(&conn, author, &[StreamId::from_bytes(STREAM).to_bytes()]);

    // The pinned owner's reservation is untouched — the guard's own contract, distinct from the
    // contributor's fold merely not erroring.
    let targets: i64 = conn
        .query_row(
            "SELECT reserved_targets FROM account_candidate_reservations WHERE account_id = ?1",
            [owner.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(targets, 2, "a pinned account's invite reservation is never topped up");
}
