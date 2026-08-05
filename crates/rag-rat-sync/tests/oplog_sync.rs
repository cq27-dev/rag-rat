//! End-to-end: real op-log entries move between two SQLite stores over the session, and the
//! receiver ingests them through the production seam (phase D, #406).
//!
//! This is the load-bearing test for D1+D2 — the unit tests exercise the protocol over an in-memory
//! store, this exercises it over `OplogSyncStore` with genuine signed account entries that the
//! receiver re-verifies via `account_ingest`.

use rag_rat_oplog::{AccountId, account_entries_for_sync, local_account};
use rag_rat_sync::{
    AuthRole, OplogSyncStore, PeerAuthorization, PeerCapability, SessionCapabilities, run_session,
};
use rusqlite::Connection;

const NOW: i64 = 1_700_000_000_000;

fn fresh_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    conn
}

async fn enroll_member_over_endpoint(
    owner_endpoint: &iroh::Endpoint,
    joiner_endpoint: &iroh::Endpoint,
    owner: &Connection,
    joiner: &Connection,
    account: AccountId,
    relay: &str,
) {
    let local = rag_rat_oplog::local_device(joiner, NOW).unwrap();
    let ticket = rag_rat_sync::mint_invite(owner, rag_rat_sync::InviteSpec {
        account_id: account,
        inviter_node_id: *owner_endpoint.id().as_bytes(),
        relay_url: relay.into(),
        role: rag_rat_oplog::DeviceRole::Member,
        label: None,
        now_ms: &|| NOW,
        ttl: std::time::Duration::from_secs(60),
    })
    .unwrap();
    let request = rag_rat_sync::EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey: local.ed25519_public_key(),
        x25519_pubkey: local.x25519_public_key(),
        transport_node_id: *joiner_endpoint.id().as_bytes(),
        budget: rag_rat_oplog::enrollment_budget(joiner, account, NOW).unwrap(),
        held_entry_hashes: rag_rat_oplog::held_account_entry_hashes(joiner, account).unwrap(),
    };
    let server = rag_rat_sync::accept_enrollment(owner_endpoint, owner, || NOW);
    let client = rag_rat_sync::connect_and_enroll(
        joiner_endpoint,
        rag_rat_sync::endpoint_addr(owner_endpoint),
        joiner,
        account,
        &request,
        NOW,
    );
    let (server_result, client_result) = tokio::join!(server, client);
    server_result.unwrap();
    client_result.unwrap();
}

/// A device with an account restores it onto a fresh, empty peer over one session.
#[tokio::test]
async fn a_fresh_peer_restores_an_account_over_the_session() {
    // Source: a real account genesis (a signed, self-authorizing account entry).
    let source = fresh_db();
    let account_id = local_account(&source, NOW).unwrap();
    let source_entries = account_entries_for_sync(&source, account_id).unwrap();
    assert!(!source_entries.is_empty(), "the genesis is a real account entry to move");

    // Destination: schema only, no account.
    let dest = fresh_db();
    assert!(
        account_entries_for_sync(&dest, account_id).unwrap().is_empty(),
        "the destination starts empty for this account",
    );

    let mut source_store = OplogSyncStore::new(&source, account_id, || NOW);
    let mut dest_store = OplogSyncStore::new(&dest, account_id, || NOW);

    let (a_send, b_recv) = tokio::io::duplex(1 << 20);
    let (b_send, a_recv) = tokio::io::duplex(1 << 20);
    let (source_report, dest_report) = tokio::join!(
        run_session(
            &mut source_store,
            a_send,
            a_recv,
            AuthRole::Acceptor,
            SessionCapabilities::bidirectional(),
        ),
        run_session(
            &mut dest_store,
            b_send,
            b_recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
        ),
    );
    let source_report = source_report.unwrap();
    let dest_report = dest_report.unwrap();

    assert_eq!(source_report.entries_sent, source_entries.len(), "source streamed its entries");
    assert_eq!(
        dest_report.entries_newly_stored,
        source_entries.len(),
        "the destination stored every entry it received",
    );

    // The destination now holds byte-identical account entries — restore-from-peer.
    let dest_entries = account_entries_for_sync(&dest, account_id).unwrap();
    let source_bytes: Vec<Vec<u8>> =
        source_entries.iter().map(|e| e.signed_bytes.clone()).collect();
    let dest_bytes: Vec<Vec<u8>> = dest_entries.iter().map(|e| e.signed_bytes.clone()).collect();
    assert_eq!(dest_bytes, source_bytes, "the account is byte-identical on the fresh peer");
}

/// Two peers already holding the same account transfer nothing — the diff is empty.
#[tokio::test]
async fn two_peers_in_sync_transfer_nothing() {
    let a = fresh_db();
    let account_id = local_account(&a, NOW).unwrap();
    let entries = account_entries_for_sync(&a, account_id).unwrap();

    // Seed b with the same entries by ingesting a's, so both hold the identical set.
    let b = fresh_db();
    {
        let mut b_store = OplogSyncStore::new(&b, account_id, || NOW);
        for e in &entries {
            use rag_rat_sync::SyncStore;
            b_store.ingest(&e.signed_bytes).unwrap();
        }
    }
    assert_eq!(account_entries_for_sync(&b, account_id).unwrap().len(), entries.len());

    let mut a_store = OplogSyncStore::new(&a, account_id, || NOW);
    let mut b_store = OplogSyncStore::new(&b, account_id, || NOW);
    let (a_send, b_recv) = tokio::io::duplex(1 << 16);
    let (b_send, a_recv) = tokio::io::duplex(1 << 16);
    let (ra, rb) = tokio::join!(
        run_session(
            &mut a_store,
            a_send,
            a_recv,
            AuthRole::Acceptor,
            SessionCapabilities::bidirectional(),
        ),
        run_session(
            &mut b_store,
            b_send,
            b_recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
        ),
    );
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    assert_eq!(ra.entries_newly_stored, 0);
    assert_eq!(rb.entries_newly_stored, 0);
    assert_eq!(ra.entries_sent, 0, "nothing to send when already in sync");
    assert_eq!(rb.entries_sent, 0);
    let _ = AccountId::from_bytes(account_id.to_bytes()); // account_id API is public
}

