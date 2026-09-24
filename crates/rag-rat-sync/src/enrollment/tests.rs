use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll};
use std::time::Duration;

use iroh::EndpointId;
use rag_rat_oplog::{
    AccountId, DeviceFingerprint, DeviceRole, ENROLLMENT_HELD_ENTRY_HASHES_MAX, EnrollmentBudget,
    verify_enrollment_device_add,
};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::redeem::RECEIPT_REPLAY_RETENTION_MS;
use super::session::{read_blob, write_blob};
use super::wire::{
    EnrollmentResponse, MAX_ENROLL_REQUEST_FRAME, MAX_ENROLL_RESPONSE_FRAME, RefusalCode,
};
use super::*;

const NOW: i64 = 1_700_000_000_000;

struct StallAfterBytes {
    remaining: usize,
}

impl AsyncWrite for StallAfterBytes {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.remaining == 0 {
            return Poll::Pending;
        }
        let written = self.remaining.min(bytes.len());
        self.remaining -= written;
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    conn
}

fn joiner_keys() -> ([u8; 32], [u8; 32]) {
    let conn = db();
    rag_rat_oplog::local_account(&conn, NOW).unwrap();
    conn.query_row(
        "SELECT public_key, x25519_public FROM oplog_device_identity WHERE id = 0",
        [],
        |row| {
            let ed: Vec<u8> = row.get(0)?;
            let x: Vec<u8> = row.get(1)?;
            Ok((ed.try_into().unwrap(), x.try_into().unwrap()))
        },
    )
    .unwrap()
}

fn ticket(conn: &Connection, account: AccountId, role: DeviceRole) -> InviteTicket {
    mint_invite(conn, InviteSpec {
        account_id: account,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        role,
        label: Some("laptop"),
        now_ms: &|| NOW,
        ttl: Duration::from_secs(60),
    })
    .unwrap()
}

fn invalid_endpoint_id() -> [u8; 32] {
    (0u64..1_000)
        .map(|ordinal| Sha256::digest(ordinal.to_be_bytes()).into())
        .find(|bytes| EndpointId::from_bytes(bytes).is_err())
        .expect("random-looking bytes include an invalid compressed Edwards point")
}

fn sample_ticket() -> InviteTicket {
    InviteTicket {
        kind: InviteTicketKind::Pairing,
        account_id: AccountId::from_bytes([9u8; 32]),
        inviter_node_id: crate::endpoint::node_id_from_secret([7u8; 32]),
        relay_url: "https://relay.example".into(),
        nonce: [3u8; 32].into(),
        expires_at_ms: 1_700_000_000_123,
        checkpoint_digest: None,
    }
}

/// The `/3` wire: the digest round-trips, and a writer ticket may never carry one.
#[test]
fn a_ticket_carries_the_checkpoint_digest_and_a_writer_ticket_may_not() {
    let pinned = InviteTicket { checkpoint_digest: Some([0x5a; 32]), ..sample_ticket() };
    let s = pinned.to_ticket_string();
    assert_eq!(InviteTicket::from_ticket_string(&s).unwrap(), pinned, "the digest round-trips");
    assert_ne!(s, sample_ticket().to_ticket_string(), "and it is carried in the bytes");

    // Unpinned stays `None` rather than a zero digest, so "no pin" is not spelled like a pin.
    assert_eq!(
        InviteTicket::from_ticket_string(&sample_ticket().to_ticket_string())
            .unwrap()
            .checkpoint_digest,
        None,
    );

    // Through `decode`, asserting the WRITER rule's own words: `from_ticket_string` collapses
    // decode failures into one wrapper string, so asserting on that would pass equally for an
    // arity, domain or route failure.
    let writer = InviteTicket {
        kind: InviteTicketKind::Writer,
        checkpoint_digest: Some([0x5a; 32]),
        ..sample_ticket()
    };
    let err = InviteTicket::decode(&writer.encode()).unwrap_err().to_string();
    assert!(err.contains("must not carry a digest"), "{err}");
    // And at the seam every writer consumer passes through, since the fields are public and an
    // in-process caller can skip decode entirely.
    let err = writer.expect_kind(InviteTicketKind::Writer).unwrap_err().to_string();
    assert!(err.contains("must not carry a digest"), "{err}");
}

/// The pasteable form of arbitrary ticket bytes, so a test can drive the path an operator does
/// (`from_ticket_string`) rather than only `decode`. Encoded by the SAME implementation production
/// uses, so the test cannot pass on a spelling the real parser would never see.
fn ticket_string(bytes: &[u8]) -> String {
    struct Raw(Vec<u8>);
    impl iroh_tickets::Ticket for Raw {
        const KIND: &'static str = super::ticket::TICKET_KIND_PREFIX;

        fn encode_bytes(&self) -> Vec<u8> {
            self.0.clone()
        }

        fn decode_bytes(bytes: &[u8]) -> Result<Self, iroh_tickets::ParseError> {
            Ok(Self(bytes.to_vec()))
        }
    }
    iroh_tickets::Ticket::encode_string(&Raw(bytes.to_vec()))
}

fn ticket_bytes(
    domain: &str,
    arity: u64,
    digest: impl Fn(&mut minicbor::Encoder<&mut Vec<u8>>),
) -> Vec<u8> {
    let t = sample_ticket();
    let mut out = Vec::new();
    let mut enc = minicbor::Encoder::new(&mut out);
    enc.array(arity).unwrap();
    enc.str(domain).unwrap();
    enc.u8(0).unwrap();
    enc.bytes(&t.account_id.to_bytes()).unwrap();
    enc.bytes(&t.inviter_node_id).unwrap();
    enc.str(&t.relay_url).unwrap();
    enc.bytes(t.nonce.as_slice()).unwrap();
    enc.i64(t.expires_at_ms).unwrap();
    digest(&mut enc);
    out
}

/// The optional digest is exactly one byte string or exactly `null`; nothing else decodes.
///
/// The wrong-length cases are defended twice over: by `fixed32`, and by the canonical re-encode
/// comparison, which no padded or truncated digest can survive. So deleting `fixed32` alone leaves
/// this green — the assertions pin the OUTCOME, not any one guard.
#[test]
fn the_optional_digest_admits_no_other_spelling() {
    let current = "rag-rat/invite-ticket/4";
    // Each case names the reason it must be refused FOR. The three classes are genuinely distinct —
    // a wrong datatype never reaches the length check — so one blanket substring would be asserting
    // something the decoder does not do, and an `||` chain over all three is barely stronger than
    // `is_err()`.
    for (name, expected, bytes) in [
        (
            "undefined",
            "expected bytes",
            ticket_bytes(current, 8, |e| {
                e.undefined().unwrap();
            }),
        ),
        (
            "a bool",
            "expected bytes",
            ticket_bytes(current, 8, |e| {
                e.bool(false).unwrap();
            }),
        ),
        (
            "an integer",
            "expected bytes",
            ticket_bytes(current, 8, |e| {
                e.u8(0).unwrap();
            }),
        ),
        (
            "a text string",
            "expected bytes",
            ticket_bytes(current, 8, |e| {
                e.str("no").unwrap();
            }),
        ),
        (
            "a short digest",
            "digest",
            ticket_bytes(current, 8, |e| {
                e.bytes(&[0; 31]).unwrap();
            }),
        ),
        (
            "a long digest",
            "digest",
            ticket_bytes(current, 8, |e| {
                e.bytes(&[0; 33]).unwrap();
            }),
        ),
        ("the field omitted", "arity", ticket_bytes(current, 7, |_| {})),
    ] {
        let err = InviteTicket::decode(&bytes).unwrap_err().to_string();
        assert!(err.contains(expected), "{name} must be refused naming `{expected}`: {err}");
    }
    assert_eq!(
        InviteTicket::decode(&ticket_bytes("rag-rat/invite-ticket/4", 8, |e| {
            e.null().unwrap();
        }))
        .unwrap(),
        sample_ticket(),
        "`null` is the one spelling of `None` that survives",
    );
}

/// The domain and the version constant are one fact spelled twice. A bump that edits the domain
/// and forgets the version inverts the message for the MOST common skew at any bump — the previous
/// release's ticket — telling that operator to upgrade a machine that is already current.
#[test]
fn the_ticket_domain_and_version_constant_agree() {
    use super::ticket::{
        OLDEST_DECODED_DOMAIN, OLDEST_DECODED_VERSION, TICKET_DOMAIN, TICKET_DOMAIN_STEM,
        TICKET_VERSION,
    };
    assert_eq!(TICKET_DOMAIN, format!("{TICKET_DOMAIN_STEM}{TICKET_VERSION}"));
    assert_eq!(OLDEST_DECODED_DOMAIN, format!("{TICKET_DOMAIN_STEM}{OLDEST_DECODED_VERSION}"));
}

/// Version skew is diagnosed on the path an OPERATOR takes, at the shapes releases actually mint.
///
/// `/1` was a 6-element array, `/2` a 7-element one, `/3` and `/4` are 8. Asserting arity before
/// reading the domain would make every one of these die on arity and never reach the message
/// written for it — and a fixture that spells an old domain at the CURRENT arity would not notice,
/// because that shape has never been minted by anything.
#[test]
fn version_skew_names_the_action_that_can_work() {
    // Exactly what the `/2` release emitted: seven elements, no digest field.
    let older = ticket_bytes("rag-rat/invite-ticket/2", 7, |_| {});
    let err = InviteTicket::from_ticket_string(&ticket_string(&older)).unwrap_err().to_string();
    assert!(err.contains("older rag-rat"), "{err}");
    assert!(err.contains("re-mint"), "and names the owner's action: {err}");

    // `/3` is NOT a skew for this binary: same layout, and every `/3` ticket was minted under the
    // founder-only gate, so its DeviceAdd is one this verifier accepts. Refusing it would strand a
    // joiner that upgraded while holding a `/3` ticket whose enrollment was already committed.
    let previous = ticket_bytes("rag-rat/invite-ticket/3", 8, |e| {
        e.null().unwrap();
    });
    assert_eq!(
        InviteTicket::from_ticket_string(&ticket_string(&previous)).unwrap(),
        sample_ticket(),
        "a `/3` ticket still decodes",
    );

    // The common direction: the owner runs `sync init`, so the MINTING side upgrades first.
    // "Ask for a re-mint" is the one action that cannot help here.
    let newer = ticket_bytes("rag-rat/invite-ticket/5", 8, |e| {
        e.null().unwrap();
    });
    let err = InviteTicket::from_ticket_string(&ticket_string(&newer)).unwrap_err().to_string();
    assert!(err.contains("newer rag-rat"), "{err}");
    assert!(err.contains("upgrade"), "and names THIS machine's action: {err}");

    // A domain the format never had is not blamed on anybody's release.
    let junk = ticket_bytes("rag-rat/not-a-ticket/1", 8, |e| {
        e.null().unwrap();
    });
    let err = InviteTicket::decode(&junk).unwrap_err().to_string();
    assert!(!err.contains("rag-rat —"), "arbitrary bytes are not a version skew: {err}");

    // `04` and `03` parse as revisions this binary decodes. Corrupt, not skewed: sending the
    // operator to upgrade or to ask for a re-mint would both be wrong, since neither release is
    // at fault.
    for spelling in [
        "rag-rat/invite-ticket/04",
        "rag-rat/invite-ticket/+4",
        "rag-rat/invite-ticket/03",
        "rag-rat/invite-ticket/+3",
    ] {
        let bytes = ticket_bytes(spelling, 8, |e| {
            e.null().unwrap();
        });
        let err = InviteTicket::decode(&bytes).unwrap_err().to_string();
        assert!(!err.contains("rag-rat —"), "{spelling} is not a skew: {err}");
    }
}

