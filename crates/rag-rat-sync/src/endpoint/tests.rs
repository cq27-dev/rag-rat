use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use rag_rat_oplog::AccountId;
use rusqlite::Connection;
use tokio::time::timeout;

use super::accept::{
    ACCEPT_BURST, ACCEPT_REFILL_PER_SEC, EGRESS_BURST_BYTES, GRACEFUL_CLOSE_TIMEOUT,
};
use super::dial::{ReconcileStep, RoundTally, reconcile_step};
use super::dispatch::serve_scope_for;
use super::enroll::{enrollment_database_matches, validate_enrollment_request_identity};
use super::*;
use crate::auth::{
    AuthConfig, AuthPolicy, AuthRole, DEFAULT_PRE_AUTH_TIMEOUT, LocalAuth, NodeAuth, PeerAdmission,
    PeerAuthorization, PeerCapability, run_auth_phase,
};
use crate::enrollment::{ENROLL_ALPN, EnrollmentRequest, InviteError};
use crate::session::{DEFAULT_IDLE_TIMEOUT, ServeScope, SyncStore};
use crate::table_session::{TableSessionReport, TableSyncStore, run_table_session};
use crate::table_wire::TABLE_SYNC_ALPN;
use crate::wire::{CONTENT_SYNC_ALPN, SYNC_ALPN};

const NOW: i64 = 1_700_000_000_000;

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

fn database() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    conn
}

#[test]
fn reconcile_step_loops_until_a_fully_quiet_round() {
    let moved =
        |newly_stored, sent, received| RoundTally::default().record(newly_stored, sent, received);
    // Nothing moved in either direction — the fixpoint.
    let quiet = moved(0, 0, 0);
    assert_eq!(reconcile_step(quiet, 1, 8), ReconcileStep::Stop { converged: true });
    // Each direction of movement, on its own, keeps the loop going under the cap: `stored`
    // (local promotion), `received` (peer still has data), and `sent` (our push may have made
    // the acceptor evict — a quiet confirmation round must prove the re-push landed).
    let stored = moved(2, 0, 0);
    let received = moved(0, 0, 5);
    let sent = moved(0, 3, 0);
    for round in [stored, received, sent] {
        assert_eq!(reconcile_step(round, 1, 8), ReconcileStep::Continue);
    }
    // Still moving at the cap stops UN-converged so a later maintenance pass continues.
    assert_eq!(reconcile_step(sent, 8, 8), ReconcileStep::Stop { converged: false });
    // A quiet round at the cap is still the converged fixpoint.
    assert_eq!(reconcile_step(quiet, 8, 8), ReconcileStep::Stop { converged: true });
}

/// A configured-peers-only resolve: nothing published, nothing to open.
fn no_announcements(_payload: &[u8]) -> Option<[u8; 32]> {
    None
}

#[tokio::test]
async fn discover_peers_resolves_valid_ids_and_counts_invalid_ones() {
    let valid = node_id_to_string(&node_id_from_secret([7u8; 32])).unwrap();
    let resolved = discover_peers(
        &[valid.clone(), "not-a-node-id".to_string()],
        "https://relay.example",
        None,
        &no_announcements,
    )
    .await;
    assert_eq!(resolved.peers.len(), 1, "the unparseable id is dropped, the valid one resolves");
    assert_eq!(resolved.peers[0].0, valid, "the resolved entry keeps its node-id label");
    assert_eq!(
        resolved.unresolved_configured, 1,
        "the unparseable id is COUNTED, not silently forgotten — the driver seeds its error tally \
         from this and cannot recover it by subtraction once discovery adds peers"
    );
}