/// A device restores its own `/3` CONTENT — the memories themselves — onto a fresh peer, after the
/// account log that authorizes them has synced. Load-bearing test for D3: content moves through the
/// same session machinery as the account log, feeding `OplogContentSyncStore`, and the moved bytes
/// fold accepted on the peer once authority is in place.
#[tokio::test]
async fn a_fresh_peer_restores_the_accounts_content_after_the_account_log() {
    use rag_rat_oplog::{
        ContentRefoldBudget, MemoryOp, NodeContent, NodeId, SealPolicy, author_content_batch,
        content_entries_for_sync, ensure_owned_stream_v2_in_tx, settle_pending_content_refolds,
    };
    use rag_rat_sync::OplogContentSyncStore;
    use rusqlite::{Transaction, TransactionBehavior};

    // Source: an account, an owned stream, and two authored content ops (the memories).
    let source = fresh_db();
    let account_id = local_account(&source, NOW).unwrap();
    let stream = {
        let tx = Transaction::new_unchecked(&source, TransactionBehavior::Immediate).unwrap();
        let s = ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW).unwrap();
        tx.commit().unwrap();
        s
    };
    let node = |id: &str, title: &str| MemoryOp::NodeCreate {
        node_id: NodeId::from(id),
        content: NodeContent {
            kind: "Invariant".into(),
            title: title.into(),
            body: "body".into(),
            confidence: "high".into(),
            source: "agent".into(),
            tags: Vec::new(),
            payload: None,
        },
    };
    author_content_batch(
        &source,
        stream,
        &[node("n1", "first"), node("n2", "second")],
        SealPolicy::Plaintext,
        NOW,
    )
    .unwrap();
    let source_content = content_entries_for_sync(&source, account_id).unwrap();
    assert_eq!(source_content.len(), 2, "the source authored two content entries to move");

    let dest = fresh_db();

    // 1) Account log first — the roster + stream ownership that authorize content acceptance. A
    //    content session run before this would still transfer the bytes, but they would park until
    //    authority arrived; restore runs the logs in dependency order.
    {
        let mut src = OplogSyncStore::new(&source, account_id, || NOW);
        let mut dst = OplogSyncStore::new(&dest, account_id, || NOW);
        let (a_send, b_recv) = tokio::io::duplex(1 << 20);
        let (b_send, a_recv) = tokio::io::duplex(1 << 20);
        let (ra, rb) = tokio::join!(
            run_session(
                &mut src,
                a_send,
                a_recv,
                AuthRole::Acceptor,
                SessionCapabilities::bidirectional(),
            ),
            run_session(
                &mut dst,
                b_send,
                b_recv,
                AuthRole::Dialer,
                SessionCapabilities::bidirectional(),
            ),
        );
        ra.unwrap();
        rb.unwrap();
    }

    // 2) Content — the memories.
    let dest_report = {
        let mut src = OplogContentSyncStore::new(&source, account_id, || NOW);
        let mut dst = OplogContentSyncStore::new(&dest, account_id, || NOW);
        let (a_send, b_recv) = tokio::io::duplex(1 << 20);
        let (b_send, a_recv) = tokio::io::duplex(1 << 20);
        let (ra, rb) = tokio::join!(
            run_session(
                &mut src,
                a_send,
                a_recv,
                AuthRole::Acceptor,
                SessionCapabilities::bidirectional(),
            ),
            run_session(
                &mut dst,
                b_send,
                b_recv,
                AuthRole::Dialer,
                SessionCapabilities::bidirectional(),
            ),
        );
        ra.unwrap();
        rb.unwrap()
    };
    assert_eq!(
        dest_report.entries_newly_stored,
        source_content.len(),
        "every content entry landed on the fresh peer",
    );

    // The fresh peer holds byte-identical content — restore-from-peer.
    let dest_content = content_entries_for_sync(&dest, account_id).unwrap();
    let mut source_bytes: Vec<Vec<u8>> =
        source_content.iter().map(|e| e.signed_bytes.clone()).collect();
    let mut dest_bytes: Vec<Vec<u8>> =
        dest_content.iter().map(|e| e.signed_bytes.clone()).collect();
    source_bytes.sort();
    dest_bytes.sort();
    assert_eq!(
        dest_bytes, source_bytes,
        "the account's content is byte-identical on the fresh peer"
    );

    // And the moved bytes are usable: once the deferred refold settles, the peer accepts them —
    // acceptance the peer recomputes from the synced authority, never trusting the sender.
    settle_pending_content_refolds(&dest, &ContentRefoldBudget::unbounded(), NOW).unwrap();
    let accepted: i64 = dest
        .query_row(
            "SELECT count(*) FROM content_entries WHERE author_account_id = ?1 AND accepted = 1",
            [account_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(accepted, 2, "the restored content folds accepted once authority is in place");
}

/// A content entry authored by a DIFFERENT account than the session is scoped to must not be stored
/// — the account-scoped content store refuses it before ingest, so a peer cannot flood this
/// account's pre-verify table with foreign candidates through a session that never named them.
#[tokio::test]
async fn foreign_account_content_is_not_stored() {
    use rag_rat_oplog::{
        MemoryOp, NodeContent, NodeId, SealPolicy, author_content_batch, content_entries_for_sync,
        ensure_owned_stream_v2_in_tx,
    };
    use rag_rat_sync::{OplogContentSyncStore, SyncStore};
    use rusqlite::{Transaction, TransactionBehavior};

    // Another account authors real content in its own DB.
    let other = fresh_db();
    let other_account = local_account(&other, NOW).unwrap();
    let other_stream = {
        let tx = Transaction::new_unchecked(&other, TransactionBehavior::Immediate).unwrap();
        let s = ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW).unwrap();
        tx.commit().unwrap();
        s
    };
    author_content_batch(
        &other,
        other_stream,
        &[MemoryOp::NodeCreate {
            node_id: NodeId::from("n1"),
            content: NodeContent {
                kind: "Invariant".into(),
                title: "t".into(),
                body: "body".into(),
                confidence: "high".into(),
                source: "agent".into(),
                tags: Vec::new(),
                payload: None,
            },
        }],
        SealPolicy::Plaintext,
        NOW,
    )
    .unwrap();
    let foreign = content_entries_for_sync(&other, other_account).unwrap()[0].signed_bytes.clone();

    // A content store scoped to MY account is handed the OTHER account's content.
    let mine = fresh_db();
    let my_account = local_account(&mine, NOW).unwrap();
    assert_ne!(my_account.to_bytes(), other_account.to_bytes());
    let mut store = OplogContentSyncStore::new(&mine, my_account, || NOW);
    assert_eq!(
        store.ingest(&foreign).unwrap(),
        rag_rat_sync::Ingested::NoChange,
        "foreign-account content is refused before ingest",
    );
    assert!(
        content_entries_for_sync(&mine, other_account).unwrap().is_empty(),
        "the other account was not grown through my session",
    );
}

/// The op-log store's [`NodeAuth`] wiring works end to end (#881): a store authorizes a binding it
/// minted for a transport node, and refuses the same binding presented from any other node — the
/// replay defense, verified through the real oplog sign/verify seams (not the fake used by the
/// protocol tests).
#[test]
fn a_store_authorizes_its_own_binding_but_not_from_another_node() {
    use rag_rat_sync::NodeAuth;

    let db = fresh_db();
    let account = local_account(&db, NOW).unwrap();
    let store = OplogSyncStore::new(&db, account, || NOW);

    let node = [4u8; 32];
    let local = store.local_auth(&node, NOW).unwrap();
    assert!(!local.binding.is_empty(), "a store with a local device mints a real binding");
    assert_eq!(local.capability, PeerCapability::ReadWrite);
    assert_eq!(
        store.authorize(&local.binding, &node, NOW).unwrap(),
        PeerAuthorization::Granted(PeerCapability::ReadWrite),
        "its owner role grants read-write access",
    );
    assert_eq!(
        store.authorize(&local.binding, &[5u8; 32], NOW).unwrap(),
        PeerAuthorization::Rejected,
        "the same binding from a different node id is refused",
    );
}

/// Auth freshness tracks the per-handshake clock, not the store's construction time (#881): a
/// long-lived store constructed long ago still mints and verifies a binding at the current
/// handshake time. Were the construction time reused, a binding minted "now" would read as
/// implausibly far in the future and be refused.
#[test]
fn a_store_authorizes_against_the_handshake_clock_not_its_construction_time() {
    use rag_rat_sync::NodeAuth;

    let db = fresh_db();
    let account = local_account(&db, NOW).unwrap();
    let stale = OplogSyncStore::new(&db, account, || NOW); // constructed with an old clock
    let node = [4u8; 32];

    let much_later = NOW + 10 * 24 * 60 * 60 * 1000; // ten days after construction
    let local = stale.local_auth(&node, much_later).unwrap();
    assert_eq!(
        stale.authorize(&local.binding, &node, much_later).unwrap(),
        PeerAuthorization::Granted(PeerCapability::ReadWrite),
        "a long-lived store still authorizes using the current handshake time",
    );
}

