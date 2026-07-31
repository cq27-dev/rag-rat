use rag_rat_db::schema;
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::*;
use crate::account::authoring;
use crate::account::bootstrap::{self, LocalAccountRef};
use crate::account::envelope::{AccountEntryHeader, VerifiedAccountEntry, sign_account_entry};
use crate::account::fold::CONTROL_LOG;
use crate::account::keywrap::unwrap_content_key;
use crate::account::ops::{AccountOp, DeviceRole};
use crate::device::{DeviceSecret, DeviceX25519Secret};
use crate::identity::local_device;

const NOW: i64 = 1_700_000_000_000;
const TAG: [u8; 32] = [0xa7; 32];
const OTHER_TAG: [u8; 32] = [0xb8; 32];
const NODE_ID: [u8; 32] = [0x4d; 32];

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::test_hooks()).unwrap();
    conn
}

/// Author a control op as the founder and refold, so the roster reflects it.
///
/// Mirrors the fixture in `secrets::author`'s tests. Duplicated rather than shared because those
/// helpers are private to that module; if a third module needs them they should move to a shared
/// test-support module rather than be copied again.
fn author_control_op(conn: &Connection, account: super::super::AccountId, op: &AccountOp) {
    use crate::account::{ops as control_ops, storage};

    let founder = local_device(conn, NOW).unwrap();
    let LocalAccountRef { genesis_hash, .. } = bootstrap::local_account_ref(conn).unwrap().unwrap();
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
    let tail = authoring::account_chain_tail(&tx, account, founder.fingerprint(), CONTROL_LOG)
        .unwrap()
        .expect("control chain is non-empty post-genesis");
    let header = AccountEntryHeader {
        account_id: account,
        log_id: CONTROL_LOG,
        device_fingerprint: founder.fingerprint(),
        seq: tail.0 + 1,
        prev_hash: Some(tail.1),
        parent_ref: Some(genesis_hash),
        entry_type: control_ops::entry_type_of(op),
        op_version: 1,
        crypto_suite: 0,
        auth_len: storage::account_effective_count(&tx, account).unwrap(),
        key_id: None,
        authority_ref: Some(genesis_hash),
    };
    let payload = control_ops::encode(op).unwrap();
    let signed = sign_account_entry(founder.secret(), &header, &payload).unwrap();
    let verified = VerifiedAccountEntry {
        header: signed.header,
        payload: signed.payload,
        entry_hash: signed.entry_hash,
    };
    storage::insert_candidate(&tx, &verified, &signed.signed_bytes, NOW).unwrap();
    storage::refold_in_tx(&tx, account, NOW).unwrap();
    tx.commit().unwrap();
}

/// Enrol a second device carrying `member_x`, and return its fingerprint.
fn add_member(
    conn: &Connection,
    account: super::super::AccountId,
    member_x: &DeviceX25519Secret,
) -> crate::op::DeviceFingerprint {
    let member_pub = DeviceSecret::from_seed(&[0x2c; 32]).public();
    let member_fp = member_pub.fingerprint();
    author_control_op(conn, account, &AccountOp::DeviceAdd {
        device_fingerprint: member_fp,
        ed25519_pubkey: member_pub.to_bytes(),
        x25519_pubkey: member_x.public().to_bytes(),
        role: DeviceRole::Member,
        label: None,
    });
    member_fp
}

/// Drop a device from the effective roster by closing its roster row — what the reader this design
/// depends on actually looks at (`closed_at IS NULL`), without building a full `DeviceRemove` and
/// its cuts.
fn close_roster_row(
    conn: &Connection,
    account: super::super::AccountId,
    fp: crate::op::DeviceFingerprint,
) {
    conn.execute(
        "UPDATE account_roster_history SET closed_at = ?3
         WHERE account_id = ?1 AND device_fingerprint = ?2",
        rusqlite::params![account.to_bytes().as_slice(), fp.to_bytes().as_slice(), 99i64],
    )
    .unwrap();
}

/// Reconstruct the wrap context a recipient would use, so a test can open a specific wrap with a
/// key the local store does not hold.
fn ctx_for(
    account: super::super::AccountId,
    tag: &[u8; 32],
    recipient: &DeviceX25519Secret,
) -> WrapContext {
    wrap_context(account, tag, &recipient.public().to_bytes())
}

fn wraps_of(envelope: &[u8]) -> Vec<SealedKeyWrap> {
    parse_wraps(envelope).expect("a sealed envelope parses")
}

// ---------------------------------------------------------------- round trip

/// The sealing device is among the recipients, so it opens its own announcement. A publisher is
/// also a device — it fetches on its own pass — and a host that could not read what it published
/// would be a hole nobody would expect.
#[test]
fn a_sealed_announcement_opens_on_the_sealing_device() {
    let conn = db();
    bootstrap::local_account(&conn, NOW).unwrap();

    let sealed = seal_discovery_announcement(&conn, &TAG, &NODE_ID).unwrap().unwrap();
    assert_eq!(sealed.recipients, 1, "a lone founder is the whole roster");

    let opened = open_discovery_announcement(&conn, &TAG, &sealed.bytes).unwrap();
    assert_eq!(opened, Some(NODE_ID));
}

