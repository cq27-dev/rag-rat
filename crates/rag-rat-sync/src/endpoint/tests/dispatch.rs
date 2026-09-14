//! Dialing, accepting and ALPN dispatch over real loopback connections.

use super::*;

/// The serve scope narrows to `PublicOnly` for EXACTLY one case — an anonymous (fallback)
/// reader of a `PublicRead` account. A verified member of a public account, and every
/// `Open`/`Closed` session regardless of admission, serves `Full`.
#[test]
fn serve_scope_narrows_only_for_a_public_read_fallback_reader() {
    assert_eq!(
        serve_scope_for(AuthPolicy::PublicRead, PeerAdmission::Fallback),
        ServeScope::PublicOnly,
    );
    assert_eq!(serve_scope_for(AuthPolicy::PublicRead, PeerAdmission::Verified), ServeScope::Full);
    for policy in [AuthPolicy::Open, AuthPolicy::Closed] {
        for admission in [PeerAdmission::Verified, PeerAdmission::Fallback] {
            assert_eq!(
                serve_scope_for(policy, admission),
                ServeScope::Full,
                "{policy:?}/{admission:?}"
            );
        }
    }
}

#[tokio::test]
async fn dialer_push_is_acknowledged_before_the_connection_closes() {
    let account = [0xa1; 32];
    let expected: Vec<_> = (1..=3).map(test_entry).collect();
    let mut source_store = TestStore::new(
        account,
        expected.clone(),
        PeerCapability::ReadWrite,
        PeerCapability::ReadWrite,
    );
    let mut destination_store =
        TestStore::new(account, [], PeerCapability::ReadWrite, PeerCapability::ReadWrite);
    let (listener, dialer) = loopback_endpoints().await;
    let policy = AuthPolicy::Closed;

    // This is the direction #926 exposed: the dialer has the data and may close as soon as its
    // own session returns, while the acceptor is still ingesting the pushed stream.
    let server = accept_and_sync(&listener, &mut destination_store, policy, || NOW);
    let client = connect_and_sync(
        &dialer,
        direct_addr(&listener),
        SyncAlpn::Account,
        &mut source_store,
        policy,
        NOW,
    );
    let (server_result, client_result) = tokio::join!(server, client);
    let server_report = server_result.unwrap();
    let client_report = client_result.unwrap();

    assert_eq!(client_report.entries_sent, expected.len());
    assert_eq!(server_report.entries_newly_stored, expected.len());
    assert_eq!(
        destination_store.entries.len(),
        expected.len(),
        "the acceptor ingested the full authorized dialer push",
    );
}

#[tokio::test]
async fn a_read_only_dialer_can_pull_over_a_real_connection() {
    let account = [0xa2; 32];
    let expected: Vec<_> = (1..=3).map(test_entry).collect();
    let reader_only = test_entry(9);
    let mut server_store = TestStore::new(
        account,
        expected.clone(),
        PeerCapability::ReadWrite,
        PeerCapability::ReadOnly,
    );
    let mut reader_store = TestStore::new(
        account,
        [reader_only.clone()],
        PeerCapability::ReadOnly,
        PeerCapability::ReadWrite,
    );
    let (listener, dialer) = loopback_endpoints().await;

    let server = accept_and_sync(&listener, &mut server_store, AuthPolicy::Closed, || NOW);
    let client = connect_and_sync(
        &dialer,
        direct_addr(&listener),
        SyncAlpn::Account,
        &mut reader_store,
        AuthPolicy::Closed,
        NOW,
    );
    let (server_result, client_result) = tokio::join!(server, client);

    let server_report = server_result.unwrap();
    let client_report = client_result.unwrap();
    assert_eq!(server_report.entries_sent, expected.len());
    assert_eq!(server_report.entries_received, 0);
    assert_eq!(client_report.entries_sent, 0);
    assert_eq!(client_report.entries_newly_stored, expected.len());
    assert_eq!(server_store.entries.len(), expected.len());
    assert_eq!(reader_store.entries.len(), expected.len() + 1);
    assert!(reader_store.entries.contains_key(&reader_only.0));
    // An ADMITTED peer does trigger the inventory snapshot — the counter the admission-refusal
    // test asserts stays zero is a real instrument, not a no-op.
    assert!(server_store.snapshot_calls.load(std::sync::atomic::Ordering::Relaxed) > 0);
}