/// Standard base32, no padding — one of the spellings `EndpointId::from_str` accepts for a
/// node id, alongside the 64-char lowercase hex that `Display` produces.
fn base32_nopad(bytes: &[u8; 32]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(52);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        acc = (acc << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// One node written several ways must be dialed once.
///
/// `EndpointId::from_str` takes 64-char lowercase hex OR standard base32, and uppercases before
/// base32-decoding — so three strings name one node, while `[sync] server_peers` only
/// de-duplicates literally. Comparing display strings would dial this peer three times per pass
/// (each a full multi-ALPN reconcile) and triple-count it in `ok`/`errors`.
#[tokio::test]
async fn discover_peers_dedupes_configured_spellings_of_one_node() {
    let bytes = node_id_from_secret([11u8; 32]);
    let hex = node_id_to_string(&bytes).unwrap();
    let base32_upper = base32_nopad(&bytes);
    let base32_lower = base32_upper.to_ascii_lowercase();
    for spelling in [&hex, &base32_upper, &base32_lower] {
        assert_eq!(
            EndpointId::from_str(spelling).unwrap().as_bytes(),
            &bytes,
            "every spelling under test must really name this node"
        );
    }
    assert_eq!(
        [&hex, &base32_upper, &base32_lower].iter().collect::<HashSet<_>>().len(),
        3,
        "the spellings must be textually distinct or the test proves nothing"
    );

    let configured =
        [hex.clone(), base32_upper.clone(), base32_lower.clone(), base32_upper.clone()];
    let resolved =
        discover_peers(&configured, "https://relay.example", None, &no_announcements).await;
    assert_eq!(resolved.peers.len(), 1, "every spelling names one peer, dialed once");
    assert_eq!(resolved.peers[0].0, hex, "the first spelling configured wins");
    assert_eq!(
        resolved.unresolved_configured, 0,
        "a de-duplicated spelling resolved fine; it is not an error"
    );
}

#[test]
fn node_id_string_round_trips_through_bytes() {
    let bytes = node_id_from_secret([9u8; 32]);
    let text = node_id_to_string(&bytes).unwrap();
    // `peer_addr` parses the same hex form, so the string is a valid dial id, and
    // `peer_addr_from_bytes` reaches the same address from the raw bytes a ticket carries.
    assert!(peer_addr(&text, "https://relay.example").is_ok());
    assert!(peer_addr_from_bytes(&bytes, "https://relay.example").is_ok());
}

struct TestStore {
    account: [u8; 32],
    entries: HashMap<[u8; 32], Vec<u8>>,
    local_capability: PeerCapability,
    peer_authorization: PeerAuthorization,
    /// Counts `snapshot()` calls, so a test can prove no inventory was computed before
    /// admission (#406/#881: the auth phase gates the session, so a rejected peer must
    /// trigger zero snapshots). Cloned out before the store moves into a session.
    snapshot_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl TestStore {
    fn new(
        account: [u8; 32],
        entries: impl IntoIterator<Item = ([u8; 32], Vec<u8>)>,
        local_capability: PeerCapability,
        peer_capability: PeerCapability,
    ) -> Self {
        Self {
            account,
            entries: entries.into_iter().collect(),
            local_capability,
            peer_authorization: PeerAuthorization::Granted(peer_capability),
            snapshot_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl SyncStore for TestStore {
    fn account_id(&self) -> [u8; 32] {
        self.account
    }

    fn set_serve_scope(&mut self, _scope: crate::session::ServeScope) {}

    fn snapshot(&self) -> anyhow::Result<Vec<([u8; 32], Vec<u8>)>> {
        self.snapshot_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(self.entries.iter().map(|(hash, bytes)| (*hash, bytes.clone())).collect())
    }

    fn ingest(&mut self, signed_bytes: &[u8]) -> anyhow::Result<crate::session::Ingested> {
        let hash: [u8; 32] = signed_bytes[..32].try_into()?;
        match self.entries.entry(hash) {
            std::collections::hash_map::Entry::Occupied(_) =>
                Ok(crate::session::Ingested::NoChange),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(signed_bytes.to_vec());
                Ok(crate::session::Ingested::Stored)
            },
        }
    }
}

impl NodeAuth for TestStore {
    fn local_auth(&self, _local_node: &[u8; 32], _now_ms: i64) -> anyhow::Result<LocalAuth> {
        Ok(LocalAuth { binding: vec![1], capability: self.local_capability })
    }

    fn authorize(
        &self,
        _binding: &[u8],
        _remote_node: &[u8; 32],
        _now_ms: i64,
    ) -> anyhow::Result<PeerAuthorization> {
        Ok(self.peer_authorization)
    }
}

struct TableTestStore {
    auth: TestStore,
    supported: Vec<crate::table_wire::ManifestItem>,
    entries: HashMap<[u8; 32], HashMap<[u8; 32], Vec<u8>>>,
}

impl TableTestStore {
    fn new(
        account: [u8; 32],
        supported: Vec<crate::table_wire::ManifestItem>,
        entries: impl IntoIterator<Item = ([u8; 32], ([u8; 32], Vec<u8>))>,
    ) -> Self {
        let mut by_stream: HashMap<_, HashMap<_, _>> = HashMap::new();
        for (stream, (hash, bytes)) in entries {
            by_stream.entry(stream).or_default().insert(hash, bytes);
        }
        Self {
            auth: TestStore::new(account, [], PeerCapability::ReadWrite, PeerCapability::ReadWrite),
            supported,
            entries: by_stream,
        }
    }
}

impl TableSyncStore for TableTestStore {
    fn account_id(&self) -> [u8; 32] {
        self.auth.account
    }

    fn supported_streams(&self) -> anyhow::Result<Vec<crate::table_wire::ManifestItem>> {
        Ok(self.supported.clone())
    }

    fn validates(&self, item: &crate::table_wire::ManifestItem) -> anyhow::Result<bool> {
        Ok(self.supported.contains(item))
    }

    fn chain_page(
        &self,
        item: &crate::table_wire::ManifestItem,
        after_device: Option<[u8; 32]>,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::table_wire::ChainHead>> {
        let mut devices: Vec<_> = self
            .entries
            .get(&item.stream_id)
            .into_iter()
            .flatten()
            .map(|(hash, _)| *hash)
            .filter(|device| after_device.is_none_or(|after| *device > after))
            .collect();
        devices.sort();
        Ok(devices
            .into_iter()
            .take(limit)
            .map(|device| crate::table_wire::ChainHead {
                device_fingerprint: device,
                lamport: 0,
                entry_hash: device,
                floor: None,
            })
            .collect())
    }

    fn frontier(
        &self,
        item: &crate::table_wire::ManifestItem,
        device: [u8; 32],
    ) -> anyhow::Result<crate::table_wire::FrontierState> {
        Ok(
            if self
                .entries
                .get(&item.stream_id)
                .is_some_and(|entries| entries.contains_key(&device))
            {
                crate::table_wire::FrontierState::Accepted { lamport: 0, entry_hash: device }
            } else {
                crate::table_wire::FrontierState::Empty
            },
        )
    }

    fn entries(
        &self,
        item: &crate::table_wire::ManifestItem,
        device: [u8; 32],
        start: crate::table_session::ChainStart,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::table_session::ChainEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(bytes) =
            self.entries.get(&item.stream_id).and_then(|entries| entries.get(&device))
        else {
            return Ok(Vec::new());
        };
        let include = match start {
            crate::table_session::ChainStart::Beginning => true,
            crate::table_session::ChainStart::After { lamport, entry_hash } => {
                if lamport != 0 || entry_hash != device {
                    return Ok(Vec::new());
                }
                false
            },
            crate::table_session::ChainStart::At { lamport, entry_hash } =>
                lamport == 0 && entry_hash == device,
        };
        Ok(include
            .then(|| crate::table_session::ChainEntry {
                lamport: 0,
                entry_hash: device,
                signed_bytes: bytes.clone(),
            })
            .into_iter()
            .collect())
    }

    fn ingest(
        &mut self,
        item: &crate::table_wire::ManifestItem,
        expected_device: [u8; 32],
        signed_bytes: &[u8],
        _advertised_floor: Option<(u64, [u8; 32])>,
    ) -> anyhow::Result<crate::session::Ingested> {
        if !self.supported.contains(item) {
            return Ok(crate::session::Ingested::NoChange);
        }
        let hash: [u8; 32] = signed_bytes[..32].try_into()?;
        if hash != expected_device {
            return Ok(crate::session::Ingested::NoChange);
        }
        Ok(match self.entries.entry(item.stream_id).or_default().entry(hash) {
            std::collections::hash_map::Entry::Occupied(_) => crate::session::Ingested::NoChange,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(signed_bytes.to_vec());
                crate::session::Ingested::Stored
            },
        })
    }
}

impl NodeAuth for TableTestStore {
    fn local_auth(&self, local_node: &[u8; 32], now_ms: i64) -> anyhow::Result<LocalAuth> {
        self.auth.local_auth(local_node, now_ms)
    }

    fn authorize(
        &self,
        binding: &[u8],
        remote_node: &[u8; 32],
        now_ms: i64,
    ) -> anyhow::Result<PeerAuthorization> {
        self.auth.authorize(binding, remote_node, now_ms)
    }
}

fn test_entry(seed: u8) -> ([u8; 32], Vec<u8>) {
    ([seed; 32], vec![seed; 40])
}

fn table_item(repo: &str, stream: u8) -> crate::table_wire::ManifestItem {
    crate::table_wire::ManifestItem {
        repo_id: repo.into(),
        incarnation_ref: [1; 32],
        scope_id: "anchors/1".into(),
        stream_id: [stream; 32],
    }
}

fn local_request(database: &Connection) -> EnrollmentRequest {
    let local = rag_rat_oplog::local_device(database, NOW).unwrap();
    EnrollmentRequest {
        nonce: [1; 32],
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

/// Two iroh endpoints on loopback UDP with the relay disabled — no network, no relay, just a
/// real QUIC transport between in-process endpoints.
async fn loopback_endpoints() -> (Endpoint, Endpoint) {
    let bind = |seed: [u8; 32]| async move {
        Endpoint::builder(presets::Minimal)
            .alpns(vec![
                SYNC_ALPN.to_vec(),
                CONTENT_SYNC_ALPN.to_vec(),
                TABLE_SYNC_ALPN.to_vec(),
                ENROLL_ALPN.to_vec(),
            ])
            .relay_mode(RelayMode::Disabled)
            .secret_key(SecretKey::from_bytes(&seed))
            .bind()
            .await
            .unwrap()
    };
    (bind([0x11; 32]).await, bind([0x12; 32]).await)
}

fn direct_addr(endpoint: &Endpoint) -> EndpointAddr {
    let port = endpoint
        .addr()
        .ip_addrs()
        .next()
        .expect("a bound endpoint advertises at least one socket address")
        .port();
    EndpointAddr::new(endpoint.id())
        .with_ip_addr(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
}

#[test]
fn accept_rate_admits_a_burst_then_denies() {
    let mut limiter = GlobalAcceptRateLimiter::new();
    // The full burst is admitted at one instant...
    for i in 0..ACCEPT_BURST as usize {
        assert!(limiter.allow(NOW), "burst connection {i} within capacity");
    }
    // ...and the next one at the same instant is denied.
    assert!(!limiter.allow(NOW), "the connection past the burst is refused");
}

#[test]
fn egress_bounds_bytes_then_refills() {
    let mut limiter = GlobalEgressLimiter::new();
    // A page is permitted while any credit remains, even one larger than the whole burst
    // (forward progress), driving the balance to zero-or-below.
    assert!(limiter.allow(EGRESS_BURST_BYTES as usize, NOW), "the burst is servable");
    assert!(!limiter.allow(1, NOW), "a further page at the same instant is refused (no credit)");
    // One second refills `EGRESS_REFILL_BYTES_PER_SEC`, so serving resumes.
    assert!(limiter.allow(1, NOW + 1000), "refilled credit permits serving again after 1s");
}

#[test]
fn accept_rate_refills_over_time() {
    let mut limiter = GlobalAcceptRateLimiter::new();
    while limiter.allow(NOW) {} // drain the burst
    assert!(!limiter.allow(NOW), "drained");
    // One second later, exactly `ACCEPT_REFILL_PER_SEC` tokens are available again.
    let later = NOW + 1000;
    for i in 0..ACCEPT_REFILL_PER_SEC as usize {
        assert!(limiter.allow(later), "refilled token {i} available after 1s");
    }
    assert!(!limiter.allow(later), "no more than the per-second refill accrues in 1s");
}

#[test]
fn accept_rate_refill_is_capped_at_the_burst() {
    let mut limiter = GlobalAcceptRateLimiter::new();
    while limiter.allow(NOW) {} // drain
    // A long idle must not accrue unbounded credit — only up to the burst.
    let long_idle = NOW + 1_000_000;
    for i in 0..ACCEPT_BURST as usize {
        assert!(limiter.allow(long_idle), "capped-refill token {i}");
    }
    assert!(!limiter.allow(long_idle), "idle time accrues at most one burst, not more");
}

#[test]
fn accept_rate_never_denies_traffic_under_the_rate() {
    let mut limiter = GlobalAcceptRateLimiter::new();
    // One connection every 250ms = 4/s, well under the 8/s refill — never denied over a long
    // run.
    for tick in 0..200 {
        let now = NOW + tick * 250;
        assert!(limiter.allow(now), "steady sub-rate traffic at tick {tick} is admitted");
    }
}

#[tokio::test]
async fn a_drained_accept_rate_refuses_a_connection_before_the_handshake() {
    let (listener, dialer) = loopback_endpoints().await;
    let mut limiter = GlobalAcceptRateLimiter::new();
    while limiter.allow(NOW) {} // drain so the next accept is refused

    let server = accept_connection_within_rate(&listener, &mut limiter, || NOW);
    let client = async {
        // The refused `Incoming` makes the dial fail rather than establish a session.
        timeout(DEFAULT_IDLE_TIMEOUT, dialer.connect(direct_addr(&listener), SYNC_ALPN)).await
    };
    let (server_result, client_result) = tokio::join!(server, client);

    assert!(
        matches!(server_result, Ok(None)),
        "a drained limiter refuses at the Incoming stage: {server_result:?}"
    );
    assert!(
        matches!(client_result, Ok(Err(_)) | Err(_)),
        "the dialer's connection does not establish"
    );
}

#[tokio::test]
async fn the_accept_rate_clock_is_read_when_the_peer_connects_not_before_the_wait() {
    // Drain the bucket, then let the connection arrive an HOUR later. The limiter must refill
    // against the CONNECT time — read via the closure after `accept()` resolves — not a
    // timestamp captured before the idle wait. With an eagerly-captured clock this
    // would wrongly refuse a legitimate connection after any load-then-idle stretch;
    // the closure makes it admit.
    let (listener, dialer) = loopback_endpoints().await;
    let mut limiter = GlobalAcceptRateLimiter::new();
    while limiter.allow(NOW) {}
    let connect_at = NOW + 3_600_000;

    let server = accept_connection_within_rate(&listener, &mut limiter, move || connect_at);
    let client = async {
        timeout(DEFAULT_IDLE_TIMEOUT, dialer.connect(direct_addr(&listener), SYNC_ALPN)).await
    };
    let (server_result, client_result) = tokio::join!(server, client);

    assert!(
        matches!(server_result, Ok(Some(_))),
        "the bucket refilled to the connect time admits the delayed connection: {server_result:?}"
    );
    assert!(matches!(client_result, Ok(Ok(_))), "the dialer connects: {client_result:?}");
}

async fn accept_test_table_sync(
    endpoint: &Endpoint,
    store: &mut TableTestStore,
) -> TableSessionReport {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    assert_eq!(conn.alpn(), TABLE_SYNC_ALPN);
    let local_node = *endpoint.id().as_bytes();
    let remote_node = *conn.remote_id().as_bytes();
    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
    let (capabilities, _admission) = run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
        role: AuthRole::Acceptor,
        account_id: store.account_id(),
        local_node,
        remote_node,
        policy: AuthPolicy::Closed,
        now_ms: NOW,
        pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
    })
    .await
    .unwrap();
    let report =
        run_table_session(store, send, recv, AuthRole::Acceptor, capabilities).await.unwrap();
    let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    conn.close(0u32.into(), b"done");
    report
}

async fn reconcile_test_tables(
    listener: &Endpoint,
    dialer: &Endpoint,
    source: &mut TableTestStore,
    destination: &mut TableTestStore,
) -> ReconcileReport {
    let client = connect_and_table_reconcile(
        dialer,
        direct_addr(listener),
        destination,
        || NOW,
        MAX_RECONCILE_ROUNDS,
    );
    tokio::pin!(client);
    loop {
        tokio::select! {
            report = &mut client => break report.unwrap(),
            _ = accept_test_table_sync(listener, source) => {},
        }
    }
}

#[tokio::test]
async fn table_reconcile_transfers_only_the_scoped_intersection_over_iroh() {
    let account = [0xa3; 32];
    let shared = table_item("repo-shared", 1);
    let source_only = table_item("repo-source", 2);
    let destination_only = table_item("repo-destination", 3);
    let shared_entry = test_entry(11);
    let private_entry = test_entry(12);
    let dialer_entry = test_entry(13);
    let mut source = TableTestStore::new(account, vec![source_only.clone(), shared.clone()], [
        (shared.stream_id, shared_entry.clone()),
        (source_only.stream_id, private_entry),
    ]);
    let mut destination = TableTestStore::new(account, vec![shared.clone(), destination_only], [(
        shared.stream_id,
        dialer_entry.clone(),
    )]);
    let (listener, dialer) = loopback_endpoints().await;

    let report = reconcile_test_tables(&listener, &dialer, &mut source, &mut destination).await;
    assert_eq!(report.rounds, 2, "one round transfers, one confirms the fixpoint");
    assert!(report.converged);
    assert_eq!(report.entries_newly_stored, 1);
    assert_eq!(report.entries_sent, 1);
    assert_eq!(destination.entries[&shared.stream_id][&shared_entry.0], shared_entry.1);
    assert_eq!(
        source.entries[&shared.stream_id][&dialer_entry.0], dialer_entry.1,
        "the acceptor stores the dialer's push before the dialer closes",
    );
    assert!(
        !destination.entries.contains_key(&source_only.stream_id),
        "a repo outside the manifest intersection never crosses the connection",
    );

    let again = reconcile_test_tables(&listener, &dialer, &mut source, &mut destination).await;
    assert_eq!(again.rounds, 1, "an idempotent replay is immediately quiet");
    assert!(again.converged);
    assert_eq!(again.entries_newly_stored, 0);
    assert_eq!(again.entries_sent, 0);
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
        nonce: [0x99; 32],
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