/// Every roster-effective device is a recipient, not just the publisher.
#[test]
fn every_roster_effective_device_can_open_the_announcement() {
    let conn = db();
    let account = bootstrap::local_account(&conn, NOW).unwrap();
    let member_x = DeviceX25519Secret::from_seed(&[0x5c; 32]);
    add_member(&conn, account, &member_x);

    let sealed = seal_discovery_announcement(&conn, &TAG, &NODE_ID).unwrap().unwrap();
    assert_eq!(sealed.recipients, 2, "founder + member");

    // Open as the member, whose key this store does not hold, by reconstructing its context.
    let ctx = ctx_for(account, &TAG, &member_x);
    let opened = wraps_of(&sealed.bytes)
        .iter()
        .find_map(|wrap| unwrap_content_key(wrap, &member_x, &ctx).ok())
        .expect("the member has a wrap it can open");
    assert_eq!(opened.as_slice(), NODE_ID.as_slice());
}

/// Sealing is NOT deterministic: each wrap carries a fresh ephemeral. Anything that tries to detect
/// a roster change by comparing envelope bytes therefore sees a change every time.
#[test]
fn two_seals_of_the_same_node_to_the_same_roster_differ() {
    let conn = db();
    bootstrap::local_account(&conn, NOW).unwrap();

    let first = seal_discovery_announcement(&conn, &TAG, &NODE_ID).unwrap().unwrap();
    let second = seal_discovery_announcement(&conn, &TAG, &NODE_ID).unwrap().unwrap();
    assert_ne!(first.bytes, second.bytes, "a fresh ephemeral per wrap makes every seal differ");
}

/// The stamp is what a caller compares to decide whether a re-seal is owed, since envelope bytes
/// cannot serve that purpose. It must be stable across reads and move when the roster does.
#[test]
fn the_roster_stamp_is_stable_until_the_roster_moves() {
    let conn = db();
    let account = bootstrap::local_account(&conn, NOW).unwrap();

    let lone = roster_stamp(&conn).unwrap().expect("an account has a stamp");
    assert_eq!(lone, roster_stamp(&conn).unwrap().unwrap(), "stable across reads");
    assert_eq!(
        lone,
        seal_discovery_announcement(&conn, &TAG, &NODE_ID).unwrap().unwrap().roster_stamp,
        "and matches what a seal reports, so the two cannot drift"
    );

    let member_x = DeviceX25519Secret::from_seed(&[0x5c; 32]);
    let member_fp = add_member(&conn, account, &member_x);
    let with_member = roster_stamp(&conn).unwrap().unwrap();
    assert_ne!(lone, with_member, "enrolling a device moves the stamp");

    close_roster_row(&conn, account, member_fp);
    assert_eq!(roster_stamp(&conn).unwrap().unwrap(), lone, "removing it moves it back");
}

#[test]
fn a_store_with_no_account_has_no_roster_stamp() {
    assert!(roster_stamp(&db()).unwrap().is_none());
}

// ---------------------------------------------------------------- revocation

/// The property this whole design exists for: a device off the roster at seal time gets no wrap.
#[test]
fn a_device_removed_before_sealing_gets_no_wrap() {
    let conn = db();
    let account = bootstrap::local_account(&conn, NOW).unwrap();
    let member_x = DeviceX25519Secret::from_seed(&[0x5c; 32]);
    let member_fp = add_member(&conn, account, &member_x);
    close_roster_row(&conn, account, member_fp);

    let sealed = seal_discovery_announcement(&conn, &TAG, &NODE_ID).unwrap().unwrap();
    assert_eq!(sealed.recipients, 1, "the removed device is not sealed to");

    let ctx = ctx_for(account, &TAG, &member_x);
    assert!(
        wraps_of(&sealed.bytes)
            .iter()
            .all(|wrap| unwrap_content_key(wrap, &member_x, &ctx).is_err()),
        "a removed device must not open anything sealed after its removal"
    );
}

/// The honest limit, pinned so nobody claims instant revocation: an announcement sealed WHILE a
/// device was effective stays readable by it. Revocation governs what is sealed next; the old
/// announcement stops mattering when it expires at the service.
#[test]
fn a_device_removed_after_sealing_still_opens_that_announcement() {
    let conn = db();
    let account = bootstrap::local_account(&conn, NOW).unwrap();
    let member_x = DeviceX25519Secret::from_seed(&[0x5c; 32]);
    let member_fp = add_member(&conn, account, &member_x);

    let sealed = seal_discovery_announcement(&conn, &TAG, &NODE_ID).unwrap().unwrap();
    close_roster_row(&conn, account, member_fp);

    let ctx = ctx_for(account, &TAG, &member_x);
    let opened = wraps_of(&sealed.bytes)
        .iter()
        .find_map(|wrap| unwrap_content_key(wrap, &member_x, &ctx).ok());
    assert!(opened.is_some(), "already-published announcements do not become unreadable");
}