/// #406 admission: a peer the acceptor's policy rejects must learn NOTHING — the acceptor must
/// not even COMPUTE its inventory before the remote passes admission. The auth phase gates the
/// session, so a rejected dialer triggers zero `snapshot()` calls (and the acceptor errors out
/// before `run_session` ever sends a Hello). The acceptor holds real entries, so the guard is
/// not vacuous: were the snapshot computed pre-auth, the counter would be non-zero.
#[tokio::test]
async fn no_inventory_is_computed_for_a_peer_that_fails_admission() {
    let account = [0xa3; 32];
    let held: Vec<_> = (1..=3).map(test_entry).collect();
    let mut server_store =
        TestStore::new(account, held, PeerCapability::ReadWrite, PeerCapability::ReadWrite);
    // The acceptor rejects the dialer's binding under its Closed policy.
    server_store.peer_authorization = PeerAuthorization::Rejected;
    let server_snapshots = server_store.snapshot_calls.clone();
    let mut dialer_store =
        TestStore::new(account, [], PeerCapability::ReadWrite, PeerCapability::ReadWrite);
    let (listener, dialer) = loopback_endpoints().await;

    let server = accept_and_sync(&listener, &mut server_store, AuthPolicy::Closed, || NOW);
    let client = connect_and_sync(
        &dialer,
        direct_addr(&listener),
        SyncAlpn::Account,
        &mut dialer_store,
        AuthPolicy::Closed,
        NOW,
    );
    let (server_result, client_result) = tokio::join!(server, client);

    assert!(
        matches!(server_result, Err(SyncFailure::Auth(crate::auth::AuthError::Unauthorized))),
        "the acceptor refuses the rejected peer on ADMISSION (not a timeout/protocol fault): \
         {server_result:?}"
    );
    assert!(client_result.is_err(), "the dialer gets no session: {client_result:?}");
    assert_eq!(
        server_snapshots.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "no inventory may be computed before admission succeeds"
    );
}

#[tokio::test]
async fn dispatcher_honors_remote_read_only_grants_for_both_streams() {
    for alpn in [SyncAlpn::Account, SyncAlpn::Content] {
        let database = database();
        let account_id = rag_rat_oplog::local_account(&database, NOW).unwrap();
        let account = account_id.to_bytes();
        let account_entries =
            rag_rat_oplog::account_entries_for_sync(&database, account_id).unwrap().len();
        let server_entry = test_entry(1);
        let stale_local_entry = test_entry(9);
        let mut account_store = crate::store::OplogSyncStore::new(&database, account_id, || NOW);
        let mut content_store = TestStore::new(
            account,
            [server_entry.clone()],
            PeerCapability::ReadWrite,
            PeerCapability::ReadWrite,
        );
        // The production account authorizer rejects this fake binding under Open and grants the
        // dialer read-only access even though the dialer still considers itself write-capable.
        let mut stale_writer_store = TestStore::new(
            account,
            [stale_local_entry.clone()],
            PeerCapability::ReadWrite,
            PeerCapability::ReadWrite,
        );
        let (listener, dialer) = loopback_endpoints().await;

        let server = accept_and_dispatch(
            &listener,
            &mut account_store,
            &mut content_store,
            AuthPolicy::Open,
            || NOW,
        );
        let client = connect_and_sync(
            &dialer,
            direct_addr(&listener),
            alpn,
            &mut stale_writer_store,
            AuthPolicy::Open,
            NOW,
        );
        let (server_result, client_result) = tokio::join!(server, client);

        let (negotiated, server_report) = server_result.unwrap();
        let client_report = client_result.unwrap();
        assert_eq!(negotiated, alpn);
        assert_eq!(client_report.entries_sent, 0);
        assert_eq!(server_report.entries_received, 0);
        assert_eq!(
            client_report.entries_newly_stored,
            if alpn == SyncAlpn::Account { account_entries } else { 1 },
        );
        assert!(!content_store.entries.contains_key(&stale_local_entry.0));
    }
}

#[tokio::test]
async fn an_anonymous_open_dialer_can_pull_from_the_selected_server() {
    let source = database();
    let account = rag_rat_oplog::local_account(&source, NOW).unwrap();
    let expected = rag_rat_oplog::account_entries_for_sync(&source, account).unwrap();
    let destination = database();
    let (listener, dialer) = loopback_endpoints().await;
    let mut source_store = crate::store::OplogSyncStore::new(&source, account, || NOW);
    let mut destination_store = crate::store::OplogSyncStore::new(&destination, account, || NOW);

    let server = accept_and_sync(&listener, &mut source_store, AuthPolicy::Open, || NOW);
    let client = connect_and_sync(
        &dialer,
        direct_addr(&listener),
        SyncAlpn::Account,
        &mut destination_store,
        AuthPolicy::Open,
        NOW,
    );
    let (server_result, client_result) = tokio::join!(server, client);

    assert_eq!(server_result.unwrap().entries_sent, expected.len());
    assert_eq!(client_result.unwrap().entries_newly_stored, expected.len());
    assert_eq!(
        rag_rat_oplog::account_entries_for_sync(&destination, account).unwrap().len(),
        expected.len(),
        "the anonymous dialer restored the selected server's account snapshot",
    );
}