/// The `/4` bytes are frozen in BOTH directions: a ticket minted by one build of this revision must
/// decode in the next. Asserting only `encode` would let a future stricter `decode` reject the
/// frozen bytes while the round-trip stayed green, since a round trip moves with whatever `encode`
/// emits.
#[test]
fn golden_invite_ticket_v4() {
    let pinned = InviteTicket { checkpoint_digest: Some([0x5a; 32]), ..sample_ticket() };
    let pinned_hex = "88777261672d7261742f696e766974652d7469636b65742f3400582009090909090909090909090909090909090909090909090909090909090909095820ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c7568747470733a2f2f72656c61792e6578616d706c65582003030303030303030303030303030303030303030303030303030303030303031b0000018bcfe5687b58205a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
    let unpinned_hex = "88777261672d7261742f696e766974652d7469636b65742f3400582009090909090909090909090909090909090909090909090909090909090909095820ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c7568747470733a2f2f72656c61792e6578616d706c65582003030303030303030303030303030303030303030303030303030303030303031b0000018bcfe5687bf6";
    assert_eq!(rag_rat_base::hash::hex_lower(&pinned.encode()), pinned_hex);
    assert_eq!(rag_rat_base::hash::hex_lower(&sample_ticket().encode()), unpinned_hex);
    assert_eq!(
        InviteTicket::decode(&rag_rat_base::hash::hex_decode(pinned_hex).unwrap()).unwrap(),
        pinned,
        "the frozen bytes still decode",
    );
    assert_eq!(
        InviteTicket::decode(&rag_rat_base::hash::hex_decode(unpinned_hex).unwrap()).unwrap(),
        sample_ticket(),
    );
    // The same bytes under the `/3` domain still decode, to the same ticket. Only the unpinned
    // shape was ever minted as `/3` — every `/3` release refused to mint under a pin — but the
    // decoder does not depend on that, so both are pinned here.
    for (hex, expected) in [(pinned_hex, &pinned), (unpinned_hex, &sample_ticket())] {
        let v3 = hex.replacen("7469636b65742f34", "7469636b65742f33", 1);
        assert_ne!(
            v3, hex,
            "the `/4` domain bytes must be present to rewrite, or this retests `/4`"
        );
        assert_eq!(
            InviteTicket::decode(&rag_rat_base::hash::hex_decode(&v3).unwrap()).unwrap(),
            *expected,
            "a `/3` ticket from the previous release still decodes",
        );
    }
}

#[test]
fn ticket_string_round_trips_through_the_scheme_tag() {
    let ticket = sample_ticket();
    let s = ticket.to_ticket_string();
    assert!(s.starts_with("ragratinvite"), "the kind prefix opens the string: {s}");
    assert!(
        s[..].chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
        "the canonical form is lowercase base32 after the prefix: {s}",
    );
    assert_eq!(InviteTicket::from_ticket_string(&s).unwrap(), ticket);
    assert_eq!(
        InviteTicket::from_ticket_string(&format!("  {s}\n")).unwrap(),
        ticket,
        "surrounding whitespace on a pasted line is tolerated",
    );
}

#[test]
fn ticket_string_rejects_a_missing_tag_and_corrupt_body() {
    let ticket = sample_ticket();
    let s = ticket.to_ticket_string();
    let body = s.strip_prefix("ragratinvite").unwrap();
    assert!(
        matches!(InviteTicket::from_ticket_string(body), Err(InviteError::Malformed(_))),
        "the bare base32 without the kind prefix is refused",
    );
    assert!(
        matches!(
            InviteTicket::from_ticket_string("ragratinvite0189"),
            Err(InviteError::Malformed(_))
        ),
        "a non-base32 body is refused",
    );
    let mut truncated = s.clone();
    truncated.truncate(s.len() - 2);
    assert!(
        InviteTicket::from_ticket_string(&truncated).is_err(),
        "dropping bytes fails the canonical-shape check",
    );
    // The wrong-kind diagnosis names the command that accepts the paste.
    let writer = InviteTicket { kind: InviteTicketKind::Writer, ..sample_ticket() };
    let err = writer.expect_kind(InviteTicketKind::Pairing).unwrap_err().to_string();
    assert!(err.contains("sync contribute"), "{err}");
    let err = sample_ticket().expect_kind(InviteTicketKind::Writer).unwrap_err().to_string();
    assert!(err.contains("sync join"), "{err}");
}

/// A budget no honest redemption can exceed, for tests that are not exercising the capacity
/// check.
fn generous_budget() -> EnrollmentBudget {
    EnrollmentBudget {
        account_entries_remaining: u64::MAX,
        account_bytes_remaining: u64::MAX,
        global_entries_remaining: u64::MAX,
        global_bytes_remaining: u64::MAX,
    }
}

#[test]
fn request_is_canonical_and_exactly_bound() {
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: [7; 32].into(),
        expected_account: AccountId::from_bytes([8; 32]),
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: EnrollmentBudget {
            account_entries_remaining: 10,
            account_bytes_remaining: 20,
            global_entries_remaining: 30,
            global_bytes_remaining: 40,
        },
        held_entry_hashes: vec![[3; 32], [4; 32]],
    };
    let bytes = request.encode();
    assert!(
        bytes.len() <= MAX_ENROLL_REQUEST_FRAME as usize,
        "the request still fits the unauthenticated frame cap"
    );
    assert_eq!(EnrollmentRequest::decode(&bytes).unwrap(), request);

    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(matches!(EnrollmentRequest::decode(&trailing), Err(InviteError::Malformed(_))));
    // A non-minimal u64 in the budget fails the canonical re-encode check.
    let mut non_minimal = bytes.clone();
    non_minimal.truncate(bytes.len() - 2); // drop the final u8-form u64 (0x18 60)
    non_minimal.push(0x1b); // and re-encode the same value in the 8-byte form
    non_minimal.extend_from_slice(&60u64.to_be_bytes());
    assert!(matches!(EnrollmentRequest::decode(&non_minimal), Err(InviteError::Malformed(_))));

    let hashes = (0..ENROLLMENT_HELD_ENTRY_HASHES_MAX)
        .map(|ordinal| {
            let mut hash = [0u8; 32];
            hash[24..].copy_from_slice(&(ordinal as u64).to_be_bytes());
            hash
        })
        .collect::<Vec<_>>();
    let maximal = EnrollmentRequest { held_entry_hashes: hashes, ..request.clone() };
    let maximal_bytes = maximal.encode();
    assert!(maximal_bytes.len() <= MAX_ENROLL_REQUEST_FRAME as usize);
    assert_eq!(EnrollmentRequest::decode(&maximal_bytes).unwrap(), maximal);

    let mut unsorted = request.clone();
    unsorted.held_entry_hashes = vec![[2; 32], [1; 32]];
    assert!(matches!(
        EnrollmentRequest::decode(&unsorted.encode()),
        Err(InviteError::Malformed(_))
    ));
    let mut duplicate = request;
    duplicate.held_entry_hashes = vec![[2; 32], [2; 32]];
    assert!(matches!(
        EnrollmentRequest::decode(&duplicate.encode()),
        Err(InviteError::Malformed(_))
    ));
}

#[test]
fn ticket_is_canonical_and_exactly_bound() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::ReadOnly);
    let bytes = ticket.encode();
    assert_eq!(InviteTicket::decode(&bytes).unwrap(), ticket);

    let mut trailing = bytes;
    trailing.push(0);
    assert!(matches!(InviteTicket::decode(&trailing), Err(InviteError::Malformed(_))));

    let invalid_node = InviteTicket { inviter_node_id: invalid_endpoint_id(), ..ticket.clone() };
    assert!(matches!(
        InviteTicket::decode(&invalid_node.encode()),
        Err(InviteError::Malformed(message)) if message.contains("node id")
    ));
    let invalid_relay = InviteTicket { relay_url: "not a relay URL".into(), ..ticket };
    assert!(matches!(
        InviteTicket::decode(&invalid_relay.encode()),
        Err(InviteError::Malformed(message)) if message.contains("relay URL")
    ));
}

#[test]
fn mint_requires_live_founder_authority_and_leaves_no_invite_on_failure() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    conn.execute(
        "UPDATE account_owner_incarnations
                SET closed_at = ?2,
                    control_boundary = 'closed',
                    control_seq = NULL,
                    control_hash = NULL
              WHERE account_id = ?1",
        params![account.to_bytes().as_slice(), NOW + 1],
    )
    .unwrap();
    conn.execute(
        "UPDATE account_roster_history
                SET closed_at = ?2,
                    control_boundary = 'closed',
                    control_seq = NULL,
                    control_hash = NULL
              WHERE account_id = ?1",
        params![account.to_bytes().as_slice(), NOW + 1],
    )
    .unwrap();

    let result = mint_invite(&conn, InviteSpec {
        account_id: account,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        role: DeviceRole::Member,
        label: Some("laptop"),
        now_ms: &|| NOW + 2,
        ttl: Duration::from_secs(60),
    });

    assert!(matches!(result, Err(InviteError::Storage(_))));
    let invites: i64 =
        conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
    assert_eq!(invites, 0, "a device unable to redeem must not distribute an invite");
    let synchronous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0)).unwrap();
    assert_eq!(synchronous, 1, "the authored-durability guard restores NORMAL");
}

#[test]
fn mint_rejects_founder_authority_bounded_by_a_cut() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let genesis_hash: Vec<u8> = conn
        .query_row("SELECT genesis_entry_hash FROM oplog_local_account WHERE id = 0", [], |row| {
            row.get(0)
        })
        .unwrap();
    conn.execute(
        "UPDATE account_roster_history
                SET control_boundary = 'cut',
                    control_seq = ?2,
                    control_hash = ?3
              WHERE account_id = ?1",
        params![account.to_bytes().as_slice(), 0_u64.to_be_bytes().as_slice(), genesis_hash,],
    )
    .unwrap();

    let result = mint_invite(&conn, InviteSpec {
        account_id: account,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        role: DeviceRole::Member,
        label: Some("laptop"),
        now_ms: &|| NOW + 1,
        ttl: Duration::from_secs(60),
    });

    assert!(matches!(result, Err(InviteError::Storage(_))));
    let invites: i64 =
        conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
    assert_eq!(invites, 0, "bounded founder authority must not mint an invite");
}

#[test]
fn mint_rejects_an_unauthorable_label_before_persisting_the_nonce() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let label = "x".repeat(64 * 1024);

    let result = mint_invite(&conn, InviteSpec {
        account_id: account,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        role: DeviceRole::Member,
        label: Some(&label),
        now_ms: &|| NOW,
        ttl: Duration::from_secs(60),
    });

    assert!(matches!(result, Err(InviteError::Malformed(_))));
    let invites: i64 =
        conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
    assert_eq!(invites, 0, "an unusable invite must never cross the mint boundary");
}

#[test]
fn mint_rejects_an_unparseable_relay_url_before_persisting_the_nonce() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let result = mint_invite(&conn, InviteSpec {
        account_id: account,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "not a relay URL".into(),
        role: DeviceRole::Member,
        label: None,
        now_ms: &|| NOW,
        ttl: Duration::from_secs(60),
    });
    assert!(matches!(result, Err(InviteError::Malformed(_))));
    let invites: i64 =
        conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
    assert_eq!(invites, 0);
}

#[test]
fn mint_rejects_an_invalid_inviter_node_before_persisting_the_nonce() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let result = mint_invite(&conn, InviteSpec {
        account_id: account,
        inviter_node_id: invalid_endpoint_id(),
        relay_url: "https://relay.example".into(),
        role: DeviceRole::Member,
        label: None,
        now_ms: &|| NOW,
        ttl: Duration::from_secs(60),
    });
    assert!(matches!(result, Err(InviteError::Malformed(_))));
    let invites: i64 =
        conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
    assert_eq!(invites, 0, "an undialable node id must fail before invite persistence");
}

