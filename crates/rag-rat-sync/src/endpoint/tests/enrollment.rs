//! Enrollment over the endpoint: request identity, the enrollment-account guard, and loopback
//! exchanges.

use super::*;

fn local_request(database: &Connection) -> EnrollmentRequest {
    let local = rag_rat_oplog::local_device(database, NOW).unwrap();
    EnrollmentRequest {
        nonce: [1; 32].into(),
        expected_account: AccountId::from_bytes([5; 32]),
        ed25519_pubkey: local.ed25519_public_key(),
        x25519_pubkey: local.x25519_public_key(),
        transport_node_id: [2; 32],
        budget: rag_rat_oplog::EnrollmentBudget {
            account_entries_remaining: u64::MAX,
            account_bytes_remaining: u64::MAX,
            global_entries_remaining: u64::MAX,
            global_bytes_remaining: u64::MAX,
        },
        held_entry_hashes: Vec::new(),
    }
}

#[test]
fn enrollment_request_must_use_the_database_device_keys() {
    let database = database();
    let request = local_request(&database);
    let expected_account = AccountId::from_bytes([5; 32]);
    validate_enrollment_request_identity(&database, expected_account, &request, NOW).unwrap();

    let mut wrong_signing_key = request.clone();
    wrong_signing_key.ed25519_pubkey = [3; 32];
    assert!(matches!(
        validate_enrollment_request_identity(
            &database,
            expected_account,
            &wrong_signing_key,
            NOW,
        ),
        Err(InviteError::Malformed(message)) if message.contains("ed25519")
    ));

    let mut wrong_encryption_key = request;
    wrong_encryption_key.x25519_pubkey = [4; 32];
    assert!(matches!(
        validate_enrollment_request_identity(
            &database,
            expected_account,
            &wrong_encryption_key,
            NOW,
        ),
        Err(InviteError::Malformed(message)) if message.contains("X25519")
    ));
}

#[test]
fn enrollment_database_must_belong_to_the_served_account() {
    let served_db = database();
    let served = rag_rat_oplog::local_account(&served_db, NOW).unwrap().to_bytes();
    assert!(enrollment_database_matches(&served_db, served).unwrap());

    let other_db = database();
    let other = rag_rat_oplog::local_account(&other_db, NOW).unwrap().to_bytes();
    assert_ne!(served, other);
    assert!(
        !enrollment_database_matches(&other_db, served).unwrap(),
        "an enrollment database for another account is not accepted"
    );

    let unminted = database();
    assert!(
        !enrollment_database_matches(&unminted, served).unwrap(),
        "an enrollment database with no local account cannot redeem anything"
    );
}

#[tokio::test]
async fn enrollment_round_trips_over_loopback_endpoints() {
    let owner_db = database();
    let account = rag_rat_oplog::local_account(&owner_db, NOW).unwrap();
    let (listener, dialer) = loopback_endpoints().await;
    let joiner_db = database();
    let local = rag_rat_oplog::local_device(&joiner_db, NOW).unwrap();
    let ticket = crate::enrollment::mint_invite(&owner_db, crate::enrollment::InviteSpec {
        account_id: account,
        inviter_node_id: *listener.id().as_bytes(),
        relay_url: "https://relay.example".into(),
        role: rag_rat_oplog::DeviceRole::Member,
        label: None,
        now_ms: &|| NOW,
        ttl: std::time::Duration::from_secs(60),
    })
    .unwrap();
    // A stale caller-supplied transport identity is overwritten from the dialing endpoint;
    // without that, the acceptor would deterministically return WrongNode.
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey: local.ed25519_public_key(),
        x25519_pubkey: local.x25519_public_key(),
        transport_node_id: [0xaa; 32],
        budget: rag_rat_oplog::EnrollmentBudget {
            account_entries_remaining: 0,
            account_bytes_remaining: 0,
            global_entries_remaining: 0,
            global_bytes_remaining: 0,
        },
        held_entry_hashes: Vec::new(),
    };
    let peer = direct_addr(&listener);
    let server = accept_enrollment(&listener, &owner_db, || NOW);
    let client = connect_and_enroll(&dialer, peer, &joiner_db, account, &request, NOW);
    let (server_r, client_r) = tokio::join!(server, client);
    let accepted = server_r.unwrap();
    let received = client_r.unwrap();
    assert_eq!(received, accepted, "dialer and acceptor agree on the receipt");
    assert_eq!(rag_rat_oplog::read_local_account(&joiner_db).unwrap(), Some(account));
}