/// A store with no local account (a fresh onboarding peer) presents an empty, anonymous binding and
/// authorizes nothing — so it can complete an `Open` handshake but never passes `Closed`.
#[test]
fn a_store_without_an_account_presents_an_empty_binding_and_authorizes_nothing() {
    use rag_rat_sync::NodeAuth;

    let owner = fresh_db();
    let account = local_account(&owner, NOW).unwrap();
    let node = [1u8; 32];
    let owner_binding =
        OplogSyncStore::new(&owner, account, || NOW).local_auth(&node, NOW).unwrap().binding;
    let stale_binding = OplogSyncStore::new(&owner, account, || NOW)
        .local_auth(&node, NOW - 24 * 60 * 60 * 1000 - 1)
        .unwrap()
        .binding;
    let future_binding = OplogSyncStore::new(&owner, account, || NOW)
        .local_auth(&node, NOW + 60 * 60 * 1000 + 1)
        .unwrap()
        .binding;
    let db = fresh_db(); // schema only, no account authority
    let store = OplogSyncStore::new(&db, account, || NOW);
    let local = store.local_auth(&node, NOW).unwrap();
    assert!(local.binding.is_empty(), "no local device => an anonymous (empty) binding");
    assert_eq!(local.capability, PeerCapability::ReadOnly);
    assert_eq!(
        store.authorize(&owner_binding, &node, NOW).unwrap(),
        PeerAuthorization::Unavailable,
        "a valid binding cannot be decided until account authority is restored",
    );
    assert_eq!(
        store.authorize(&stale_binding, &node, NOW).unwrap(),
        PeerAuthorization::Rejected,
        "an expired binding is rejected before the unavailable-roster classification",
    );
    assert_eq!(
        store.authorize(&future_binding, &node, NOW).unwrap(),
        PeerAuthorization::Rejected,
        "a future-dated binding is rejected before the unavailable-roster classification",
    );
    assert_eq!(
        store.authorize(&[0u8; 10], &node, NOW).unwrap(),
        PeerAuthorization::Rejected,
        "a malformed binding is rejected rather than gaining bootstrap capability",
    );
}

/// The content store carries the same account-level node authorization as the account-log store —
/// the binding is about the account + transport node, not the payload.
#[test]
fn the_content_store_carries_the_same_node_auth() {
    use rag_rat_sync::{NodeAuth, OplogContentSyncStore};

    let db = fresh_db();
    let account = local_account(&db, NOW).unwrap();
    let store = OplogContentSyncStore::new(&db, account, || NOW);
    let node = [4u8; 32];
    let local = store.local_auth(&node, NOW).unwrap();
    assert_eq!(
        store.authorize(&local.binding, &node, NOW).unwrap(),
        PeerAuthorization::Granted(PeerCapability::ReadWrite),
        "own node authorized",
    );
    assert_eq!(
        store.authorize(&local.binding, &[5u8; 32], NOW).unwrap(),
        PeerAuthorization::Rejected,
        "other node refused",
    );
}

/// LIVE: the real iroh endpoint, dialed peer-to-peer over the configured relay. Ignored by default
/// (needs network + the relay); run with `--ignored` to exercise the actual transport rather than
/// the in-process duplex the other tests use.
#[tokio::test]
#[ignore = "live: binds iroh endpoints and dials over the relay"]
async fn a_real_iroh_round_trip_restores_an_account() {
    use rag_rat_oplog::ensure_owned_stream_v2_in_tx;
    use rusqlite::{Transaction, TransactionBehavior};

    let relay = std::env::var("RAG_RAT_SYNC_RELAY").expect("set RAG_RAT_SYNC_RELAY to run this");

    let source = fresh_db();
    let account_id = local_account(&source, NOW).unwrap();
    let dest = fresh_db();

    let listener = rag_rat_sync::build_endpoint([1u8; 32], &relay).await.unwrap();
    let dialer = rag_rat_sync::build_endpoint([2u8; 32], &relay).await.unwrap();
    let listener_addr = rag_rat_sync::endpoint_addr(&listener);
    enroll_member_over_endpoint(&listener, &dialer, &source, &dest, account_id, &relay).await;

    // Author one account-log entry after enrollment so the enrolled peer has something to pull.
    let tx = Transaction::new_unchecked(&source, TransactionBehavior::Immediate).unwrap();
    ensure_owned_stream_v2_in_tx(&tx, "live-restore", NOW).unwrap();
    tx.commit().unwrap();
    let want = account_entries_for_sync(&source, account_id).unwrap();

    let mut source_store = OplogSyncStore::new(&source, account_id, || NOW);
    let mut dest_store = OplogSyncStore::new(&dest, account_id, || NOW);
    let policy = rag_rat_sync::AuthPolicy::Closed;
    let server =
        async { rag_rat_sync::accept_and_sync(&listener, &mut source_store, policy, || NOW).await };
    let client = async {
        rag_rat_sync::connect_and_sync(
            &dialer,
            listener_addr,
            rag_rat_sync::SYNC_ALPN,
            &mut dest_store,
            policy,
            NOW,
        )
        .await
    };
    let (server_r, client_r) = tokio::join!(server, client);
    server_r.unwrap();
    client_r.unwrap();

    let got = account_entries_for_sync(&dest, account_id).unwrap();
    assert_eq!(got.len(), want.len(), "the account restored over real iroh transport");
}