#[test]
fn mint_rejects_a_live_key_the_founder_cannot_recover() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let stream = rag_rat_oplog::ensure_owned_stream_v2_in_tx(&tx, "recovery-test", NOW).unwrap();
    rag_rat_oplog::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW).unwrap();
    tx.commit().unwrap();

    let other = db();
    rag_rat_oplog::local_account(&other, NOW).unwrap();
    let (secret, public): (Vec<u8>, Vec<u8>) = other
        .query_row(
            "SELECT x25519_secret, x25519_public FROM oplog_device_identity WHERE id = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    conn.execute(
        "UPDATE oplog_device_identity SET x25519_secret = ?1, x25519_public = ?2 WHERE id = 0",
        params![secret, public],
    )
    .unwrap();

    let result = mint_invite(&conn, InviteSpec {
        account_id: account,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        role: DeviceRole::Member,
        label: None,
        now_ms: &|| NOW + 1,
        ttl: Duration::from_secs(60),
    });
    assert!(matches!(result, Err(InviteError::Storage(_))));
    let invites: i64 =
        conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
    assert_eq!(invites, 0, "an unrecoverable live key must gate invite issuance");
}

#[test]
fn mint_refuses_a_ttl_that_is_already_expired() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    for ttl in [Duration::ZERO, Duration::from_nanos(999_999)] {
        let result = mint_invite(&conn, InviteSpec {
            account_id: account,
            inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
            relay_url: "https://relay.example".into(),
            role: DeviceRole::Member,
            label: None,
            now_ms: &|| NOW,
            ttl,
        });
        assert!(
            matches!(result, Err(InviteError::Malformed(_))),
            "a {ttl:?} TTL mints a ticket redemption always rejects as expired"
        );
    }
    let invites: i64 =
        conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
    assert_eq!(invites, 0, "an unusable invite must never cross the mint boundary");
}

#[test]
fn a_wrong_account_enrollment_is_refused_before_the_arrival_clock_is_read() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: AccountId::from_bytes([0x55; 32]),
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let clock_reads = std::cell::Cell::new(0);
    let clock = || {
        clock_reads.set(clock_reads.get() + 1);
        NOW + 1
    };
    assert!(matches!(
        redeem_invite(&conn, request, [9; 32], &clock),
        Err(InviteError::AccountMismatch)
    ));
    assert_eq!(clock_reads.get(), 0, "the account check runs before the arrival clock is read");
}

#[test]
fn every_budget_scope_gates_the_consume() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    type BudgetScope = (&'static str, fn(&mut EnrollmentBudget));
    let scopes: [BudgetScope; 4] = [
        ("account_entries_remaining", |budget| budget.account_entries_remaining = 0),
        ("account_bytes_remaining", |budget| budget.account_bytes_remaining = 0),
        ("global_entries_remaining", |budget| budget.global_entries_remaining = 0),
        ("global_bytes_remaining", |budget| budget.global_bytes_remaining = 0),
    ];
    for (name, zero) in scopes {
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let mut budget = generous_budget();
        zero(&mut budget);
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget,
            held_entry_hashes: Vec::new(),
        };
        assert!(
            matches!(
                redeem_invite(&conn, request, [9; 32], &|| NOW + 1),
                Err(InviteError::JoinerCapacity)
            ),
            "zeroing {name} must refuse"
        );
        let used: Option<i64> = conn
            .query_row(
                "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
                [ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(used, None, "the {name} refusal must not consume the nonce");
    }
}

#[test]
fn redemption_charges_only_receipt_entries_the_joiner_does_not_hold() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    // Enroll one device so the next receipt carries genesis + AddB + AddJoiner.
    let first_ticket = ticket(&conn, account, DeviceRole::Member);
    let (first_ed, first_x) = joiner_keys();
    let first = EnrollmentRequest {
        nonce: first_ticket.nonce,
        expected_account: account,
        ed25519_pubkey: first_ed,
        x25519_pubkey: first_x,
        transport_node_id: [8; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let _ = redeem_invite(&conn, first, [8; 32], &|| NOW + 1).unwrap();

    // The joiner proves it already holds every prior entry (genesis + AddB): only the new
    // DeviceAdd is charged, so a one-entry budget fits.
    let prior = rag_rat_oplog::account_entries_for_sync(&conn, account).unwrap();
    assert_eq!(prior.len(), 2, "genesis plus the first DeviceAdd");
    let mut held = [prior[0].entry_hash, prior[1].entry_hash];
    held.sort_unstable();
    let second_ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: second_ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: EnrollmentBudget { account_entries_remaining: 1, ..generous_budget() },
        held_entry_hashes: held.iter().map(|hash| hash.to_bytes()).collect(),
    };
    let _ = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 2).unwrap();
}

#[test]
fn redemption_preserves_the_nonce_when_the_receipt_exceeds_the_declared_budget() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    // Enroll one device so the next receipt carries history beyond genesis + its DeviceAdd.
    let first_ticket = ticket(&conn, account, DeviceRole::Member);
    let (first_ed, first_x) = joiner_keys();
    let first = EnrollmentRequest {
        nonce: first_ticket.nonce,
        expected_account: account,
        ed25519_pubkey: first_ed,
        x25519_pubkey: first_x,
        transport_node_id: [8; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let _ = redeem_invite(&conn, first, [8; 32], &|| NOW + 1).unwrap();

    let second_ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let mut request = EnrollmentRequest {
        nonce: second_ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    // The receipt is genesis + two DeviceAdds; a budget for only two entries cannot hold it.
    request.budget.account_entries_remaining = 2;
    assert!(matches!(
        redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 2),
        Err(InviteError::JoinerCapacity)
    ));
    let used: Option<i64> = conn
        .query_row(
            "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
            [second_ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(used, None, "a capacity refusal must not consume the nonce");
    // The same invite redeems once the joiner declares honest headroom.
    request.budget = generous_budget();
    let _ = redeem_invite(&conn, request, [9; 32], &|| NOW + 3).unwrap();
}

#[test]
fn mint_reserves_and_redemption_releases_the_mandatory_candidate_capacity() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (reserved_entries, reserved_bytes): (i64, i64) = conn
        .query_row(
            "SELECT reserved_entries, reserved_bytes
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
            [ticket.nonce.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("minting persists a reservation for the mandatory redemption entries");
    assert!(reserved_entries >= 1, "the DeviceAdd itself must be reserved");
    assert!(reserved_bytes > 0, "the reserved entries must carry their byte cost");

    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let _ = redeem_invite(&conn, request, [9; 32], &|| NOW + 1).unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_candidate_reservations WHERE reservation_id = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "redemption releases its own reservation under the writer lock");
}

#[test]
fn an_outstanding_invite_reservation_gates_the_next_mint_until_expiry() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    // Simulate an already-minted invite whose reservation consumes the whole per-account
    // candidate budget: the next mint must refuse rather than distribute a ticket the first
    // redemption would strand.
    // The TTL is wall-clock live: the mint preflight charges reservations that are outstanding
    // against the WALL CLOCK (#1362), so a TTL anchored to the fixture's fixed past `NOW` would
    // already have lapsed and would gate nothing.
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    rag_rat_oplog::upsert_account_candidate_reservation_in_tx(
        &tx,
        account,
        [0x77; 32],
        4_096,
        0,
        0,
        rag_rat_base::time::now_ms() + 3_600_000,
    )
    .unwrap();
    tx.commit().unwrap();

    let spec = |now_ms: &'static dyn Fn() -> i64| InviteSpec {
        account_id: account,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        role: DeviceRole::Member,
        label: None,
        now_ms,
        ttl: Duration::from_secs(60),
    };
    assert!(matches!(mint_invite(&conn, spec(&|| NOW)), Err(InviteError::Storage(_))));
    let invites: i64 =
        conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
    assert_eq!(invites, 0, "no second invite may be minted against reserved capacity");
    // After the outstanding reservation expires, minting prunes it and succeeds. Expiry is judged
    // against the wall clock, so lapse the ROW rather than advancing the mint's own clock.
    conn.execute(
        "UPDATE account_candidate_reservations SET expires_at_ms = ?1 WHERE reservation_id = ?2",
        params![rag_rat_base::time::now_ms() - 1_000, [0x77u8; 32].as_slice()],
    )
    .unwrap();
    let _ = mint_invite(&conn, spec(&|| NOW)).expect("expiry frees the reservation");
}

#[test]
fn mint_reads_the_clock_once_after_acquiring_the_writer_lock() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let clock_reads = AtomicI64::new(0);
    let ticket = mint_invite(&conn, InviteSpec {
        account_id: account,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        role: DeviceRole::Member,
        label: None,
        now_ms: &|| NOW + clock_reads.fetch_add(1, Ordering::SeqCst),
        ttl: Duration::from_secs(60),
    })
    .unwrap();
    assert_eq!(
        clock_reads.load(Ordering::SeqCst),
        1,
        "one post-lock read: a pre-lock timestamp cannot mint an already-expired ticket"
    );
    let created_at_ms: i64 = conn
        .query_row(
            "SELECT created_at_ms FROM sync_invites WHERE nonce = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ticket.expires_at_ms, created_at_ms + 60_000);
}

#[test]
fn new_mandatory_key_targets_grow_an_outstanding_invites_reservation() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    // Minted against the WALL CLOCK rather than the shared `ticket` helper's fixed `NOW`: the
    // top-up decides an invite is outstanding by the real clock (#1362), so a TTL anchored to a
    // past instant is already lapsed and its reservation would correctly never grow.
    let ticket = mint_invite(&conn, InviteSpec {
        account_id: account,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        role: DeviceRole::Member,
        label: Some("laptop"),
        now_ms: &rag_rat_base::time::now_ms,
        ttl: Duration::from_secs(3600),
    })
    .unwrap();
    let reservation_of = |nonce: [u8; 32]| {
        conn.query_row(
            "SELECT reserved_entries, reserved_bytes
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
            [nonce.as_slice()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap()
    };
    let (entries0, bytes0) = reservation_of(*ticket.nonce.as_bytes());
    assert_eq!(entries0, 1, "no live keys yet: only the DeviceAdd is reserved");

    // Authoring a new live key target grows the outstanding invite's reservation in the same
    // transaction, so ordinary candidate writes cannot consume the headroom redemption will
    // need for the catch-up wrap.
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let stream = rag_rat_oplog::ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW + 1).unwrap();
    rag_rat_oplog::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW + 1).unwrap();
    tx.commit().unwrap();
    let (entries1, bytes1) = reservation_of(*ticket.nonce.as_bytes());
    assert_eq!(entries1, entries0 + 1, "one new live key target is one reserved wrap");
    assert!(bytes1 > bytes0, "the reserved wrap carries its byte cost");

    // Redemption re-measures the CURRENT requirement and succeeds against the grown
    // reservation, delivering the post-mint key's wrap to the joiner.
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (_, catch_up) = redeem_invite(&conn, request, [9; 32], &|| NOW + 2).unwrap();
    assert_eq!(catch_up.authored.len(), 1, "the post-mint key is wrapped for the joiner");
}