#[tokio::test]
async fn two_accounts_share_one_endpoint_and_route_by_the_named_account() {
    // Two DISTINCT accounts, each in its own store, hosted on ONE endpoint via
    // `dispatch_connection_multi`. A dialer naming account A restores A; a dialer naming
    // account B restores B — over the SAME listener. Because each dialer's store is
    // scoped to its own account (a foreign account's entries are rejected at ingest), a
    // B-dialer restoring B's log proves the host SELECTED B's store, not a fixed first
    // account — the isolation + routing guarantee together.
    let db_a = database();
    let acct_a = rag_rat_oplog::local_account(&db_a, NOW).unwrap();
    let db_b = database();
    let acct_b = rag_rat_oplog::local_account(&db_b, NOW).unwrap();
    let a_expected = rag_rat_oplog::account_entries_for_sync(&db_a, acct_a).unwrap();
    let b_expected = rag_rat_oplog::account_entries_for_sync(&db_b, acct_b).unwrap();
    assert_ne!(acct_a.to_bytes(), acct_b.to_bytes(), "the two hosted accounts are distinct");

    let (listener, dialer) = loopback_endpoints().await;
    let local_node = *listener.id().as_bytes();
    let mut hosts = vec![
        HostedAccount::new(
            crate::store::OplogSyncStore::new(&db_a, acct_a, || NOW),
            crate::store::OplogContentSyncStore::new(&db_a, acct_a, || NOW),
            AuthPolicy::Open,
        )
        .unwrap(),
        HostedAccount::new(
            crate::store::OplogSyncStore::new(&db_b, acct_b, || NOW),
            crate::store::OplogContentSyncStore::new(&db_b, acct_b, || NOW),
            AuthPolicy::Open,
        )
        .unwrap(),
    ];

    // Round 1 — a fresh peer anonymously restores account A.
    let dest_a = database();
    let mut dest_a_store = crate::store::OplogSyncStore::new(&dest_a, acct_a, || NOW);
    let server = async {
        let conn = accept_connection(&listener).await?;
        dispatch_connection_multi(conn, local_node, &mut hosts, || NOW, None).await
    };
    let client = connect_and_sync(
        &dialer,
        direct_addr(&listener),
        SyncAlpn::Account,
        &mut dest_a_store,
        AuthPolicy::Open,
        NOW,
    );
    let (server_result, _client_result) = tokio::join!(server, client);
    let (_alpn, report_a) = server_result.unwrap();
    assert_eq!(report_a.entries_sent, a_expected.len(), "the host served account A's log");
    assert_eq!(
        rag_rat_oplog::account_entries_for_sync(&dest_a, acct_a).unwrap().len(),
        a_expected.len(),
        "the A-dialer restored account A",
    );

    // Round 2 — a fresh peer anonymously restores account B over the SAME host.
    let dest_b = database();
    let mut dest_b_store = crate::store::OplogSyncStore::new(&dest_b, acct_b, || NOW);
    let server = async {
        let conn = accept_connection(&listener).await?;
        dispatch_connection_multi(conn, local_node, &mut hosts, || NOW, None).await
    };
    let client = connect_and_sync(
        &dialer,
        direct_addr(&listener),
        SyncAlpn::Account,
        &mut dest_b_store,
        AuthPolicy::Open,
        NOW,
    );
    let (server_result, _client_result) = tokio::join!(server, client);
    let (_alpn, report_b) = server_result.unwrap();
    assert_eq!(report_b.entries_sent, b_expected.len(), "the host served account B's log");
    assert_eq!(
        rag_rat_oplog::account_entries_for_sync(&dest_b, acct_b).unwrap().len(),
        b_expected.len(),
        "the B-dialer restored account B — selection served B's store, not a fixed first account",
    );
}