#[tokio::test]
async fn dispatch_routes_enrollment_over_loopback() {
    let owner_db = database();
    let account = rag_rat_oplog::local_account(&owner_db, NOW).unwrap();
    let (listener, dialer) = loopback_endpoints().await;
    let joiner_db = database();
    let local = rag_rat_oplog::local_device(&joiner_db, NOW).unwrap();
    let ticket = crate::enrollment::mint_invite(&owner_db, crate::enrollment::InviteSpec {
        account_id: account,
        inviter_node_id: *listener.id().as_bytes(),
        relay_url: "https://relay.example".into(),
        role: rag_rat_oplog::DeviceRole::Member,
        label: None,
        now_ms: &|| NOW,
        ttl: std::time::Duration::from_secs(60),
    })
    .unwrap();
    let request = EnrollmentRequest {
        nonce: ticket.nonce,
        expected_account: account,
        ed25519_pubkey: local.ed25519_public_key(),
        x25519_pubkey: local.x25519_public_key(),
        transport_node_id: [0xbb; 32],
        budget: rag_rat_oplog::EnrollmentBudget {
            account_entries_remaining: 0,
            account_bytes_remaining: 0,
            global_entries_remaining: 0,
            global_bytes_remaining: 0,
        },
        held_entry_hashes: Vec::new(),
    };
    let mut account_store = crate::store::OplogSyncStore::new(&owner_db, account, || NOW);
    let mut content_store = crate::store::OplogContentSyncStore::new(&owner_db, account, || NOW);
    let peer = direct_addr(&listener);
    let server = accept_and_dispatch(
        &listener,
        &mut account_store,
        &mut content_store,
        crate::auth::AuthPolicy::Open,
        || NOW,
    );
    let client = connect_and_enroll(&dialer, peer, &joiner_db, account, &request, NOW);
    let (server_r, client_r) = tokio::join!(server, client);
    let (alpn, _) = server_r.unwrap();
    assert_eq!(alpn, SyncAlpn::Enroll, "the dispatcher routed the enrollment stream");
    client_r.unwrap();
}

#[tokio::test]
async fn enrollment_refusal_and_wrong_alpn_close_over_loopback() {
    let owner_db = database();
    let _ = rag_rat_oplog::local_account(&owner_db, NOW).unwrap();
    let (listener, dialer) = loopback_endpoints().await;
    let joiner_db = database();
    let local = rag_rat_oplog::local_device(&joiner_db, NOW).unwrap();
    // An unknown nonce redeems nothing: the acceptor answers a semantic refusal and closes
    // without the enrolled wait.
    let request = EnrollmentRequest {
        nonce: [0x99; 32].into(),
        expected_account: rag_rat_oplog::read_local_account(&owner_db).unwrap().unwrap(),
        ed25519_pubkey: local.ed25519_public_key(),
        x25519_pubkey: local.x25519_public_key(),
        transport_node_id: [0; 32],
        budget: rag_rat_oplog::EnrollmentBudget {
            account_entries_remaining: 0,
            account_bytes_remaining: 0,
            global_entries_remaining: 0,
            global_bytes_remaining: 0,
        },
        held_entry_hashes: Vec::new(),
    };
    let peer = direct_addr(&listener);
    let server = accept_enrollment(&listener, &owner_db, || NOW);
    let client =
        connect_and_enroll(&dialer, peer, &joiner_db, request.expected_account, &request, NOW);
    let (server_r, client_r) = tokio::join!(server, client);
    assert!(matches!(server_r, Err(InviteError::Unknown)), "server: {server_r:?}");
    assert!(matches!(client_r, Err(InviteError::Unknown)), "client: {client_r:?}");

    // A connection negotiating the wrong ALPN is refused before any enrollment frame.
    let server = accept_enrollment(&listener, &joiner_db, || NOW);
    let client = async {
        let conn = dialer
            .connect(direct_addr(&listener), SYNC_ALPN)
            .await
            .map_err(|e| InviteError::Transport(e.to_string()))?;
        let (send, _recv) =
            conn.open_bi().await.map_err(|e| InviteError::Transport(e.to_string()))?;
        drop(send);
        conn.closed().await;
        Ok::<(), InviteError>(())
    };
    let (server_r, client_r) = tokio::join!(server, client);
    assert!(matches!(server_r, Err(InviteError::Malformed(_))), "server: {server_r:?}");
    client_r.unwrap();
}

#[tokio::test]
async fn an_enrollment_accept_on_a_closed_endpoint_is_a_transport_failure() {
    let (listener, _dialer) = loopback_endpoints().await;
    listener.close().await;
    let owner_db = database();
    let error = accept_enrollment(&listener, &owner_db, || NOW).await.unwrap_err();
    assert!(matches!(error, InviteError::Transport(ref message) if message == "endpoint closed"));
    assert_eq!(error.to_string(), "enrollment transport: endpoint closed");
}

#[test]
fn enrollment_refuses_a_conflicting_local_account_before_dialing() {
    let database = database();
    let request = local_request(&database);
    let existing_account = rag_rat_oplog::local_account(&database, NOW).unwrap();
    let expected_account = AccountId::from_bytes([7; 32]);
    assert_ne!(existing_account, expected_account);
    assert!(matches!(
        validate_enrollment_request_identity(
            &database,
            expected_account,
            &request,
            NOW,
        ),
        Err(InviteError::Malformed(message)) if message.contains("existing local account")
    ));
}