#[test]
fn synced_key_target_growth_tops_up_the_outstanding_reservation() {
    // Another device of the same account authors a StreamOwn + key; the entries arrive here
    // through ordinary (untrusted) account sync. The fold-time top-up must grow the
    // outstanding invite's reservation to cover the new mandatory catch-up wrap — the local
    // authoring hooks never see this transition.
    // Enroll this store's device so it holds a roster wrap recipient: key recovery targets
    // are measured for the local device, exactly as they are on a real inviter.
    let inviter = db();
    let account = rag_rat_oplog::local_account(&inviter, NOW).unwrap();
    let conn = db();
    let _ = rag_rat_oplog::local_device(&conn, NOW).unwrap();
    let (ed25519_pubkey, x25519_pubkey): ([u8; 32], [u8; 32]) = conn
        .query_row(
            "SELECT public_key, x25519_public FROM oplog_device_identity WHERE id = 0",
            [],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .map(|(ed, x)| (ed.try_into().unwrap(), x.try_into().unwrap()))
        .unwrap();
    let ticket = ticket(&inviter, account, DeviceRole::Member);
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (receipt, _) = redeem_invite(&inviter, request, [9; 32], &|| NOW + 1).unwrap();
    let genesis_hash = rag_rat_oplog::verify_enrollment_device_add(
        &receipt.account_entries,
        account,
        receipt.device_add_hash.into(),
        &receipt.device_add_signed,
        ed25519_pubkey,
        x25519_pubkey,
    )
    .unwrap();
    let fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(ed25519_pubkey).into());
    rag_rat_oplog::adopt_enrollment_bootstrap(&conn, rag_rat_oplog::EnrollmentBootstrap {
        account_entries: &receipt.account_entries,
        account_id: account,
        genesis_hash,
        device_fingerprint: fingerprint,
        device_add_hash: receipt.device_add_hash.into(),
        now_ms: NOW + 1,
    })
    .unwrap();

    // The founder authors the new stream + key AFTER this device enrolled; the wrap names it.
    let tx = Transaction::new_unchecked(&inviter, TransactionBehavior::Immediate).unwrap();
    let stream = rag_rat_oplog::ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW + 2).unwrap();
    rag_rat_oplog::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW + 2).unwrap();
    tx.commit().unwrap();
    let entries = rag_rat_oplog::account_entries_for_sync(&inviter, account).unwrap();

    // An outstanding invite on this store reserved only the DeviceAdd (no targets yet). Its TTL is
    // wall-clock live, which is what makes it outstanding to the top-up (#1362) — the fixture's
    // `NOW` is a fixed past instant, so a TTL near it has already lapsed.
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    rag_rat_oplog::upsert_account_candidate_reservation_in_tx(
        &tx,
        account,
        [0x88; 32],
        1,
        200,
        0,
        rag_rat_base::time::now_ms() + 3_600_000,
    )
    .unwrap();
    tx.commit().unwrap();
    // The StreamOwn + wrap arrive through ordinary untrusted account sync.
    for entry in &entries {
        let _ = rag_rat_oplog::account_ingest(&conn, &entry.signed_bytes, NOW + 2).unwrap();
    }
    let (reserved_entries, reserved_targets): (i64, i64) = conn
        .query_row(
            "SELECT reserved_entries, reserved_targets
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
            [[0x88; 32].as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(reserved_targets, 1, "the synced key target is now covered");
    assert_eq!(reserved_entries, 2, "DeviceAdd plus the new mandatory wrap");

    // A sealed /3 entry authored under the FIRST key, then a rotation to a second key: the
    // accepted content pins the historical key as a live catch-up target only when the
    // CONTENT settles — a path no account fold traverses (#949).
    rag_rat_oplog::author_content_batch(
        &inviter,
        stream,
        &[rag_rat_oplog::MemoryOp::NodeCreate {
            node_id: rag_rat_oplog::NodeId::from("n1"),
            content: rag_rat_oplog::NodeContent {
                kind: "Invariant".into(),
                title: "t".into(),
                body: "body".into(),
                confidence: "high".into(),
                source: "agent".into(),
                tags: Vec::new(),
                payload: None,
            },
        }],
        rag_rat_oplog::SealPolicy::Sealed,
        NOW + 3,
    )
    .unwrap();
    let sealed = rag_rat_oplog::content_entries_for_sync(&inviter, account).unwrap();
    let sealed = sealed.last().expect("one sealed content entry").signed_bytes.clone();
    let tx = Transaction::new_unchecked(&inviter, TransactionBehavior::Immediate).unwrap();
    rag_rat_oplog::rotate_stream_key_in_tx(&tx, stream, NOW + 4).unwrap();
    tx.commit().unwrap();
    // The rotation's wrap arrives by account sync; with no accepted content yet, the live
    // set is still only the selected key.
    for entry in rag_rat_oplog::account_entries_for_sync(&inviter, account).unwrap() {
        let _ = rag_rat_oplog::account_ingest(&conn, &entry.signed_bytes, NOW + 4).unwrap();
    }
    let (_, targets_after_rotation): (i64, i64) = conn
        .query_row(
            "SELECT reserved_entries, reserved_targets
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
            [[0x88; 32].as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(targets_after_rotation, 1, "rotation alone does not pin the historical key");
    // The sealed entry settles through content sync — acceptance pins the historical key and
    // the reservation grows without any account fold.
    rag_rat_oplog::content_ingest(&conn, &sealed, NOW + 5).unwrap();
    rag_rat_oplog::settle_pending_content_refolds(
        &conn,
        &rag_rat_oplog::ContentRefoldBudget::unbounded(),
        NOW + 5,
    )
    .unwrap();
    let (entries_after_content, targets_after_content): (i64, i64) = conn
        .query_row(
            "SELECT reserved_entries, reserved_targets
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
            [[0x88; 32].as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(targets_after_content, 2, "settled content pins its sealing key as a target");
    assert_eq!(entries_after_content, 3, "both live wraps are now reserved");
}

#[test]
fn replay_reconstructs_the_exact_acknowledged_receipt_from_the_manifest() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let first_ticket = ticket(&conn, account, DeviceRole::Member);
    let (first_ed, first_x) = joiner_keys();
    let first = EnrollmentRequest {
        nonce: first_ticket.nonce,
        expected_account: account,
        ed25519_pubkey: first_ed,
        x25519_pubkey: first_x,
        transport_node_id: [8; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (original, _) = redeem_invite(&conn, first.clone(), [8; 32], &|| NOW + 1).unwrap();

    // A second enrollment advances the account snapshot AFTER the first receipt was stored.
    let second_ticket = ticket(&conn, account, DeviceRole::Member);
    let (second_ed, second_x) = joiner_keys();
    let second = EnrollmentRequest {
        nonce: second_ticket.nonce,
        expected_account: account,
        ed25519_pubkey: second_ed,
        x25519_pubkey: second_x,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let _ = redeem_invite(&conn, second, [9; 32], &|| NOW + 2).unwrap();

    // Replaying the first request reconstructs the EXACT acknowledged receipt from the hash
    // manifest — never a superset the joiner's measured capacity might not fit, and never a
    // second stored copy of the bootstrap bytes.
    let (replayed, _) = redeem_invite(&conn, first, [8; 32], &|| NOW + 3).unwrap();
    assert_eq!(replayed, original);
    let duplicated: bool = conn
        .query_row(
            "SELECT receipt_bytes IS NOT NULL FROM sync_invites WHERE nonce = ?1",
            [first_ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!duplicated, "new redemptions persist only the DeviceAdd plus the hash manifest");

    // The reconstructed receipt adopts cleanly on the original joiner.
    let joiner_db = db();
    let _ = rag_rat_oplog::local_device(&joiner_db, NOW).unwrap();
    let genesis_hash = rag_rat_oplog::verify_enrollment_device_add(
        &replayed.account_entries,
        account,
        replayed.device_add_hash.into(),
        &replayed.device_add_signed,
        first_ed,
        first_x,
    )
    .unwrap();
    let fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(first_ed).into());
    rag_rat_oplog::adopt_enrollment_bootstrap(&joiner_db, rag_rat_oplog::EnrollmentBootstrap {
        account_entries: &replayed.account_entries,
        account_id: account,
        genesis_hash,
        device_fingerprint: fingerprint,
        device_add_hash: replayed.device_add_hash.into(),
        now_ms: NOW + 3,
    })
    .unwrap();
}

#[test]
fn redemption_rejects_joiner_candidates_absent_from_the_owner_snapshot() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    // The joiner claims a candidate the owner's authenticated snapshot does not hold — a
    // competing branch or a false claim. Adoption would refold the union, so redemption
    // refuses BEFORE consuming the nonce (and before releasing the reservation).
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: vec![[0xee; 32]],
    };
    assert!(matches!(
        redeem_invite(&conn, request, [9; 32], &|| NOW + 1),
        Err(InviteError::HeldStateConflict)
    ));
    let unused: bool = conn
        .query_row(
            "SELECT used_at_ms IS NULL FROM sync_invites WHERE nonce = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(unused, "an unreconciled held-state claim must not consume the nonce");
    let reservation: bool = conn
        .query_row(
            "SELECT EXISTS(
                     SELECT 1 FROM account_candidate_reservations WHERE reservation_id = ?1)",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(reservation, "the rolled-back redemption restores the invite's reservation");
}

#[test]
fn a_replayed_receipt_ignores_the_declared_budget() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let mut request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (receipt, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1).unwrap();
    // The retry declares zero headroom: replay returns the stored receipt regardless — the
    // capacity check gates only the one-time consume.
    request.budget = EnrollmentBudget {
        account_entries_remaining: 0,
        account_bytes_remaining: 0,
        global_entries_remaining: 0,
        global_bytes_remaining: 0,
    };
    let (replayed, _) = redeem_invite(&conn, request, [9; 32], &|| NOW + 2).unwrap();
    assert_eq!(replayed, receipt);
}

/// An owner store holding a published PublicRead stream, with a minted writer invite — the
/// state `sync invite-writer` leaves behind while it serves.
fn writer_fixture() -> (Connection, AccountId, [u8; 32], InviteTicket) {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let stream = {
        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        let stream = rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(
            &tx,
            "repo-w",
            rag_rat_oplog::AccessMode::PublicRead,
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
        stream.to_bytes()
    };
    let ticket = mint_writer_invite(&conn, WriterInviteSpec {
        account_id: account,
        stream_id: stream,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        now_ms: &|| NOW,
        ttl: Duration::from_secs(60),
    })
    .unwrap();
    (conn, account, stream, ticket)
}

#[tokio::test]
async fn a_writer_invite_redeems_over_the_wire_and_replays_for_the_same_contributor() {
    let (conn, account, stream, ticket) = writer_fixture();
    assert_eq!(ticket.kind, InviteTicketKind::Writer);
    let contributor = AccountId::from_bytes([0x77; 32]);

    let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
    let (mut accept_send, mut dial_recv) = tokio::io::duplex(4096);
    let (dial, accept) = tokio::join!(
        run_writer_grant_dialer(&mut dial_recv, &mut dial_send, &ticket, contributor),
        run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || NOW + 1,),
    );
    let receipt = dial.expect("the redemption grants");
    assert_eq!(receipt.stream_id, stream, "the receipt names the granted stream");
    assert!(matches!(accept, Ok(EnrollmentAcceptorOutcome::WriterGranted(_))));
    assert_eq!(
        rag_rat_oplog::effective_writer_grant(
            &conn,
            account,
            rag_rat_oplog::StreamId::from_bytes(stream),
            contributor,
        )
        .unwrap(),
        Some(Into::into(receipt.grant_id)),
        "the grant folds effective for the contributor",
    );

    // The SAME redemption replays (a dialer that lost the response can retry) without a
    // second grant...
    let request = WriterGrantRequest {
        nonce: ticket.nonce,
        expected_account: account,
        contributor_account: contributor,
    };
    let replayed = redeem_writer_invite(&conn, &request, [9; 32], &|| NOW + 2).unwrap();
    assert_eq!(replayed.grant_id, receipt.grant_id);
    assert_eq!(
        rag_rat_oplog::open_writer_grants(
            &conn,
            account,
            rag_rat_oplog::StreamId::from_bytes(stream),
            contributor,
        )
        .unwrap()
        .len(),
        1,
        "replay authors nothing new",
    );
    // ...while ANY other contributor finds the nonce spent.
    let other = WriterGrantRequest {
        contributor_account: AccountId::from_bytes([0x88; 32]),
        ..request.clone()
    };
    assert!(matches!(
        redeem_writer_invite(&conn, &other, [9; 32], &|| NOW + 2),
        Err(InviteError::Used)
    ));

    // Past the retention window even the SAME contributor gets Used, and the consumed row is
    // pruned rather than retained indefinitely — the same bound enrollment replays honor.
    let past_retention = NOW + 2 + RECEIPT_REPLAY_RETENTION_MS;
    assert!(matches!(
        redeem_writer_invite(&conn, &request, [9; 32], &|| past_retention),
        Err(InviteError::Used)
    ));
    let retained: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sync_invites WHERE nonce = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, 0, "the replay-expired row is pruned");
}

#[test]
fn a_writer_nonce_presented_to_the_pairing_flow_is_refused_as_unknown() {
    let (conn, account, _stream, writer_ticket) = writer_fixture();
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: writer_ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let clock_reads = std::cell::Cell::new(0);
    let clock = || {
        clock_reads.set(clock_reads.get() + 1);
        NOW + 1
    };
    let error = redeem_invite(&conn, request, [9; 32], &clock).unwrap_err();
    assert!(matches!(error, InviteError::Unknown), "{error}");
    assert!(super::wire::refusal_code(&error).is_some(), "the refusal goes back on the wire");
    assert_eq!(clock_reads.get(), 0, "the kind gate refuses before the arrival clock is read");

    // The writer invite is untouched and still redeems through its own flow.
    let grant = WriterGrantRequest {
        nonce: writer_ticket.nonce,
        expected_account: account,
        contributor_account: AccountId::from_bytes([0x77; 32]),
    };
    redeem_writer_invite(&conn, &grant, [9; 32], &|| NOW + 1).unwrap();
}

#[test]
fn a_transport_failure_is_never_answered_with_a_refusal_frame() {
    let error = InviteError::Transport("enrollment dial timed out".into());
    assert!(super::wire::refusal_code(&error).is_none());
    assert_eq!(error.to_string(), "enrollment transport: enrollment dial timed out");
}

#[tokio::test]
async fn the_writer_dialer_refuses_a_pairing_ticket_by_name() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let pairing = ticket(&conn, account, DeviceRole::Member);
    let (mut dial_send, _accept_recv) = tokio::io::duplex(1024);
    let (_accept_send, mut dial_recv) = tokio::io::duplex(1024);
    let err = run_writer_grant_dialer(
        &mut dial_recv,
        &mut dial_send,
        &pairing,
        AccountId::from_bytes([0x77; 32]),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("sync join"), "the wrong kind names its command: {err}");
}

#[test]
fn writer_redemption_preflight_refusals_are_deterministic() {
    let (conn, account, _stream, writer_ticket) = writer_fixture();
    let contributor = AccountId::from_bytes([0x77; 32]);

    // An enrollment nonce presented to the grant flow is as unknown as a random one.
    let pairing = ticket(&conn, account, DeviceRole::Member);
    let cross_flow = WriterGrantRequest {
        nonce: pairing.nonce,
        expected_account: account,
        contributor_account: contributor,
    };
    assert!(matches!(
        redeem_writer_invite(&conn, &cross_flow, [9; 32], &|| NOW + 1),
        Err(InviteError::Unknown)
    ));

    // A self-grant is refused before the nonce is consumed.
    let self_grant = WriterGrantRequest {
        nonce: writer_ticket.nonce,
        expected_account: account,
        contributor_account: account,
    };
    assert!(matches!(
        redeem_writer_invite(&conn, &self_grant, [9; 32], &|| NOW + 1),
        Err(InviteError::AccountMismatch)
    ));

    // A wrong expected account is refused.
    let wrong_owner = WriterGrantRequest {
        nonce: writer_ticket.nonce,
        expected_account: AccountId::from_bytes([0x55; 32]),
        contributor_account: contributor,
    };
    assert!(matches!(
        redeem_writer_invite(&conn, &wrong_owner, [9; 32], &|| NOW + 1),
        Err(InviteError::AccountMismatch)
    ));

    // Expiry, evaluated at redemption time — and the unburned nonce still redeems within TTL.
    let request = WriterGrantRequest {
        nonce: writer_ticket.nonce,
        expected_account: account,
        contributor_account: contributor,
    };
    assert!(matches!(
        redeem_writer_invite(&conn, &request, [9; 32], &|| NOW + 120_000),
        Err(InviteError::Expired)
    ));
    redeem_writer_invite(&conn, &request, [9; 32], &|| NOW + 1)
        .expect("the refusals above consumed nothing");
}

#[tokio::test]
async fn joiner_capacity_refusal_travels_the_wire() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: EnrollmentBudget { account_entries_remaining: 0, ..generous_budget() },
        held_entry_hashes: Vec::new(),
    };
    let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
    let (mut accept_send, mut dial_recv) = tokio::io::duplex(4096);
    let (dial, accept) = tokio::join!(
        run_enrollment_dialer(&mut dial_recv, &mut dial_send, account, &request),
        run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || NOW + 1,),
    );
    assert!(matches!(dial, Err(InviteError::JoinerCapacity)));
    assert!(matches!(accept, Ok(EnrollmentAcceptorOutcome::Refused(InviteError::JoinerCapacity))));
}