/// LIVE: content (`/3`) restores over the real iroh transport via ALPN dispatch (#907). One
/// endpoint binds both ALPNs; the dialer syncs the account log (`SYNC_ALPN`) then content
/// (`CONTENT_SYNC_ALPN`), and `accept_and_dispatch` routes each connection to the matching store.
#[tokio::test]
#[ignore = "live: binds iroh endpoints and dials over the relay"]
async fn a_real_iroh_round_trip_restores_content_via_alpn_dispatch() {
    use rag_rat_oplog::{
        MemoryOp, NodeContent, NodeId, SealPolicy, author_content_batch, content_entries_for_sync,
        ensure_owned_stream_v2_in_tx,
    };
    use rag_rat_sync::{CONTENT_SYNC_ALPN, OplogContentSyncStore, SYNC_ALPN};
    use rusqlite::{Transaction, TransactionBehavior};

    let relay = std::env::var("RAG_RAT_SYNC_RELAY").expect("set RAG_RAT_SYNC_RELAY to run this");

    // Source: an account, an owned stream, two authored content entries.
    let source = fresh_db();
    let account_id = local_account(&source, NOW).unwrap();
    let stream = {
        let tx = Transaction::new_unchecked(&source, TransactionBehavior::Immediate).unwrap();
        let s = ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW).unwrap();
        tx.commit().unwrap();
        s
    };
    let node = |id: &str, title: &str| MemoryOp::NodeCreate {
        node_id: NodeId::from(id),
        content: NodeContent {
            kind: "Invariant".into(),
            title: title.into(),
            body: "body".into(),
            confidence: "high".into(),
            source: "agent".into(),
            tags: Vec::new(),
            payload: None,
        },
    };
    author_content_batch(
        &source,
        stream,
        &[node("n1", "first"), node("n2", "second")],
        SealPolicy::Plaintext,
        NOW,
    )
    .unwrap();
    let want = content_entries_for_sync(&source, account_id).unwrap();
    assert_eq!(want.len(), 2, "the source authored two content entries to move");

    let dest = fresh_db();
    let listener = rag_rat_sync::build_endpoint([3u8; 32], &relay).await.unwrap();
    let dialer = rag_rat_sync::build_endpoint([4u8; 32], &relay).await.unwrap();
    let listener_addr = rag_rat_sync::endpoint_addr(&listener);
    enroll_member_over_endpoint(&listener, &dialer, &source, &dest, account_id, &relay).await;
    let policy = rag_rat_sync::AuthPolicy::Closed;

    // Account log FIRST — carries the roster + stream ownership that authorize content acceptance.
    {
        let mut src_account = OplogSyncStore::new(&source, account_id, || NOW);
        let mut src_content = OplogContentSyncStore::new(&source, account_id, || NOW);
        let mut dst_account = OplogSyncStore::new(&dest, account_id, || NOW);
        let server = async {
            rag_rat_sync::accept_and_dispatch(
                &listener,
                &mut src_account,
                &mut src_content,
                policy,
                || NOW,
            )
            .await
        };
        let client = async {
            rag_rat_sync::connect_and_sync(
                &dialer,
                listener_addr.clone(),
                SYNC_ALPN,
                &mut dst_account,
                policy,
                NOW,
            )
            .await
        };
        let (server_r, client_r) = tokio::join!(server, client);
        let (alpn, _) = server_r.unwrap();
        assert_eq!(alpn, SYNC_ALPN, "the account-log connection routes to the account store");
        client_r.unwrap();
    }

    // Content next — accepted now that its authority is present on the dest.
    {
        let mut src_account = OplogSyncStore::new(&source, account_id, || NOW);
        let mut src_content = OplogContentSyncStore::new(&source, account_id, || NOW);
        let mut dst_content = OplogContentSyncStore::new(&dest, account_id, || NOW);
        let server = async {
            rag_rat_sync::accept_and_dispatch(
                &listener,
                &mut src_account,
                &mut src_content,
                policy,
                || NOW,
            )
            .await
        };
        let client = async {
            rag_rat_sync::connect_and_sync(
                &dialer,
                listener_addr,
                CONTENT_SYNC_ALPN,
                &mut dst_content,
                policy,
                NOW,
            )
            .await
        };
        let (server_r, client_r) = tokio::join!(server, client);
        let (alpn, _) = server_r.unwrap();
        assert_eq!(alpn, CONTENT_SYNC_ALPN, "the content connection routes to the content store");
        client_r.unwrap();
    }

    let got = content_entries_for_sync(&dest, account_id).unwrap();
    assert_eq!(
        got.len(),
        want.len(),
        "content restored over real iroh transport via ALPN dispatch"
    );
}

/// Two loopback endpoints with NO relay (`RelayMode::Disabled`), reachable only by their direct
/// 127.0.0.1 socket address — a relay-free transport so the pairing drill runs in CI, unlike the
/// `#[ignore]` live tests that dial over a real relay.
async fn loopback_endpoints() -> (iroh::Endpoint, iroh::Endpoint) {
    use rag_rat_sync::{CONTENT_SYNC_ALPN, ENROLL_ALPN, SYNC_ALPN, TABLE_SYNC_ALPN};
    let bind = |seed: [u8; 32]| async move {
        iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .alpns(vec![
                SYNC_ALPN.to_vec(),
                CONTENT_SYNC_ALPN.to_vec(),
                TABLE_SYNC_ALPN.to_vec(),
                ENROLL_ALPN.to_vec(),
            ])
            .relay_mode(iroh::RelayMode::Disabled)
            .secret_key(iroh::SecretKey::from_bytes(&seed))
            .bind()
            .await
            .unwrap()
    };
    (bind([0x31; 32]).await, bind([0x32; 32]).await)
}

/// A relay-free, directly dialable address for a loopback endpoint (its 127.0.0.1 socket).
fn direct_addr(endpoint: &iroh::Endpoint) -> iroh::EndpointAddr {
    let port = endpoint
        .addr()
        .ip_addrs()
        .next()
        .expect("a bound endpoint advertises at least one socket address")
        .port();
    iroh::EndpointAddr::new(endpoint.id())
        .with_ip_addr(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
}

#[tokio::test]
async fn production_anchors_replicate_through_dispatch_before_local_repo_registration() {
    use rag_rat_sync::{
        AuthPolicy, OplogContentSyncStore, OplogTableSyncStore, TABLE_SYNC_ALPN,
        accept_and_dispatch, connect_and_table_sync,
    };

    let owner = fresh_db();
    let account = local_account(&owner, NOW).unwrap();
    let joiner = fresh_db();
    let (owner_endpoint, joiner_endpoint) = loopback_endpoints().await;
    enroll_member_over_endpoint(
        &owner_endpoint,
        &joiner_endpoint,
        &owner,
        &joiner,
        account,
        "https://relay.example",
    )
    .await;

    owner
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
             VALUES ('repo-a', 'repo-a', 0)",
            [],
        )
        .unwrap();
    rag_rat_oplog::ensure_repo_incarnation(&owner, "repo-a", NOW + 1).unwrap().unwrap();
    owner
        .execute(
            "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 anchor_status, created_at_ms)
             VALUES ('repo-a', 'memory-a', 'path', 'src/lib.rs', 'src/lib.rs', 4, 5,
                     'current', ?1)",
            [NOW + 2],
        )
        .unwrap();
    for entry in account_entries_for_sync(&owner, account).unwrap() {
        rag_rat_oplog::account_ingest(&joiner, &entry.signed_bytes, NOW + 2).unwrap();
    }

    let mut account_store = OplogSyncStore::new(&owner, account, || NOW);
    let mut content_store = OplogContentSyncStore::new(&owner, account, || NOW);
    let mut table_store = OplogTableSyncStore::new(&joiner, account, || NOW);
    assert!(table_store.has_streams().unwrap());
    let server = accept_and_dispatch(
        &owner_endpoint,
        &mut account_store,
        &mut content_store,
        AuthPolicy::Closed,
        || NOW,
    );
    let client = connect_and_table_sync(
        &joiner_endpoint,
        direct_addr(&owner_endpoint),
        &mut table_store,
        NOW,
    );
    let (server, client) = tokio::join!(server, client);
    let (alpn, server_report) = server.unwrap();
    let client_report = client.unwrap();
    assert_eq!(alpn, TABLE_SYNC_ALPN);
    assert_eq!(server_report.entries_sent, 1);
    assert_eq!(client_report.entries_newly_stored, 1);
    let replicated: bool = joiner
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'
                   AND path = 'src/lib.rs')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(replicated);
}