// ---------------------------------------------------------------- AAD binding

/// A wrap is bound to the tag it was published under, so it cannot be replayed beneath another —
/// including by a legitimate recipient.
#[test]
fn a_wrap_does_not_open_under_another_tag() {
    let conn = db();
    bootstrap::local_account(&conn, NOW).unwrap();
    let sealed = seal_discovery_announcement(&conn, &TAG, &NODE_ID).unwrap().unwrap();

    assert_eq!(open_discovery_announcement(&conn, &TAG, &sealed.bytes).unwrap(), Some(NODE_ID));
    assert_eq!(
        open_discovery_announcement(&conn, &OTHER_TAG, &sealed.bytes).unwrap(),
        None,
        "the tag is AAD; a different tag must not open the wrap"
    );
}

/// The remaining context fields are pinned the same way — by showing an opener that differs in one
/// of them fails. A byte golden would pin the layout but not that these values are the ones fed in.
#[test]
fn the_wrap_context_carries_account_tag_epoch_and_recipient() {
    let conn = db();
    let account = bootstrap::local_account(&conn, NOW).unwrap();
    let ctx = wrap_context(account, &TAG, &[0x11; 32]);

    assert_eq!(ctx.account_id, account.to_bytes());
    assert_eq!(ctx.stream_id, TAG, "the tag occupies the stream-id slot");
    assert_eq!(ctx.key_epoch, 0, "discovery has no epochs; the slot is pinned at zero");
    assert_eq!(ctx.recipient_pub, [0x11; 32]);
}

// ---------------------------------------------------------------- envelope shape

#[test]
fn the_envelope_is_a_version_byte_then_eighty_bytes_per_recipient() {
    let conn = db();
    let account = bootstrap::local_account(&conn, NOW).unwrap();
    add_member(&conn, account, &DeviceX25519Secret::from_seed(&[0x5c; 32]));

    let sealed = seal_discovery_announcement(&conn, &TAG, &NODE_ID).unwrap().unwrap();
    assert_eq!(sealed.bytes[0], ANNOUNCEMENT_VERSION);
    assert_eq!(sealed.bytes.len(), 1 + 2 * 80, "one version byte, 80 per recipient");
    assert_eq!(wraps_of(&sealed.bytes).len(), 2);
}

/// Anything that is not one of ours is refused at parse, individually and quietly.
#[test]
fn parsing_refuses_everything_that_is_not_an_envelope() {
    let good = 1 + 80;
    assert!(parse_wraps(&[]).is_none(), "empty");
    assert!(parse_wraps(&[ANNOUNCEMENT_VERSION]).is_none(), "version with no wraps");
    assert!(parse_wraps(&vec![9u8; good]).is_none(), "unknown version");
    assert!(parse_wraps(&vec![ANNOUNCEMENT_VERSION; good + 1]).is_none(), "ragged length");
    assert!(parse_wraps(&vec![ANNOUNCEMENT_VERSION; good - 1]).is_none(), "short by one");

    // The legacy wire was a bare 32-byte node id. One whose first byte happens to equal the version
    // must still be refused rather than half-parsed.
    let mut legacy = [0x77u8; 32];
    legacy[0] = ANNOUNCEMENT_VERSION;
    assert!(parse_wraps(&legacy).is_none(), "a legacy node id is not an envelope");
    assert!(parse_wraps(&[0x77u8; 32]).is_none());
}

// ---------------------------------------------------------------- absent state

#[test]
fn a_store_with_no_account_seals_nothing_and_opens_nothing() {
    let conn = db();
    assert!(seal_discovery_announcement(&conn, &TAG, &NODE_ID).unwrap().is_none());
    assert!(
        open_discovery_announcement(&conn, &TAG, &[ANNOUNCEMENT_VERSION; 81]).unwrap().is_none()
    );
}

/// Opening is silent when a wrap is not ours — the ordinary case, once per foreign recipient per
/// announcement. Recording those as security events, the way the content path records unwrap
/// failures, would bury the real ones under discovery traffic.
#[test]
fn opening_a_foreign_wrap_records_no_security_event() {
    let conn = db();
    let account = bootstrap::local_account(&conn, NOW).unwrap();
    let stranger_x = DeviceX25519Secret::from_seed(&[0x9e; 32]);

    // An envelope sealed to nobody this store knows: one wrap, for a stranger.
    let payload = ContentKey::from_seed(&NODE_ID);
    let ctx = ctx_for(account, &TAG, &stranger_x);
    let sealed = keywrap::seal_content_key(&payload, &ctx, &stranger_x.public()).unwrap();
    let mut envelope = vec![ANNOUNCEMENT_VERSION];
    envelope.extend_from_slice(&sealed.ephemeral_pubkey);
    envelope.extend_from_slice(&sealed.ciphertext);

    assert_eq!(open_discovery_announcement(&conn, &TAG, &envelope).unwrap(), None);

    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM sync_security_events", [], |row| row.get(0))
        .unwrap_or(0);
    assert_eq!(events, 0, "a foreign wrap is expected, not an incident");
}