#[test]
fn replay_is_refused_once_the_enrolled_device_is_removed() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::ReadOnly);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (receipt, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1).unwrap();
    // While the acknowledged DeviceAdd is roster-effective, the exact request replays.
    let (replayed, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 2).unwrap();
    assert_eq!(replayed, receipt);
    // The owner removes the device inside the replay window: the stored bootstrap and its
    // stream-key wraps must not be released again.
    conn.execute(
        "UPDATE account_roster_history SET closed_at = ?2 WHERE roster_ref = ?1",
        params![receipt.device_add_hash.as_slice(), NOW + 3],
    )
    .unwrap();
    assert!(matches!(
        redeem_invite(&conn, request, [9; 32], &|| NOW + 3),
        Err(InviteError::Revoked)
    ));
}

#[test]
fn redemption_replays_only_the_same_request_and_uses_the_server_side_role() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::ReadOnly);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (receipt, catch_up) = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1).unwrap();
    assert_eq!(catch_up.authored.len(), 0);
    let role: String = conn
        .query_row(
            "SELECT role FROM account_roster_history
                 WHERE account_id = ?1 AND roster_ref = ?2",
            params![account.to_bytes().as_slice(), receipt.device_add_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(role, "read_only");
    let (response_account, response_hash) =
        rag_rat_oplog::account_entry_ref(&receipt.device_add_signed).unwrap();
    assert_eq!(response_account, account);
    assert_eq!(response_hash, receipt.device_add_hash.into());
    assert!(
        receipt.account_entries.len() >= 2,
        "the receipt bootstraps genesis plus the DeviceAdd"
    );
    verify_enrollment_device_add(
        &receipt.account_entries,
        account,
        receipt.device_add_hash.into(),
        &receipt.device_add_signed,
        ed25519_pubkey,
        x25519_pubkey,
    )
    .unwrap();
    assert!(
        verify_enrollment_device_add(
            &receipt.account_entries,
            AccountId::from_bytes([0xff; 32]),
            receipt.device_add_hash.into(),
            &receipt.device_add_signed,
            ed25519_pubkey,
            x25519_pubkey,
        )
        .is_err(),
        "a receipt for another account is not trusted"
    );
    assert!(
        verify_enrollment_device_add(
            &receipt.account_entries,
            account,
            receipt.device_add_hash.into(),
            &receipt.device_add_signed,
            [0xee; 32],
            x25519_pubkey,
        )
        .is_err(),
        "a receipt enrolling different request keys is not trusted"
    );
    let mut forged_signed = receipt.device_add_signed.clone();
    forged_signed[0] ^= 1;
    assert!(
        verify_enrollment_device_add(
            &receipt.account_entries,
            account,
            receipt.device_add_hash.into(),
            &forged_signed,
            ed25519_pubkey,
            x25519_pubkey,
        )
        .is_err(),
        "the acknowledged signed bytes must be the verified bootstrap entry"
    );

    let joiner = db();
    let genesis_hash = verify_enrollment_device_add(
        &receipt.account_entries,
        account,
        receipt.device_add_hash.into(),
        &receipt.device_add_signed,
        ed25519_pubkey,
        x25519_pubkey,
    )
    .unwrap();
    let joiner_fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(ed25519_pubkey).into());
    rag_rat_oplog::adopt_enrollment_bootstrap(&joiner, rag_rat_oplog::EnrollmentBootstrap {
        account_entries: &receipt.account_entries,
        account_id: account,
        genesis_hash,
        device_fingerprint: joiner_fingerprint,
        device_add_hash: receipt.device_add_hash.into(),
        now_ms: NOW + 2,
    })
    .unwrap();
    assert_eq!(rag_rat_oplog::read_local_account(&joiner).unwrap(), Some(account));
    let folded_role: String = joiner
        .query_row(
            "SELECT role FROM account_roster_history
                 WHERE account_id = ?1 AND roster_ref = ?2 AND closed_at IS NULL",
            params![account.to_bytes().as_slice(), receipt.device_add_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        folded_role, "read_only",
        "bootstrap ingestion makes the joiner roster-effective before closed sync"
    );

    let interrupted_joiner = db();
    let mut invalid_bootstrap = receipt.account_entries.clone();
    invalid_bootstrap.push(vec![0xff]);
    assert!(
        rag_rat_oplog::adopt_enrollment_bootstrap(
            &interrupted_joiner,
            rag_rat_oplog::EnrollmentBootstrap {
                account_entries: &invalid_bootstrap,
                account_id: account,
                genesis_hash,
                device_fingerprint: joiner_fingerprint,
                device_add_hash: receipt.device_add_hash.into(),
                now_ms: NOW + 2,
            },
        )
        .is_err()
    );
    let retained_entries: i64 = interrupted_joiner
        .query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0))
        .unwrap();
    assert_eq!(retained_entries, 0, "a later bootstrap failure rolls back earlier entries");
    assert_eq!(
        rag_rat_oplog::read_local_account(&interrupted_joiner).unwrap(),
        None,
        "a failed bootstrap cannot publish the local-account pointer"
    );
    let (replayed, replay_catch_up) =
        redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 2).unwrap();
    assert_eq!(replayed, receipt);
    assert!(replay_catch_up.authored.is_empty());
    assert!(replay_catch_up.already_covered.is_empty());

    let mut different = request;
    different.x25519_pubkey = [7; 32];
    assert!(matches!(
        redeem_invite(&conn, different, [9; 32], &|| NOW + 2),
        Err(InviteError::Used)
    ));
}

#[test]
fn redemption_rechecks_expiry_with_the_post_lock_clock() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    // The optimistic pre-lock read sees the invite still valid; the writer-lock wait then
    // crosses the TTL, so the in-tx check must refuse with the refreshed clock instead of
    // consuming against the stale pre-wait one.
    let reads = AtomicI64::new(0);
    let clock = || {
        if reads.fetch_add(1, Ordering::SeqCst) == 0 { NOW + 1 } else { ticket.expires_at_ms }
    };
    assert!(matches!(
        redeem_invite(&conn, request.clone(), [9; 32], &clock),
        Err(InviteError::Expired)
    ));
    let used: Option<i64> = conn
        .query_row(
            "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(used, None, "an expiry-crossing redemption must not consume the nonce");
    // The still-valid invite redeems normally afterwards.
    let _ = redeem_invite(&conn, request, [9; 32], &|| NOW + 2).unwrap();
}