#[test]
fn hosted_account_rejects_a_sync_content_pair_for_different_accounts() {
    // The isolation guard lifted to construction: a content store behind another account's log
    // is unrepresentable, so a connection authenticated for A can never reach B's content.
    let db_a = database();
    let acct_a = rag_rat_oplog::local_account(&db_a, NOW).unwrap();
    let db_b = database();
    let acct_b = rag_rat_oplog::local_account(&db_b, NOW).unwrap();
    let mismatched = HostedAccount::new(
        crate::store::OplogSyncStore::new(&db_a, acct_a, || NOW),
        crate::store::OplogContentSyncStore::new(&db_b, acct_b, || NOW),
        AuthPolicy::Open,
    );
    assert!(mismatched.is_err(), "a sync/content pair for different accounts is refused");
}

#[tokio::test]
async fn a_multi_host_applies_each_accounts_own_admission_policy() {
    // One host, two accounts: A is Open, B is Closed. An anonymous dialer restores A but is
    // refused on B — the policy is per-account (from the selection), not endpoint-wide.
    let db_a = database();
    let acct_a = rag_rat_oplog::local_account(&db_a, NOW).unwrap();
    let db_b = database();
    let acct_b = rag_rat_oplog::local_account(&db_b, NOW).unwrap();
    let a_expected = rag_rat_oplog::account_entries_for_sync(&db_a, acct_a).unwrap();
    let (listener, dialer) = loopback_endpoints().await;
    let local_node = *listener.id().as_bytes();
    let mut hosts = vec![
        HostedAccount::new(
            crate::store::OplogSyncStore::new(&db_a, acct_a, || NOW),
            crate::store::OplogContentSyncStore::new(&db_a, acct_a, || NOW),
            AuthPolicy::Open,
        )
        .unwrap(),
        HostedAccount::new(
            crate::store::OplogSyncStore::new(&db_b, acct_b, || NOW),
            crate::store::OplogContentSyncStore::new(&db_b, acct_b, || NOW),
            AuthPolicy::Closed,
        )
        .unwrap(),
    ];

    // The Open account A admits the anonymous dialer and restores its log.
    let dest_a = database();
    let mut dest_a_store = crate::store::OplogSyncStore::new(&dest_a, acct_a, || NOW);
    let server = async {
        let conn = accept_connection(&listener).await?;
        dispatch_connection_multi(conn, local_node, &mut hosts, || NOW, None).await
    };
    let client = connect_and_sync(
        &dialer,
        direct_addr(&listener),
        SyncAlpn::Account,
        &mut dest_a_store,
        AuthPolicy::Open,
        NOW,
    );
    let (server_a, _client_a) = tokio::join!(server, client);
    assert!(server_a.is_ok(), "the Open account admits the anonymous dialer: {server_a:?}");
    assert_eq!(
        rag_rat_oplog::account_entries_for_sync(&dest_a, acct_a).unwrap().len(),
        a_expected.len(),
    );

    // The Closed account B refuses the same anonymous dialer — on the SAME host.
    let dest_b = database();
    let mut dest_b_store = crate::store::OplogSyncStore::new(&dest_b, acct_b, || NOW);
    let server = async {
        let conn = accept_connection(&listener).await?;
        dispatch_connection_multi(conn, local_node, &mut hosts, || NOW, None).await
    };
    let client = connect_and_sync(
        &dialer,
        direct_addr(&listener),
        SyncAlpn::Account,
        &mut dest_b_store,
        AuthPolicy::Open,
        NOW,
    );
    let (server_b, _client_b) = tokio::join!(server, client);
    assert!(
        matches!(server_b, Err(SyncFailure::Auth(_))),
        "the Closed account refuses the anonymous dialer: {server_b:?}"
    );
}

#[tokio::test]
async fn an_anonymous_open_dialer_suppresses_its_push() {
    let source = database();
    let account = rag_rat_oplog::local_account(&source, NOW).unwrap();
    let destination = database();
    let (listener, dialer) = loopback_endpoints().await;
    let mut source_store = crate::store::OplogSyncStore::new(&source, account, || NOW);
    let mut destination_store = crate::store::OplogSyncStore::new(&destination, account, || NOW);

    let server = accept_and_sync(&listener, &mut destination_store, AuthPolicy::Open, || NOW);
    let client = connect_and_sync(
        &dialer,
        direct_addr(&listener),
        SyncAlpn::Account,
        &mut source_store,
        AuthPolicy::Open,
        NOW,
    );
    let (server_result, client_result) = tokio::join!(server, client);

    assert_eq!(server_result.unwrap().entries_received, 0);
    assert_eq!(client_result.unwrap().entries_sent, 0);
    assert!(
        rag_rat_oplog::account_entries_for_sync(&destination, account).unwrap().is_empty(),
        "anonymous open admission reaches no account ingest",
    );
}