#[tokio::test]
async fn production_overlay_replicates_summaries_and_verdicts() {
    use rag_rat_sync::{
        AuthPolicy, OplogContentSyncStore, OplogTableSyncStore, TABLE_SYNC_ALPN,
        accept_and_dispatch, connect_and_table_sync,
    };

    let owner = fresh_db();
    let account = local_account(&owner, NOW).unwrap();
    let joiner = fresh_db();
    let (owner_endpoint, joiner_endpoint) = loopback_endpoints().await;
    enroll_member_over_endpoint(
        &owner_endpoint,
        &joiner_endpoint,
        &owner,
        &joiner,
        account,
        "https://relay.example",
    )
    .await;

    owner
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
             VALUES ('repo-a', 'repo-a', 0)",
            [],
        )
        .unwrap();
    rag_rat_oplog::ensure_repo_incarnation(&owner, "repo-a", NOW + 1).unwrap().unwrap();
    // Regenerable dream output: one verdict (memory_reality) and one summary (memory_summaries).
    owner
        .execute(
            "INSERT INTO memory_reality(
                 memory_id, repo_id, content_hash, verdict, direction, checked_inputs_hash,
                 evidence_json, model_id, prompt_version, checked_at_ms)
             VALUES ('memory-a', 'repo-a', 'hash-a', 'confirmed', 'note_ahead', 'inputs-a',
                     '[]', 'model-x', 'v1', ?1)",
            [NOW + 2],
        )
        .unwrap();
    owner
        .execute(
            "INSERT INTO memory_summaries(
                 memory_id, repo_id, content_hash, summary, model_id, prompt_version,
                 generated_at_ms)
             VALUES ('memory-a', 'repo-a', 'hash-a', 'A concise summary.', 'model-x', 'v1', ?1)",
            [NOW + 2],
        )
        .unwrap();
    for entry in account_entries_for_sync(&owner, account).unwrap() {
        rag_rat_oplog::account_ingest(&joiner, &entry.signed_bytes, NOW + 2).unwrap();
    }
    // The joiner is actively working in repo-a, so the apply-side lane bump has a registered repo
    // to advance (the bump is gated on registration to avoid phantom rows on a peer that only
    // relays).
    joiner
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
             VALUES ('repo-a', 'repo-a', 0)",
            [],
        )
        .unwrap();

    let mut account_store = OplogSyncStore::new(&owner, account, || NOW);
    let mut content_store = OplogContentSyncStore::new(&owner, account, || NOW);
    let mut table_store = OplogTableSyncStore::new(&joiner, account, || NOW);
    let server = accept_and_dispatch(
        &owner_endpoint,
        &mut account_store,
        &mut content_store,
        AuthPolicy::Closed,
        || NOW,
    );
    let client = connect_and_table_sync(
        &joiner_endpoint,
        direct_addr(&owner_endpoint),
        &mut table_store,
        NOW,
    );
    let (server, client) = tokio::join!(server, client);
    let (alpn, server_report) = server.unwrap();
    let client_report = client.unwrap();
    assert_eq!(alpn, TABLE_SYNC_ALPN);
    assert_eq!(server_report.entries_sent, 2, "one verdict and one summary");
    assert_eq!(client_report.entries_newly_stored, 2);

    let verdict: String = joiner
        .query_row(
            "SELECT verdict FROM memory_reality WHERE repo_id = 'repo-a' AND memory_id = \
             'memory-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(verdict, "confirmed");
    let summary: String = joiner
        .query_row(
            "SELECT summary FROM memory_summaries
             WHERE repo_id = 'repo-a' AND memory_id = 'memory-a' AND content_hash = 'hash-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(summary, "A concise summary.");
    // The apply advanced the memories Lens lane on the receiver, so the synced rows surface in Lens
    // without a local dream pass.
    let lane: i64 = joiner
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM repo_meta
             WHERE repo_id = 'repo-a' AND key = 'lens_memories_revision'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(lane >= 1, "applying overlay rows advances the memories lane");
}

#[tokio::test]
async fn a_pruned_overlay_summary_is_removed_on_the_peer() {
    use rag_rat_sync::{
        AuthPolicy, OplogContentSyncStore, OplogTableSyncStore, accept_and_dispatch,
        connect_and_table_sync,
    };

    let owner = fresh_db();
    let account = local_account(&owner, NOW).unwrap();
    let joiner = fresh_db();
    let (owner_endpoint, joiner_endpoint) = loopback_endpoints().await;
    enroll_member_over_endpoint(
        &owner_endpoint,
        &joiner_endpoint,
        &owner,
        &joiner,
        account,
        "https://relay.example",
    )
    .await;

    owner
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
             VALUES ('repo-a', 'repo-a', 0)",
            [],
        )
        .unwrap();
    rag_rat_oplog::ensure_repo_incarnation(&owner, "repo-a", NOW + 1).unwrap().unwrap();
    owner
        .execute(
            "INSERT INTO memory_summaries(
                 memory_id, repo_id, content_hash, summary, model_id, prompt_version,
                 generated_at_ms)
             VALUES ('memory-a', 'repo-a', 'hash-old', 'Stale summary.', 'model-x', 'v1', ?1)",
            [NOW + 2],
        )
        .unwrap();
    for entry in account_entries_for_sync(&owner, account).unwrap() {
        rag_rat_oplog::account_ingest(&joiner, &entry.signed_bytes, NOW + 2).unwrap();
    }

    let sync_once = async |owner: &_, joiner: &_| {
        let mut account_store = OplogSyncStore::new(owner, account, || NOW);
        let mut content_store = OplogContentSyncStore::new(owner, account, || NOW);
        let mut table_store = OplogTableSyncStore::new(joiner, account, || NOW);
        let server = accept_and_dispatch(
            &owner_endpoint,
            &mut account_store,
            &mut content_store,
            AuthPolicy::Closed,
            || NOW,
        );
        let client = connect_and_table_sync(
            &joiner_endpoint,
            direct_addr(&owner_endpoint),
            &mut table_store,
            NOW,
        );
        let (server, client) = tokio::join!(server, client);
        server.unwrap();
        client.unwrap();
    };

    sync_once(&owner, &joiner).await;
    let present: bool = joiner
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_summaries WHERE content_hash = 'hash-old')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(present, "the summary replicates on the first pass");

    // A newer note supersedes the old summary: the producer's prune deletes the old row locally,
    // and the next authoring pass emits a Remove that must delete it on the peer too.
    owner.execute("DELETE FROM memory_summaries WHERE content_hash = 'hash-old'", []).unwrap();
    sync_once(&owner, &joiner).await;
    let gone: bool = joiner
        .query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM memory_summaries WHERE content_hash = 'hash-old')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(gone, "the pruned summary is removed on the peer");
}