#[test]
fn replay_within_the_retention_window_survives_the_post_lock_clock() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (receipt, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1).unwrap();
    // A retry arriving before expiry but checked after the TTL (and within the 24h replay
    // window) must still replay the stored receipt: the in-tx replay check runs before the
    // refreshed expiry check.
    let reads = AtomicI64::new(0);
    let clock = || {
        if reads.fetch_add(1, Ordering::SeqCst) == 0 {
            NOW + 2
        } else {
            ticket.expires_at_ms + 60_000
        }
    };
    let (replayed, _) = redeem_invite(&conn, request, [9; 32], &clock).unwrap();
    assert_eq!(replayed, receipt);
}

#[test]
fn expiry_and_node_binding_fail_before_consumption() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    assert!(matches!(
        redeem_invite(&conn, request.clone(), [8; 32], &|| NOW + 1),
        Err(InviteError::WrongNode)
    ));
    assert!(matches!(
        redeem_invite(&conn, request, [9; 32], &|| ticket.expires_at_ms),
        Err(InviteError::Expired)
    ));
    let used: Option<i64> = conn
        .query_row(
            "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(used, None);
}

#[test]
fn expected_account_mismatch_fails_before_authoring_or_consumption() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let mut request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: AccountId::from_bytes([0xa5; 32]),
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let roster_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM account_roster_history", [], |row| row.get(0))
        .unwrap();

    assert!(matches!(
        redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1),
        Err(InviteError::AccountMismatch)
    ));
    let (used_at_ms, roster_after): (Option<i64>, i64) = (
        conn.query_row(
            "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap(),
        conn.query_row("SELECT COUNT(*) FROM account_roster_history", [], |row| row.get(0))
            .unwrap(),
    );
    assert_eq!(used_at_ms, None);
    assert_eq!(roster_after, roster_before);

    request.expected_account = account;
    redeem_invite(&conn, request, [9; 32], &|| NOW + 2)
        .expect("the account-mismatch refusal leaves the invite redeemable");
}

#[test]
fn receipt_replay_is_retained_for_one_day_then_pruned() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let used_at_ms = NOW + 1;
    let (receipt, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| used_at_ms).unwrap();
    let (replayed, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| {
        used_at_ms + RECEIPT_REPLAY_RETENTION_MS - 1
    })
    .unwrap();
    assert_eq!(replayed, receipt);

    assert!(matches!(
        redeem_invite(&conn, request, [9; 32], &|| used_at_ms + RECEIPT_REPLAY_RETENTION_MS),
        Err(InviteError::Used)
    ));
    let retained: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_invites WHERE nonce = ?1)",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!retained, "the expired multi-megabyte receipt row is deleted");
}

#[test]
fn failed_device_add_rolls_back_invite_consumption() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let bad = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey: [0; 32],
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    assert!(matches!(
        redeem_invite(&conn, bad, [9; 32], &|| NOW + 1),
        Err(InviteError::Storage(_))
    ));
    let good = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    redeem_invite(&conn, good, [9; 32], &|| NOW + 2)
        .expect("the failed transaction must leave the invite redeemable");
}

#[test]
fn account_entry_sized_receipt_fits_the_bootstrap_frame() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let label = "x".repeat(8 * 1024);
    let ticket = mint_invite(&conn, InviteSpec {
        account_id: account,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        role: DeviceRole::Member,
        label: Some(&label),
        now_ms: &|| NOW,
        ttl: Duration::from_secs(60),
    })
    .unwrap();
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };

    redeem_invite(&conn, request, [9; 32], &|| NOW + 1)
        .expect("the enrollment frame cap covers account-entry-sized receipts");
    let used: Option<i64> = conn
        .query_row(
            "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(used, Some(NOW + 1));
}

#[test]
fn simultaneous_redemption_has_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enrollment.sqlite");
    let conn = Connection::open(&path).unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    drop(conn);

    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for transport_node_id in [[8; 32], [9; 32]] {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        let nonce = ticket.nonce;
        handles.push(std::thread::spawn(move || {
            let conn = Connection::open(path).unwrap();
            conn.busy_timeout(Duration::from_secs(5)).unwrap();
            let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
            let request = EnrollmentRequest {
                nonce,
                expected_account: account,
                ed25519_pubkey,
                x25519_pubkey,
                transport_node_id,
                budget: generous_budget(),
                held_entry_hashes: Vec::new(),
            };
            barrier.wait();
            redeem_invite(&conn, request, transport_node_id, &|| NOW + 1)
        }));
    }
    barrier.wait();
    let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| matches!(result, Err(InviteError::Used))).count(), 1);

    let conn = Connection::open(path).unwrap();
    let additions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_roster_history
                 WHERE account_id = ?1",
            [account.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    // AccountGenesis contributes the owner row; exactly one DeviceAdd may join it.
    assert_eq!(additions, 2);
}

#[tokio::test]
async fn duplex_exchange_returns_the_authored_device_add() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
    let (mut accept_send, mut dial_recv) = tokio::io::duplex(4096);
    let (dial, accept) = tokio::join!(
        run_enrollment_dialer(&mut dial_recv, &mut dial_send, account, &request),
        run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || NOW + 1,),
    );
    let dial = dial.unwrap();
    let EnrollmentAcceptorOutcome::Enrolled(accept, _) = accept.unwrap() else {
        panic!("valid enrollment must succeed");
    };
    assert_eq!(dial, accept);
}

#[tokio::test]
async fn a_slow_but_progressing_response_completes() {
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: [42; 32].into(),
        expected_account: AccountId::from_bytes([1; 32]),
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (mut dial_send, mut accept_recv) = tokio::io::duplex(1024);
    let (mut accept_send, mut dial_recv) = tokio::io::duplex(1024);
    // Drip the refusal in small writes with gaps — comfortably inside each per-chunk window,
    // past what a whole-exchange deadline of the same size would tolerate.
    let server = async move {
        let mut prefix = [0u8; 4];
        accept_recv.read_exact(&mut prefix).await.unwrap();
        let mut req = vec![0u8; u32::from_be_bytes(prefix) as usize];
        accept_recv.read_exact(&mut req).await.unwrap();
        let response = EnrollmentResponse::Refused(RefusalCode::Unknown).encode();
        let framed = [(response.len() as u32).to_be_bytes().as_slice(), &response].concat();
        for piece in framed.chunks(8) {
            accept_send.write_all(piece).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let (dial, _) = tokio::join!(
        run_enrollment_dialer_with_progress(
            &mut dial_recv,
            &mut dial_send,
            AccountId::from_bytes([1; 32]),
            &request,
            Duration::from_millis(500),
        ),
        server,
    );
    assert!(matches!(dial, Err(InviteError::Unknown)));
}

#[tokio::test]
async fn a_stalled_response_times_out_within_one_window() {
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: [42; 32].into(),
        expected_account: AccountId::from_bytes([1; 32]),
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (mut dial_send, mut accept_recv) = tokio::io::duplex(1024);
    let (mut accept_send, mut dial_recv) = tokio::io::duplex(1024);
    let server = tokio::spawn(async move {
        let mut prefix = [0u8; 4];
        accept_recv.read_exact(&mut prefix).await.unwrap();
        let mut req = vec![0u8; u32::from_be_bytes(prefix) as usize];
        accept_recv.read_exact(&mut req).await.unwrap();
        // A valid prefix and half the body, then silence forever.
        accept_send.write_all(&128u32.to_be_bytes()).await.unwrap();
        accept_send.write_all(&[0u8; 64]).await.unwrap();
        accept_send.flush().await.unwrap();
        std::future::pending::<()>().await;
    });
    let start = std::time::Instant::now();
    let dial = run_enrollment_dialer_with_progress(
        &mut dial_recv,
        &mut dial_send,
        AccountId::from_bytes([1; 32]),
        &request,
        Duration::from_millis(100),
    )
    .await;
    server.abort();
    assert!(
        matches!(dial, Err(InviteError::Io(ref error)) if error.kind() == std::io::ErrorKind::TimedOut),
        "a stalled frame must die with a timeout: {dial:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(2), "the stall dies in one window");
}

#[tokio::test]
async fn a_stalled_reader_times_out_the_frame_write() {
    let (mut send, _recv) = tokio::io::duplex(64);
    let body = vec![0u8; 256 * 1024];
    let result = write_blob(
        &mut send,
        &body,
        MAX_ENROLL_RESPONSE_FRAME,
        "response",
        Duration::from_millis(100),
    )
    .await;
    assert!(
        matches!(result, Err(InviteError::Io(ref error)) if error.kind() == std::io::ErrorKind::TimedOut),
        "a back-pressured write must die with a timeout: {result:?}"
    );
}

#[tokio::test]
async fn dialer_acks_the_response_once_decoded() {
    let conn = db();
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: [42; 32].into(),
        expected_account: AccountId::from_bytes([1; 32]),
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
    let (mut accept_send, mut dial_recv) = tokio::io::duplex(4096);
    let (dial, accept) = tokio::join!(
        run_enrollment_dialer(
            &mut dial_recv,
            &mut dial_send,
            AccountId::from_bytes([1; 32]),
            &request,
        ),
        run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || NOW + 1,),
    );
    assert!(matches!(dial, Err(InviteError::Unknown)));
    assert!(matches!(accept, Ok(EnrollmentAcceptorOutcome::Refused(_))));
    let mut ack = [0u8; 1];
    accept_recv.read_exact(&mut ack).await.unwrap();
    assert_eq!(ack, [RESPONSE_ACK], "the dialer acks even a refused response");
}

#[tokio::test]
async fn a_stalled_response_ack_is_bounded_and_best_effort() {
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: [42; 32].into(),
        expected_account: AccountId::from_bytes([1; 32]),
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let response = EnrollmentResponse::Refused(RefusalCode::Unknown).encode();
    let framed = [(response.len() as u32).to_be_bytes().as_slice(), &response].concat();
    let (mut response_send, mut dial_recv) = tokio::io::duplex(4096);
    response_send.write_all(&framed).await.unwrap();
    let mut dial_send = StallAfterBytes { remaining: 4 + request.encode().len() };

    let start = std::time::Instant::now();
    let dial = run_enrollment_dialer_with_progress(
        &mut dial_recv,
        &mut dial_send,
        AccountId::from_bytes([1; 32]),
        &request,
        Duration::from_millis(100),
    )
    .await;

    assert!(matches!(dial, Err(InviteError::Unknown)));
    assert!(start.elapsed() < Duration::from_secs(2), "the stalled ack is bounded");
}

#[tokio::test]
async fn duplex_exchange_returns_a_semantic_refusal() {
    let conn = db();
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: [42; 32].into(),
        expected_account: AccountId::from_bytes([1; 32]),
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
    let (mut accept_send, mut dial_recv) = tokio::io::duplex(4096);
    let (dial, accept) = tokio::join!(
        run_enrollment_dialer(
            &mut dial_recv,
            &mut dial_send,
            AccountId::from_bytes([1; 32]),
            &request,
        ),
        run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || NOW + 1,),
    );
    assert!(matches!(dial, Err(InviteError::Unknown)));
    assert!(matches!(accept, Ok(EnrollmentAcceptorOutcome::Refused(InviteError::Unknown))));
}

#[tokio::test]
async fn acceptor_checks_expiry_after_receiving_the_request() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let clock = AtomicI64::new(NOW + 1);
    let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
    let (mut accept_send, _dial_recv) = tokio::io::duplex(4096);
    let send_request = async {
        write_blob(
            &mut dial_send,
            &request.encode(),
            MAX_ENROLL_REQUEST_FRAME,
            "request",
            ENROLL_PROGRESS_TIMEOUT,
        )
        .await
        .unwrap();
        clock.store(ticket.expires_at_ms, Ordering::SeqCst);
    };
    let accept =
        run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || {
            clock.load(Ordering::SeqCst)
        });

    let (_, result) = tokio::join!(send_request, accept);
    assert!(matches!(result, Ok(EnrollmentAcceptorOutcome::Refused(InviteError::Expired))));
}

