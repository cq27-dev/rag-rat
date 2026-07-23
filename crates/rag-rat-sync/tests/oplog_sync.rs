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

    let mut source_store = OplogSyncStore::new(&source, account_id, NOW);
    let mut dest_store = OplogSyncStore::new(&dest, account_id, NOW);

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
        let mut b_store = OplogSyncStore::new(&b, account_id, NOW);
        for e in &entries {
            use rag_rat_sync::SyncStore;
            b_store.ingest(&e.signed_bytes).unwrap();
        }
    }
    assert_eq!(account_entries_for_sync(&b, account_id).unwrap().len(), entries.len());

    let mut a_store = OplogSyncStore::new(&a, account_id, NOW);
    let mut b_store = OplogSyncStore::new(&b, account_id, NOW);
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

    let mut source_store = OplogSyncStore::new(&source, account_id, NOW);
    let mut dest_store = OplogSyncStore::new(&dest, account_id, NOW);
    let server = async { rag_rat_sync::accept_and_sync(&listener, &mut source_store).await };
    let client =
        async { rag_rat_sync::connect_and_sync(&dialer, listener_addr, &mut dest_store).await };
    let (server_r, client_r) = tokio::join!(server, client);
    server_r.unwrap();
    client_r.unwrap();

    let got = account_entries_for_sync(&dest, account_id).unwrap();
    assert_eq!(got.len(), want.len(), "the account restored over real iroh transport");
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
    let mut store = OplogSyncStore::new(&mine, my_account, NOW);
    let outcome = store.ingest(&other_entry).unwrap();
    assert_eq!(outcome, rag_rat_sync::Ingested::NoChange, "a foreign-account entry is refused");
    // And it did not land: my account holds only its own genesis, the other account holds nothing.
    assert!(
        account_entries_for_sync(&mine, other_account).unwrap().is_empty(),
        "the other account was not grown through my session",
    );
}