#[tokio::test]
async fn production_distill_records_replicate_and_regenerate() {
    use rag_rat_sync::{
        AuthPolicy, OplogContentSyncStore, OplogTableSyncStore, TABLE_SYNC_ALPN,
        accept_and_dispatch, connect_and_table_sync,
    };

    let owner = fresh_db();
    let account = local_account(&owner, NOW).unwrap();
    let joiner = fresh_db();
    let (owner_endpoint, joiner_endpoint) = loopback_endpoints().await;
    enroll_member_over_endpoint(
        &owner_endpoint,
        &joiner_endpoint,
        &owner,
        &joiner,
        account,
        "https://relay.example",
    )
    .await;

    for db in [&owner, &joiner] {
        db.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
             VALUES ('repo-a', 'repo-a', 0)",
            [],
        )
        .unwrap();
    }
    rag_rat_oplog::ensure_repo_incarnation(&owner, "repo-a", NOW + 1).unwrap().unwrap();
    // A distilled record for a closed issue thread; also seed a sibling repo's record to prove
    // cross-repo isolation (repo-b never rides repo-a's stream).
    let seed_record = |item_key: &str, repo: &str, hash: &str, cause: &str| {
        owner
            .execute(
                "INSERT INTO papertrail_distill
                     (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                      root_cause, fix_edge_source, thread_shape, distilled_at_ms, repo_id)
                 VALUES ('github', 'o/r', 'issue', ?1, ?2, 2, ?3, 'provider', 'investigation',
                         ?4, ?5)",
                rusqlite::params![item_key, hash, cause, NOW + 2, repo],
            )
            .unwrap();
    };
    seed_record("7", "repo-a", "sha256:in-a", "the original cause");
    // An enrichment edge and a rejected alternative on the same repo-a thread — the child tables.
    owner
        .execute(
            "INSERT INTO papertrail_distill_edges
                 (tracker, project, src_item_kind, src_item_key, dst_item_kind, dst_item_key,
                  edge_kind, created_at_ms, repo_id)
             VALUES ('github', 'o/r', 'issue', '7', 'change_request', '8', 'coalesced', ?1,
                     'repo-a')",
            [NOW + 2],
        )
        .unwrap();
    owner
        .execute(
            "INSERT INTO papertrail_distill_alternatives
                 (tracker, project, item_kind, item_key, ordinal, alternative, reason, repo_id)
             VALUES ('github', 'o/r', 'issue', '7', 0, 'revert instead', 'loses the fix', 'repo-a')",
            [],
        )
        .unwrap();
    owner
        .execute(
            "INSERT INTO papertrail_distill_record_commits
                 (tracker, project, item_kind, item_key, commit_sha, created_at_ms, repo_id)
             VALUES ('github', 'o/r', 'issue', '7', 'fixsha1', ?1, 'repo-a')",
            [NOW + 2],
        )
        .unwrap();
    owner
        .execute(
            "INSERT INTO papertrail_distill_evidence
                 (tracker, project, item_kind, item_key, ordinal, field, source_kind, source_id,
                  byte_start, byte_end, quote, repo_id)
             VALUES ('github', 'o/r', 'issue', '7', 0, 'root_cause', 'item', '7', 0, 5,
                     'the exact cause quote', 'repo-a')",
            [],
        )
        .unwrap();
    owner
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
             VALUES ('repo-b', 'repo-b', 0)",
            [],
        )
        .unwrap();
    seed_record("9", "repo-b", "sha256:in-b", "a sibling repo cause");

    for entry in account_entries_for_sync(&owner, account).unwrap() {
        rag_rat_oplog::account_ingest(&joiner, &entry.signed_bytes, NOW + 2).unwrap();
    }

    // Whole-row LWW resolves by lamport (authoring increments it), not wall-clock, so both passes
    // use the same clock — the regeneration below still wins on its higher authored lamport.
    let reconcile = async || {
        let mut account_store = OplogSyncStore::new(&owner, account, || NOW);
        let mut content_store = OplogContentSyncStore::new(&owner, account, || NOW);
        let mut table_store = OplogTableSyncStore::new(&joiner, account, || NOW);
        let server = accept_and_dispatch(
            &owner_endpoint,
            &mut account_store,
            &mut content_store,
            AuthPolicy::Closed,
            || NOW,
        );
        let client = connect_and_table_sync(
            &joiner_endpoint,
            direct_addr(&owner_endpoint),
            &mut table_store,
            NOW,
        );
        let (server, client) = tokio::join!(server, client);
        let (alpn, _) = server.unwrap();
        client.unwrap();
        assert_eq!(alpn, TABLE_SYNC_ALPN);
    };

    reconcile().await;
    let cause: String = joiner
        .query_row(
            "SELECT root_cause FROM papertrail_distill
             WHERE repo_id = 'repo-a' AND item_kind = 'issue' AND item_key = '7'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cause, "the original cause", "the distilled record replicates");
    let edge_kind: String = joiner
        .query_row(
            "SELECT edge_kind FROM papertrail_distill_edges
             WHERE repo_id = 'repo-a' AND src_item_key = '7' AND dst_item_key = '8'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(edge_kind, "coalesced", "the distill edge child replicates");
    let (alternative, reason): (String, String) = joiner
        .query_row(
            "SELECT alternative, reason FROM papertrail_distill_alternatives
             WHERE repo_id = 'repo-a' AND item_key = '7' AND ordinal = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(alternative, "revert instead", "the distill alternative child replicates");
    assert_eq!(reason, "loses the fix", "the alternative's nullable reason column replicates too");
    let commit_sha: String = joiner
        .query_row(
            "SELECT commit_sha FROM papertrail_distill_record_commits
             WHERE repo_id = 'repo-a' AND item_key = '7'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(commit_sha, "fixsha1", "the distill record_commits child replicates");
    let quote: String = joiner
        .query_row(
            "SELECT quote FROM papertrail_distill_evidence
             WHERE repo_id = 'repo-a' AND item_key = '7' AND ordinal = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(quote, "the exact cause quote", "the distill evidence child replicates");
    // repo-a advertises no route for repo-b's incarnation, so repo-b's record never lands.
    let sibling: bool = joiner
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM papertrail_distill WHERE repo_id = 'repo-b')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!sibling, "a sibling repo's distilled record never crosses repo-a's stream");

    // Regeneration: the record is re-distilled (new distill_input_hash + fields). A whole-row LWW
    // upsert on the stable natural key replaces it on the peer.
    owner
        .execute(
            "UPDATE papertrail_distill
             SET distill_input_hash = 'sha256:in-a2', root_cause = 'the regenerated cause'
             WHERE repo_id = 'repo-a' AND item_key = '7'",
            [],
        )
        .unwrap();
    reconcile().await;
    let regenerated: String = joiner
        .query_row(
            "SELECT root_cause FROM papertrail_distill
             WHERE repo_id = 'repo-a' AND item_kind = 'issue' AND item_key = '7'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(regenerated, "the regenerated cause", "regeneration upserts the record on the peer");
}

#[tokio::test]
async fn a_fresh_peer_converges_through_a_compacted_chain_via_the_advertised_floor() {
    use rag_rat_sync::{
        AuthPolicy, OplogContentSyncStore, OplogTableSyncStore, TABLE_SYNC_ALPN,
        accept_and_dispatch, connect_and_table_sync,
    };

    let owner = fresh_db();
    let account = local_account(&owner, NOW).unwrap();
    let joiner = fresh_db();
    let (owner_endpoint, joiner_endpoint) = loopback_endpoints().await;
    enroll_member_over_endpoint(
        &owner_endpoint,
        &joiner_endpoint,
        &owner,
        &joiner,
        account,
        "https://relay.example",
    )
    .await;

    owner
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
             VALUES ('repo-a', 'repo-a', 0)",
            [],
        )
        .unwrap();
    rag_rat_oplog::ensure_repo_incarnation(&owner, "repo-a", NOW + 1).unwrap().unwrap();
    for (memory, path) in [("memory-a", "src/a.rs"), ("memory-b", "src/b.rs")] {
        owner
            .execute(
                "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                     anchor_status, created_at_ms)
                 VALUES ('repo-a', ?1, 'path', ?2, ?2, 4, 5, 'current', ?3)",
                rusqlite::params![memory, path, NOW + 2],
            )
            .unwrap();
        rag_rat_oplog::table_sync_author_pending(&owner, account, NOW + 2).unwrap();
    }
    // Compact the owner's chain below all but its last entry: the first binding's winning entry
    // drops, and only the floor advertisement lets a fresh peer converge.
    let compacted = rag_rat_oplog::table_sync_compact_overdue(&owner, account, NOW + 3, &|scope| {
        (scope == "anchors/1").then_some(1)
    })
    .unwrap();
    assert_eq!(compacted, 1, "the first binding's entry is reclaimed");
    for entry in account_entries_for_sync(&owner, account).unwrap() {
        rag_rat_oplog::account_ingest(&joiner, &entry.signed_bytes, NOW + 2).unwrap();
    }

    let mut account_store = OplogSyncStore::new(&owner, account, || NOW);
    let mut content_store = OplogContentSyncStore::new(&owner, account, || NOW);
    let mut table_store = OplogTableSyncStore::new(&joiner, account, || NOW);
    let server = accept_and_dispatch(
        &owner_endpoint,
        &mut account_store,
        &mut content_store,
        AuthPolicy::Closed,
        || NOW,
    );
    let client = connect_and_table_sync(
        &joiner_endpoint,
        direct_addr(&owner_endpoint),
        &mut table_store,
        NOW,
    );
    let (server, client) = tokio::join!(server, client);
    let (alpn, _server_report) = server.unwrap();
    let client_report = client.unwrap();
    assert_eq!(alpn, TABLE_SYNC_ALPN);
    assert_eq!(
        client_report.entries_newly_stored, 1,
        "only the retained suffix transfers — the floor entry roots the fresh chain",
    );

    let surviving: bool = joiner
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-b')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(surviving, "the row whose entry is at/above the floor converges");
    let compacted_row: bool = joiner
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!compacted_row, "the reclaimed prefix honestly does not arrive");
    let floor: Option<i64> = joiner
        .query_row("SELECT lamport FROM table_sync_retained_floors", [], |row| row.get(0))
        .ok();
    assert_eq!(floor, Some(1), "the adopted floor is recorded, so re-offers propagate it");
}