#[tokio::test]
async fn request_length_is_capped_before_allocating_its_body() {
    let (mut send, mut recv) = tokio::io::duplex(16);
    send.write_all(&(MAX_ENROLL_REQUEST_FRAME + 1).to_be_bytes()).await.unwrap();
    let error = read_blob(&mut recv, MAX_ENROLL_REQUEST_FRAME, "request", ENROLL_PROGRESS_TIMEOUT)
        .await
        .unwrap_err();
    assert!(
        matches!(error, InviteError::Malformed(message) if message.contains("request")),
        "the unauthenticated request uses its small cap"
    );
}

#[test]
fn an_unknown_stored_role_preserves_each_flows_refusal_order() {
    let (conn, account, _stream, ticket) = writer_fixture();
    conn.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
    conn.execute("UPDATE sync_invites SET role = 'future_role' WHERE nonce = ?1", [ticket
        .nonce
        .as_slice()])
        .unwrap();
    let writer = WriterGrantRequest {
        nonce: ticket.nonce,
        expected_account: account,
        contributor_account: AccountId::from_bytes([0x77; 32]),
    };
    assert!(matches!(
        redeem_writer_invite(&conn, &writer, [9; 32], &|| NOW + 1),
        Err(InviteError::Unknown)
    ));
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let pairing = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    conn.execute("UPDATE sync_invites SET expires_at_ms = ?1 WHERE nonce = ?2", rusqlite::params![
        NOW,
        ticket.nonce.as_slice()
    ])
    .unwrap();
    assert!(matches!(
        redeem_invite(&conn, pairing, [9; 32], &|| NOW + 1),
        Err(InviteError::Expired)
    ));
}

#[test]
fn writer_redemption_refuses_a_pinned_contributor_before_fresh_or_replay() {
    for replay in [false, true] {
        let (conn, account, _stream, ticket) = writer_fixture();
        let contributor_db = db();
        let contributor = rag_rat_oplog::local_account(&contributor_db, NOW).unwrap();
        let device = rag_rat_oplog::local_device(&contributor_db, NOW).unwrap();
        let tx =
            Transaction::new_unchecked(&contributor_db, TransactionBehavior::Immediate).unwrap();
        let bundle = rag_rat_oplog::prepare_checkpoint_in_tx(&tx, contributor, &device).unwrap();
        tx.commit().unwrap();
        let pin = rag_rat_oplog::TrustedCheckpointPin {
            account_id: contributor,
            checkpoint_digest: bundle.certificate_digest(),
            required_control_version: 2,
        };
        let proof = rag_rat_oplog::verify_checkpoint(pin, &bundle).unwrap();
        let request = WriterGrantRequest {
            nonce: ticket.nonce,
            expected_account: account,
            contributor_account: contributor,
        };
        if replay {
            redeem_writer_invite(&conn, &request, [9; 32], &|| NOW + 1).unwrap();
        }
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        rag_rat_oplog::pin_checkpoint_in_tx(&tx, pin, &proof).unwrap();
        tx.commit().unwrap();
        let before: i64 =
            conn.query_row("SELECT count(*) FROM account_entries", [], |r| r.get(0)).unwrap();
        let error = redeem_writer_invite(&conn, &request, [9; 32], &|| NOW + 2).unwrap_err();
        assert!(
            matches!(error, InviteError::Storage(ref source) if source.downcast_ref::<rag_rat_oplog::UnsupportedAccountControlVersion>().is_some()),
            "{error:#}"
        );
        let after: i64 =
            conn.query_row("SELECT count(*) FROM account_entries", [], |r| r.get(0)).unwrap();
        assert_eq!(before, after);
    }
}

/// A refusal the owner can emit and the joiner cannot read is a silent enrollment hang: the
/// joiner reports `unknown enrollment refusal <token>` instead of the reason it was refused for.
/// Ranging over the enum rather than a hand-listed set is what makes this hold for codes added
/// later.
///
/// The expected token is restated here rather than read from `as_str`, because a round trip is
/// blind to a RENAME: change the token in both directions and the enum still survives its own
/// encoding while the bytes on the wire move under every peer running an older build. This second
/// copy is the golden vector, and the match is exhaustive so a new code cannot skip it.
///
/// It reads the token out of the frame and compares it whole. A substring search over the encoded
/// bytes looks equivalent and is not: every refusal frame carries the literal `refused`, whose
/// tail IS the `used` token, so that spelling would assert nothing at all for `Used` — and for the
/// rest it would pass any rename that keeps the old token as a substring.
#[test]
fn every_refusal_code_survives_the_wire_and_names_its_error() {
    use strum::IntoEnumIterator;

    for code in RefusalCode::iter() {
        let expected_token = match code {
            RefusalCode::Expired => "expired",
            RefusalCode::Used => "used",
            RefusalCode::Unknown => "unknown",
            RefusalCode::WrongNode => "wrong_node",
            RefusalCode::AccountMismatch => "account_mismatch",
            RefusalCode::Revoked => "revoked",
            RefusalCode::JoinerCapacity => "joiner_capacity",
            RefusalCode::HeldStateConflict => "held_state_conflict",
            RefusalCode::CheckpointPinMoved => "checkpoint_pin_moved",
            RefusalCode::DeviceRemoved => "device_removed",
        };
        let bytes = EnrollmentResponse::Refused(code).encode();
        let mut frame = minicbor::Decoder::new(&bytes);
        frame.array().unwrap();
        frame.str().unwrap();
        assert_eq!(frame.str().unwrap(), "refused", "a refusal frame names itself");
        assert_eq!(
            frame.str().unwrap(),
            expected_token,
            "{code:?} must go on the wire as `{expected_token}`",
        );

        let decoded = EnrollmentResponse::decode(&bytes)
            .unwrap_or_else(|error| panic!("{code:?} must decode: {error}"));
        let EnrollmentResponse::Refused(round_tripped) = decoded else {
            panic!("{code:?} must decode as a refusal");
        };
        assert_eq!(round_tripped, code, "{code:?} must survive the wire unchanged");
        // The acceptor picks the token from the error and the joiner turns it back into one, so
        // the two directions have to agree or a refusal changes meaning in transit.
        assert_eq!(
            super::wire::refusal_code(&code.into_error()),
            Some(code),
            "{code:?} must map back to itself through its error",
        );
    }
}

/// The stamp an invite records is read from the account's real control policy, so a genuinely
/// pinned account must produce the pin's digest and an unpinned one must produce nothing. The
/// comparison built on it is exercised through redemption below; this is the reading underneath.
#[test]
fn the_mint_stamp_is_the_pin_the_account_actually_carries() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    assert_eq!(
        super::redeem::mint_time_pin(&conn, account).unwrap(),
        None,
        "an unpinned account records no pin",
    );

    let device = rag_rat_oplog::load_local_device(&conn).unwrap().unwrap();
    let bundle = rag_rat_oplog::propose_checkpoint(&conn, account, &device).unwrap();
    let digest = bundle.certificate_digest();
    rag_rat_oplog::install_checkpoint(
        &conn,
        rag_rat_oplog::TrustedCheckpointPin {
            account_id: account,
            checkpoint_digest: digest,
            required_control_version: 2,
        },
        &bundle,
    )
    .unwrap();

    assert_eq!(
        super::redeem::mint_time_pin(&conn, account).unwrap(),
        Some(digest),
        "a pinned account records the digest its pin names",
    );
}

/// The state the comparison refuses does not require a PINNED ACCOUNT — only a row whose recorded
/// pin disagrees with the account's. Writing that disagreement onto the row instead of into the
/// account reaches the refusal through the real redemption entry point, which a pinned account
/// cannot do while `require_supported_account_control` refuses it at the top of the screen.
fn disagree_with_the_accounts_pin(conn: &Connection, nonce: [u8; 32]) {
    let updated = conn
        .execute("UPDATE sync_invites SET checkpoint_digest = ?2 WHERE nonce = ?1", params![
            nonce.as_slice(),
            vec![7u8; 32]
        ])
        .unwrap();
    assert_eq!(updated, 1, "the invite under test must exist");
}

#[test]
fn an_invite_whose_pin_moved_is_refused_before_its_nonce_is_consumed() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    disagree_with_the_accounts_pin(&conn, *ticket.nonce.as_bytes());

    assert!(
        matches!(
            redeem_invite(&conn, request, [9; 32], &|| NOW + 1),
            Err(InviteError::CheckpointPinMoved)
        ),
        "a redemption the owner cannot honour must refuse",
    );
    let used: Option<i64> = conn
        .query_row(
            "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(used, None, "the refusal must land before the one-time nonce is spent");
}

/// A replay consumes nothing and answers from a receipt whose every field predates the pin, so the
/// mismatch must not preempt it. The case is exactly the one the replay window exists for — the
/// owner authored the enrollment and the response was lost — and refusing it would strand a joiner
/// the owner's own log counts as enrolled.
#[test]
fn a_pin_that_moved_after_consumption_still_replays_the_acknowledged_receipt() {
    let conn = db();
    let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
    let ticket = ticket(&conn, account, DeviceRole::Member);
    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (receipt, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1).unwrap();

    disagree_with_the_accounts_pin(&conn, *ticket.nonce.as_bytes());

    let (replayed, _) = redeem_invite(&conn, request, [9; 32], &|| NOW + 2)
        .expect("a lost response must still be answerable after the pin moves");
    assert_eq!(replayed, receipt, "the replay must be the receipt already acknowledged");
}

/// A store that joined `founder`'s account at `role`, exactly as enrollment adopts one: its local
/// device is the joined device, so everything it mints is authorized as that device.
fn joined_store(founder: &Connection, account: AccountId, role: DeviceRole) -> Connection {
    let joined = db();
    let device = rag_rat_oplog::local_device(&joined, NOW).unwrap();
    let tx = Transaction::new_unchecked(founder, TransactionBehavior::Immediate).unwrap();
    let device_add = rag_rat_oplog::author_device_add_in_tx(
        &tx,
        rag_rat_oplog::EnrollingDevice {
            ed25519_pubkey: device.ed25519_public_key(),
            x25519_pubkey: device.x25519_public_key(),
            label: None,
        },
        role,
        NOW,
    )
    .unwrap();
    tx.commit().unwrap();
    let entries: Vec<Vec<u8>> = rag_rat_oplog::account_entries_for_enrollment(founder, account)
        .unwrap()
        .into_iter()
        .map(|entry| entry.signed_bytes)
        .collect();
    rag_rat_oplog::adopt_enrollment_bootstrap(&joined, rag_rat_oplog::EnrollmentBootstrap {
        account_entries: &entries,
        account_id: account,
        genesis_hash: rag_rat_oplog::read_local_account_genesis(founder).unwrap().unwrap(),
        device_fingerprint: device.fingerprint(),
        device_add_hash: device_add,
        now_ms: NOW + 1,
    })
    .unwrap();
    joined
}

