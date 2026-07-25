//! End-to-end: real op-log entries move between two SQLite stores over the session, and the
//! receiver ingests them through the production seam (phase D, #406).
//!
//! This is the load-bearing test for D1+D2 — the unit tests exercise the protocol over an in-memory
//! store, this exercises it over `OplogSyncStore` with genuine signed account entries that the
//! receiver re-verifies via `account_ingest`.

use rag_rat_oplog::{AccountId, account_entries_for_sync, local_account};
use rag_rat_sync::{OplogSyncStore, run_session};
use rusqlite::Connection;

const NOW: i64 = 1_700_000_000_000;

fn fresh_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    conn
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
        run_session(&mut source_store, a_send, a_recv),
        run_session(&mut dest_store, b_send, b_recv),
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
        run_session(&mut a_store, a_send, a_recv),
        run_session(&mut b_store, b_send, b_recv),
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
            run_session(&mut src, a_send, a_recv),
            run_session(&mut dst, b_send, b_recv),
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
            run_session(&mut src, a_send, a_recv),
            run_session(&mut dst, b_send, b_recv),
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
    let binding = store.local_binding(&node, NOW).unwrap();
    assert!(!binding.is_empty(), "a store with a local device mints a real binding");
    assert!(store.authorize(&binding, &node, NOW).unwrap(), "its own node id is authorized");
    assert!(
        !store.authorize(&binding, &[5u8; 32], NOW).unwrap(),
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
    let binding = stale.local_binding(&node, much_later).unwrap();
    assert!(
        stale.authorize(&binding, &node, much_later).unwrap(),
        "a long-lived store still authorizes using the current handshake time",
    );
}

/// A store with no local account (a fresh onboarding peer) presents an empty, anonymous binding and
/// authorizes nothing — so it can complete an `Open` handshake but never passes `Closed`.
#[test]
fn a_store_without_an_account_presents_an_empty_binding_and_authorizes_nothing() {
    use rag_rat_sync::NodeAuth;

    let db = fresh_db(); // schema only, no account
    let account = AccountId::from_bytes([7u8; 32]);
    let store = OplogSyncStore::new(&db, account, || NOW);
    assert!(
        store.local_binding(&[1u8; 32], NOW).unwrap().is_empty(),
        "no local device => an anonymous (empty) binding",
    );
    assert!(
        !store.authorize(&[0u8; 10], &[1u8; 32], NOW).unwrap(),
        "an accountless store authorizes no peer",
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
    let binding = store.local_binding(&node, NOW).unwrap();
    assert!(store.authorize(&binding, &node, NOW).unwrap(), "own node authorized");
    assert!(!store.authorize(&binding, &[5u8; 32], NOW).unwrap(), "other node refused");
}

/// LIVE: the real iroh endpoint, dialed peer-to-peer over the configured relay. Ignored by default
/// (needs network + the relay); run with `--ignored` to exercise the actual transport rather than
/// the in-process duplex the other tests use.
#[tokio::test]
#[ignore = "live: binds iroh endpoints and dials over the relay"]
async fn a_real_iroh_round_trip_restores_an_account() {
    let relay = std::env::var("RAG_RAT_SYNC_RELAY").expect("set RAG_RAT_SYNC_RELAY to run this");

    let source = fresh_db();
    let account_id = local_account(&source, NOW).unwrap();
    let want = account_entries_for_sync(&source, account_id).unwrap();
    let dest = fresh_db();

    let listener = rag_rat_sync::build_endpoint([1u8; 32], &relay).await.unwrap();
    let dialer = rag_rat_sync::build_endpoint([2u8; 32], &relay).await.unwrap();
    let listener_addr = rag_rat_sync::endpoint_addr(&listener);

    let mut source_store = OplogSyncStore::new(&source, account_id, || NOW);
    let mut dest_store = OplogSyncStore::new(&dest, account_id, || NOW);
    // The dest is a fresh peer with no local device (onboarding), so this round-trip uses `Open`;
    // the node-authorization path is covered directly by the auth-phase tests below and the oplog
    // node_binding unit tests.
    let policy = rag_rat_sync::AuthPolicy::Open;
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
    let policy = rag_rat_sync::AuthPolicy::Open;

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