#[tokio::test]
async fn closed_table_auth_refusal_reveals_no_manifest() {
    use rag_rat_sync::{AuthPolicy, Frame, OplogContentSyncStore, TABLE_SYNC_ALPN};
    use tokio::io::AsyncReadExt;

    let owner = fresh_db();
    let account = local_account(&owner, NOW).unwrap();
    let (owner_endpoint, stranger_endpoint) = loopback_endpoints().await;
    let mut account_store = OplogSyncStore::new(&owner, account, || NOW);
    let mut content_store = OplogContentSyncStore::new(&owner, account, || NOW);

    let server = rag_rat_sync::accept_and_dispatch(
        &owner_endpoint,
        &mut account_store,
        &mut content_store,
        // Even an Open account-log endpoint must force Closed semantics for table streams.
        AuthPolicy::Open,
        || NOW,
    );
    let client = async {
        let conn =
            stranger_endpoint.connect(direct_addr(&owner_endpoint), TABLE_SYNC_ALPN).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        rag_rat_sync::codec::write_frame(&mut send, &Frame::Auth {
            account_id: account.to_bytes(),
            binding: Vec::new(),
        })
        .await
        .unwrap();
        let revealed =
            tokio::time::timeout(std::time::Duration::from_secs(1), recv.read_u8()).await;
        assert!(
            !matches!(revealed, Ok(Ok(_))),
            "an unauthorized peer must receive no application bytes"
        );
    };
    let (server, ()) = tokio::join!(server, client);
    assert!(matches!(server, Err(rag_rat_sync::SyncFailure::Auth(_))));
}

/// Multi-round reconciliation (#878): `connect_and_reconcile` re-dials until a round is dry, so a
/// single session's `Done` is never mistaken for a complete store. Over loopback: a fresh peer
/// takes TWO rounds (one transfers the account, one confirms the fixpoint), and re-running against
/// the now-in-sync peer takes exactly ONE (immediately dry — no over-dialing). The serve side
/// accepts each re-dial in a select loop and drops the pending accept once the client converges.
#[tokio::test]
async fn connect_and_reconcile_pulls_an_account_to_a_fixpoint_over_loopback() {
    use rag_rat_sync::{
        AuthPolicy, MAX_RECONCILE_ROUNDS, OplogSyncStore, ReconcileReport, SYNC_ALPN,
        accept_and_sync, connect_and_reconcile,
    };

    let source = fresh_db();
    let account = local_account(&source, NOW).unwrap();
    let want = account_entries_for_sync(&source, account).unwrap();
    assert!(!want.is_empty(), "the source has account entries to transfer");

    let dest = fresh_db();
    let (listener, dialer) = loopback_endpoints().await;
    let listener_addr = direct_addr(&listener);
    let policy = AuthPolicy::Open; // a fresh dest pulls the account log read-only

    // Drive the client (which re-dials internally) while the serve side accepts each connection,
    // stopping when the client resolves.
    async fn reconcile_with_server(
        source: &Connection,
        dest: &Connection,
        account: AccountId,
        listener: &iroh::Endpoint,
        dialer: &iroh::Endpoint,
        addr: iroh::EndpointAddr,
        policy: AuthPolicy,
    ) -> ReconcileReport {
        let mut src_store = OplogSyncStore::new(source, account, || NOW);
        let mut dst_store = OplogSyncStore::new(dest, account, || NOW);
        let client = connect_and_reconcile(
            dialer,
            addr,
            SYNC_ALPN,
            &mut dst_store,
            policy,
            || NOW,
            MAX_RECONCILE_ROUNDS,
        );
        tokio::pin!(client);
        loop {
            tokio::select! {
                resolved = &mut client => break resolved.unwrap(),
                accepted = accept_and_sync(listener, &mut src_store, policy, || NOW) => {
                    accepted.unwrap();
                }
            }
        }
    }

    let report = reconcile_with_server(
        &source,
        &dest,
        account,
        &listener,
        &dialer,
        listener_addr.clone(),
        policy,
    )
    .await;
    assert_eq!(report.rounds, 2, "one round transfers the account, one confirms the fixpoint");
    assert!(report.converged, "the reconciliation reached a fixpoint");
    assert_eq!(
        account_entries_for_sync(&dest, account).unwrap().len(),
        want.len(),
        "the whole account restored across the reconciliation",
    );

    let again =
        reconcile_with_server(&source, &dest, account, &listener, &dialer, listener_addr, policy)
            .await;
    assert_eq!(again.rounds, 1, "an already-in-sync peer converges in a single dry round");
    assert!(again.converged);
    assert_eq!(again.entries_newly_stored, 0, "nothing new to store when already converged");
}