/// The recovery RFC 9420 §16.12 prescribes for a member that cannot continue, end to end: the
/// founder device is lost, another owner removes it and enrolls a replacement, and the replacement
/// verifies the receipt and adopts the account. Before, only the founder could mint, only a
/// founder-signed DeviceAdd verified, and an account that lost its founder device had no path back.
#[test]
fn another_owner_removes_a_lost_founder_and_enrolls_its_replacement() {
    let founder = db();
    let account = rag_rat_oplog::local_account(&founder, NOW).unwrap();
    let founder_fp = rag_rat_oplog::local_device(&founder, NOW).unwrap().fingerprint();
    let owner = joined_store(&founder, account, DeviceRole::Owner);

    let tx = Transaction::new_unchecked(&owner, TransactionBehavior::Immediate).unwrap();
    rag_rat_oplog::author_device_remove_in_tx(&tx, founder_fp, "lost", NOW + 2).unwrap();
    tx.commit().unwrap();

    let ticket = ticket(&owner, account, DeviceRole::Owner);
    let replacement = db();
    let device = rag_rat_oplog::local_device(&replacement, NOW).unwrap();
    let (ed25519_pubkey, x25519_pubkey) = (device.ed25519_public_key(), device.x25519_public_key());
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let (receipt, _) = redeem_invite(&owner, request, [9; 32], &|| NOW + 3).unwrap();
    let genesis_hash = rag_rat_oplog::verify_enrollment_device_add(
        &receipt.account_entries,
        account,
        receipt.device_add_hash.into(),
        &receipt.device_add_signed,
        ed25519_pubkey,
        x25519_pubkey,
    )
    .expect("the replacement verifies a DeviceAdd the second owner signed");
    rag_rat_oplog::adopt_enrollment_bootstrap(&replacement, rag_rat_oplog::EnrollmentBootstrap {
        account_entries: &receipt.account_entries,
        account_id: account,
        genesis_hash,
        device_fingerprint: device.fingerprint(),
        device_add_hash: receipt.device_add_hash.into(),
        now_ms: NOW + 4,
    })
    .expect("and adopts the account");

    assert_eq!(
        rag_rat_oplog::read_local_account(&replacement).unwrap(),
        Some(account),
        "the replacement now holds the account",
    );
    let (founder_enrolled, replacement_enrolled): (bool, bool) = replacement
        .query_row(
            "SELECT
                 EXISTS(SELECT 1 FROM account_roster_history
                         WHERE device_fingerprint = ?1 AND closed_at IS NULL),
                 EXISTS(SELECT 1 FROM account_roster_history
                         WHERE device_fingerprint = ?2 AND closed_at IS NULL)",
            [founder_fp.to_bytes().as_slice(), device.fingerprint().to_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(replacement_enrolled, "the replacement is on the roster");
    assert!(!founder_enrolled, "and the lost founder device is not");
}

/// A removed device's store re-enrolls under a fresh identity (#1417). Its old fingerprint is
/// refused by name with the nonce unspent — the store may never have learned of its removal — and
/// the same ticket then enrolls a staged identity, which adoption swaps in over the account the
/// store already holds. The removed device was an owner that authored an account entry the
/// inviter never received: the re-enrolling store leaves its own identity's entries out of its
/// held-state claim, or the redemption would refuse as unreconcilable.
#[test]
fn a_removed_device_is_refused_by_name_then_reenrolls_under_a_staged_identity() {
    let founder = db();
    let account = rag_rat_oplog::local_account(&founder, NOW).unwrap();
    let removed = joined_store(&founder, account, DeviceRole::Owner);
    let old = rag_rat_oplog::local_device(&removed, NOW).unwrap();
    let tx = Transaction::new_unchecked(&removed, TransactionBehavior::Immediate).unwrap();
    rag_rat_oplog::ensure_owned_stream_v2_in_tx(&tx, "unsynced-repo", NOW + 1).unwrap();
    tx.commit().unwrap();
    let tx = Transaction::new_unchecked(&founder, TransactionBehavior::Immediate).unwrap();
    rag_rat_oplog::author_device_remove_in_tx(&tx, old.fingerprint(), "forked", NOW + 2).unwrap();
    tx.commit().unwrap();
    let ticket = ticket(&founder, account, DeviceRole::Member);
    let request_for = |(ed25519_pubkey, x25519_pubkey): ([u8; 32], [u8; 32])| EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: rag_rat_oplog::held_account_entry_hashes(&removed, account)
            .unwrap()
            .into_iter()
            .map(|hash| hash.to_bytes())
            .collect(),
    };

    let refused = redeem_invite(
        &founder,
        request_for((old.ed25519_public_key(), old.x25519_public_key())),
        [9; 32],
        &|| NOW + 3,
    );
    assert!(matches!(refused, Err(InviteError::DeviceRemoved)), "{:?}", refused.err());
    let used: Option<i64> = founder
        .query_row(
            "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(used, None, "the refusal leaves the ticket spendable");

    let staged = rag_rat_oplog::stage_reenrollment_identity(&removed, NOW + 4).unwrap();
    assert_eq!(
        rag_rat_oplog::stage_reenrollment_identity(&removed, NOW + 4).unwrap(),
        staged,
        "a retry presents the same staged keys, so an owner's exact replay still matches",
    );
    assert_eq!(
        rag_rat_oplog::local_device(&removed, NOW + 4).unwrap().fingerprint(),
        old.fingerprint(),
        "staging leaves the live identity alone",
    );
    let (receipt, _) = redeem_invite(&founder, request_for(staged), [9; 32], &|| NOW + 5)
        .expect("the same ticket enrolls the staged identity");
    let genesis_hash = rag_rat_oplog::verify_enrollment_device_add(
        &receipt.account_entries,
        account,
        receipt.device_add_hash.into(),
        &receipt.device_add_signed,
        staged.0,
        staged.1,
    )
    .unwrap();
    let fresh = DeviceFingerprint::from_bytes(Sha256::digest(staged.0).into());
    rag_rat_oplog::adopt_enrollment_bootstrap(&removed, rag_rat_oplog::EnrollmentBootstrap {
        account_entries: &receipt.account_entries,
        account_id: account,
        genesis_hash,
        device_fingerprint: fresh,
        device_add_hash: receipt.device_add_hash.into(),
        now_ms: NOW + 6,
    })
    .expect("the store adopts the account it already belonged to under its new identity");
    let live = rag_rat_oplog::local_device(&removed, NOW + 7).unwrap();
    assert_eq!(live.fingerprint(), fresh, "adoption swapped the staged identity in");
    assert_eq!(rag_rat_oplog::pending_identity_keys(&removed).unwrap(), None);
    let roster = rag_rat_oplog::local_account_roster(&removed).unwrap();
    assert!(roster.iter().any(|device| device.fingerprint == fresh && device.this_device));
    assert!(roster.iter().all(|device| device.fingerprint != old.fingerprint()));
}

/// A redemption whose inviter can no longer author the DeviceAdd rolls back with the nonce unspent.
/// Here the founder minted the invite and was then removed by another owner, so by redemption time
/// it holds no owner incarnation. The refusal must come from inside the redemption transaction,
/// before `used_at_ms` is written — moving the authoring below the consume would spend the invite
/// on nothing.
#[test]
fn an_enrollment_the_founder_can_no_longer_author_leaves_the_nonce_unspent() {
    let founder = db();
    let account = rag_rat_oplog::local_account(&founder, NOW).unwrap();
    let founder_fp = rag_rat_oplog::local_device(&founder, NOW).unwrap().fingerprint();
    let ticket = ticket(&founder, account, DeviceRole::Member);

    let second = joined_store(&founder, account, DeviceRole::Owner);
    let tx = Transaction::new_unchecked(&second, TransactionBehavior::Immediate).unwrap();
    rag_rat_oplog::author_device_remove_in_tx(&tx, founder_fp, "lost", NOW + 2).unwrap();
    tx.commit().unwrap();
    for entry in rag_rat_oplog::account_entries_for_enrollment(&second, account).unwrap() {
        rag_rat_oplog::account_ingest(&founder, &entry.signed_bytes, NOW + 3).unwrap();
    }

    let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey,
        x25519_pubkey,
        transport_node_id: [9; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let error = redeem_invite(&founder, request, [9; 32], &|| NOW + 4)
        .expect_err("a removed founder cannot complete an enrollment");
    assert!(
        error.to_string().contains("author a control op"),
        "refused as a non-owner when authoring, not by something that ran before it: {error}",
    );
    let used: Option<i64> = founder
        .query_row(
            "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
            [ticket.nonce.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(used, None, "the refusal rolled back with the one-time nonce unspent");
}

/// A second owner mints a WRITER invite and redeems it: it authors the StreamGrant under its own
/// incarnation, and the grant folds effective. Writer redemption authors and checks the grant
/// before it spends the nonce, and the contributor applies no founder check, so this path had no
/// joiner half to widen — only the gate.
#[tokio::test]
async fn a_second_owner_mints_and_redeems_a_writer_invite() {
    let founder = db();
    let account = rag_rat_oplog::local_account(&founder, NOW).unwrap();
    let stream = {
        let tx = Transaction::new_unchecked(&founder, TransactionBehavior::Immediate).unwrap();
        let stream = rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(
            &tx,
            "repo-w",
            rag_rat_oplog::AccessMode::PublicRead,
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
        stream.to_bytes()
    };
    let owner = joined_store(&founder, account, DeviceRole::Owner);
    let ticket = mint_writer_invite(&owner, WriterInviteSpec {
        account_id: account,
        stream_id: stream,
        inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
        relay_url: "https://relay.example".into(),
        now_ms: &|| NOW + 2,
        ttl: Duration::from_secs(60),
    })
    .expect("a second owner mints a writer invite");
    let contributor = AccountId::from_bytes([0x78; 32]);

    let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
    let (mut accept_send, mut dial_recv) = tokio::io::duplex(4096);
    let (dial, accept) = tokio::join!(
        run_writer_grant_dialer(&mut dial_recv, &mut dial_send, &ticket, contributor),
        run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &owner, [9; 32], || NOW + 3),
    );
    let receipt = dial.expect("the redemption grants");
    assert!(matches!(accept, Ok(EnrollmentAcceptorOutcome::WriterGranted(_))));
    assert_eq!(
        rag_rat_oplog::effective_writer_grant(
            &owner,
            account,
            rag_rat_oplog::StreamId::from_bytes(stream),
            contributor,
        )
        .unwrap(),
        Some(Into::into(receipt.grant_id)),
        "the grant a second owner authored folds effective",
    );
}

/// The invite nonce is a bearer secret: whoever holds it can redeem the invite. Formatting a ticket
/// or either request with `{:?}` — in an error path, a `dbg!`, a tracing field, a failing test's
/// message — must not print it, in either Debug form. The redaction lives on the nonce's own type,
/// so a field added to any of these later cannot bring the leak back.
#[test]
fn debug_formatting_never_prints_the_invite_nonce() {
    let secret = [0xab; 32];
    let nonce: InviteNonce = secret.into();
    // The run of element values a derived Debug prints for the bytes, whitespace removed. Comparing
    // whole renderings misses the pretty form: a nested one is indented differently, and a pretty
    // array ends `171,]` where a compact one ends `171]`. The bare run appears in both.
    let squash = |text: &str| text.split_whitespace().collect::<String>();
    let leak = squash(&format!("{secret:?}")).trim_matches(|c| c == '[' || c == ']').to_owned();
    let ticket = InviteTicket { nonce, ..sample_ticket() };
    let enrollment = EnrollmentRequest {
        nonce,
        expected_account: AccountId::from_bytes([1; 32]),
        ed25519_pubkey: [2; 32],
        x25519_pubkey: [3; 32],
        transport_node_id: [4; 32],
        budget: generous_budget(),
        held_entry_hashes: Vec::new(),
    };
    let writer = WriterGrantRequest {
        nonce,
        expected_account: AccountId::from_bytes([1; 32]),
        contributor_account: AccountId::from_bytes([5; 32]),
    };
    for (name, rendered) in [
        ("ticket", format!("{ticket:?}")),
        ("ticket, pretty", format!("{ticket:#?}")),
        ("enrollment request", format!("{enrollment:?}")),
        ("enrollment request, pretty", format!("{enrollment:#?}")),
        ("writer request", format!("{writer:?}")),
        ("writer request, pretty", format!("{writer:#?}")),
    ] {
        assert!(!squash(&rendered).contains(&leak), "{name} prints the nonce: {rendered}");
        assert!(rendered.contains("<redacted>"), "{name} names the field as redacted: {rendered}");
    }
}