/// The pairing acceptance drill (#930): a FRESH device holding no account enrolls into an owner's
/// account over the real iroh transport (loopback endpoints, no relay), then restores the account
/// log and its `/3` content — byte-identical — over the same endpoints. This is the D4
/// restore-from-zero criterion running in CI, composing the enrollment exchange
/// (`accept_enrollment` / `connect_and_enroll`) with steady-state sync (`accept_and_dispatch` /
/// `connect_and_sync`).
///
/// Content is `Plaintext`-sealed so the assertion is a byte-level restore of the moved entries;
/// sealed-key delivery to a joiner is covered separately by the oplog enrollment key-catch-up
/// tests.
#[tokio::test]
async fn a_fresh_device_enrolls_then_restores_the_account_byte_for_byte() {
    use std::collections::BTreeSet;

    use rag_rat_oplog::{
        DeviceRole, MemoryOp, NodeContent, NodeId, SealPolicy, author_content_batch,
        content_entries_for_sync, ensure_owned_stream_v2_in_tx,
    };
    use rag_rat_sync::{
        AuthPolicy, CONTENT_SYNC_ALPN, EnrollmentRequest, InviteSpec, OplogContentSyncStore,
        SYNC_ALPN, accept_and_dispatch, accept_enrollment, connect_and_enroll, connect_and_sync,
        mint_invite,
    };
    use rusqlite::{Transaction, TransactionBehavior};

    // Owner: an account, an owned stream, two authored memories.
    let owner = fresh_db();
    let account = local_account(&owner, NOW).unwrap();
    let stream = {
        let tx = Transaction::new_unchecked(&owner, TransactionBehavior::Immediate).unwrap();
        let s = ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW).unwrap();
        tx.commit().unwrap();
        s
    };
    let node = |id: &str, title: &str| MemoryOp::NodeCreate {
        node_id: NodeId::from(id),
        content: NodeContent {
            kind: "Invariant".into(),
            title: title.into(),
            body: "body".into(),
            confidence: "high".into(),
            source: "agent".into(),
            tags: Vec::new(),
            payload: None,
        },
    };
    author_content_batch(
        &owner,
        stream,
        &[node("n1", "first"), node("n2", "second")],
        SealPolicy::Plaintext,
        NOW,
    )
    .unwrap();
    assert_eq!(
        content_entries_for_sync(&owner, account).unwrap().len(),
        2,
        "the owner authored two memories to restore",
    );

    // Joiner: a fresh store with a device identity but no account membership yet.
    let joiner = fresh_db();
    assert!(
        rag_rat_oplog::read_local_account(&joiner).unwrap().is_none(),
        "the joiner starts with no account",
    );

    let (owner_ep, joiner_ep) = loopback_endpoints().await;
    let owner_addr = direct_addr(&owner_ep);

    // 1) Enrollment — the pairing moment. The owner atomically authors the joiner's DeviceAdd.
    {
        let local = rag_rat_oplog::local_device(&joiner, NOW).unwrap();
        let ticket = mint_invite(&owner, InviteSpec {
            account_id: account,
            inviter_node_id: *owner_ep.id().as_bytes(),
            relay_url: "https://relay.example".into(),
            role: DeviceRole::Member,
            label: Some("laptop"),
            now_ms: &|| NOW,
            ttl: std::time::Duration::from_secs(60),
        })
        .unwrap();
        // Budget / held / transport are recomputed inside `connect_and_enroll`; the placeholders
        // here mirror how the CLI hands off a freshly minted request.
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey: local.ed25519_public_key(),
            x25519_pubkey: local.x25519_public_key(),
            transport_node_id: [0u8; 32],
            budget: rag_rat_oplog::enrollment_budget(&joiner, account, NOW).unwrap(),
            held_entry_hashes: Vec::new(),
        };
        let server = accept_enrollment(&owner_ep, &owner, || NOW);
        let client =
            connect_and_enroll(&joiner_ep, owner_addr.clone(), &joiner, account, &request, NOW);
        let (server_r, client_r) = tokio::join!(server, client);
        server_r.unwrap();
        client_r.unwrap();
    }
    assert_eq!(
        rag_rat_oplog::read_local_account(&joiner).unwrap(),
        Some(account),
        "the joiner adopted the account on enrollment",
    );

    let policy = AuthPolicy::Closed;

    // 2) Account log first — the roster + stream ownership that authorize content acceptance.
    {
        let mut owner_account = OplogSyncStore::new(&owner, account, || NOW);
        let mut owner_content = OplogContentSyncStore::new(&owner, account, || NOW);
        let mut joiner_account = OplogSyncStore::new(&joiner, account, || NOW);
        let server =
            accept_and_dispatch(&owner_ep, &mut owner_account, &mut owner_content, policy, || NOW);
        let client = connect_and_sync(
            &joiner_ep,
            owner_addr.clone(),
            SYNC_ALPN,
            &mut joiner_account,
            policy,
            NOW,
        );
        let (server_r, client_r) = tokio::join!(server, client);
        assert_eq!(server_r.unwrap().0, SYNC_ALPN, "the account-log connection routed by ALPN");
        client_r.unwrap();
    }

    // 3) Content — accepted now that its authority is present on the joiner.
    {
        let mut owner_account = OplogSyncStore::new(&owner, account, || NOW);
        let mut owner_content = OplogContentSyncStore::new(&owner, account, || NOW);
        let mut joiner_content = OplogContentSyncStore::new(&joiner, account, || NOW);
        let server =
            accept_and_dispatch(&owner_ep, &mut owner_account, &mut owner_content, policy, || NOW);
        let client = connect_and_sync(
            &joiner_ep,
            owner_addr,
            CONTENT_SYNC_ALPN,
            &mut joiner_content,
            policy,
            NOW,
        );
        let (server_r, client_r) = tokio::join!(server, client);
        assert_eq!(server_r.unwrap().0, CONTENT_SYNC_ALPN, "the content connection routed by ALPN");
        client_r.unwrap();
    }

    // Restore-from-zero: the joiner's account log and content are byte-identical to the owner's
    // FINAL state — the owner's account log grew by the joiner's own `DeviceAdd` during enrollment,
    // so the comparison reads the owner after pairing, not the pre-pairing snapshot.
    let signed = |entries: Vec<Vec<u8>>| -> BTreeSet<Vec<u8>> { entries.into_iter().collect() };
    let owner_account = signed(
        account_entries_for_sync(&owner, account)
            .unwrap()
            .into_iter()
            .map(|e| e.signed_bytes)
            .collect(),
    );
    let owner_content = signed(
        content_entries_for_sync(&owner, account)
            .unwrap()
            .into_iter()
            .map(|e| e.signed_bytes)
            .collect(),
    );
    let joiner_account = signed(
        account_entries_for_sync(&joiner, account)
            .unwrap()
            .into_iter()
            .map(|e| e.signed_bytes)
            .collect(),
    );
    let joiner_content = signed(
        content_entries_for_sync(&joiner, account)
            .unwrap()
            .into_iter()
            .map(|e| e.signed_bytes)
            .collect(),
    );
    assert_eq!(
        joiner_account, owner_account,
        "the account log restored byte-for-byte on the joiner"
    );
    assert_eq!(joiner_content, owner_content, "the content restored byte-for-byte on the joiner");
}

/// A peer that offers a valid entry for a DIFFERENT account than the session is scoped to must not
/// have it stored — the account-scoped store rejects it before ingest, so it cannot grow other
/// accounts through a session that never named them.
#[tokio::test]
async fn an_entry_for_another_account_is_not_stored() {
    use rag_rat_sync::SyncStore;
    // Two independent accounts in two DBs.
    let other = fresh_db();
    let other_account = local_account(&other, NOW).unwrap();
    let other_entry =
        account_entries_for_sync(&other, other_account).unwrap()[0].signed_bytes.clone();

    let mine = fresh_db();
    let my_account = local_account(&mine, NOW).unwrap();
    assert_ne!(my_account.to_bytes(), other_account.to_bytes());

    // A store scoped to MY account is handed the OTHER account's (perfectly valid) entry.
    let mut store = OplogSyncStore::new(&mine, my_account, || NOW);
    let outcome = store.ingest(&other_entry).unwrap();
    assert_eq!(outcome, rag_rat_sync::Ingested::NoChange, "a foreign-account entry is refused");
    // And it did not land: my account holds only its own genesis, the other account holds nothing.
    assert!(
        account_entries_for_sync(&mine, other_account).unwrap().is_empty(),
        "the other account was not grown through my session",
    );
}
