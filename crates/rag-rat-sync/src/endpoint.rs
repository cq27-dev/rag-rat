//! The iroh endpoint adapter (phase D, #406).
//!
//! Binds a QUIC endpoint that speaks [`SYNC_ALPN`] over a pinned relay, and runs one
//! [`run_session`] per connection. iroh's stream types implement
//! `tokio::io::AsyncRead`/`AsyncWrite`, so the bi- stream drops straight into the
//! transport-agnostic session with no adapter shims.
//!
//! `Endpoint::builder(presets::Minimal)` is deliberate: Minimal disables the public n0 node
//! directory, so discovery happens ONLY through the relay this deployment pins — a peer is
//! reachable iff it shares the configured relay, never via a third-party lookup.
//!
//! # Authorization (#881)
//!
//! iroh authenticates the transport KEY; on top of that, both [`connect_and_sync`] and
//! [`accept_and_sync`] run the mutual node-authorization handshake ([`run_auth_phase`]) BEFORE any
//! inventory is exchanged. Under [`AuthPolicy::Closed`] a peer is admitted only if it presents a
//! signed binding proving its authenticated node id belongs to a roster device of the account;
//! under [`AuthPolicy::Open`] an acceptor admits any dialer to read but rejects its entry frames
//! without a write-capable roster role. A fresh dialer may accept entries from the transport-pinned
//! server it explicitly selected so it can restore the roster; storage still verifies every entry.
//! The ONBOARDING case uses the separate [`ENROLL_ALPN`] exchange: an owner atomically redeems a
//! one-time invite into a roster `DeviceAdd` before normal sync authentication can admit the new
//! device.

use std::collections::HashSet;
use std::str::FromStr;

use iroh::endpoint::{Connection as IrohConnection, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use rag_rat_oplog::{self, AccountId, DeviceFingerprint};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tokio::time::timeout;

use crate::auth::{
    AuthConfig, AuthPolicy, AuthRole, DEFAULT_PRE_AUTH_TIMEOUT, NodeAuth, PeerAdmission, Selected,
    run_auth_phase, run_auth_phase_selected,
};
use crate::enrollment::{
    ENROLL_ALPN, EnrollmentAcceptorOutcome, EnrollmentReceipt, EnrollmentRequest, InviteError,
    RESPONSE_ACK, RESPONSE_ACK_TIMEOUT, run_enrollment_acceptor, run_enrollment_dialer,
};
use crate::session::{
    DEFAULT_IDLE_TIMEOUT, ServeScope, SessionError, SessionReport, SyncStore, run_session,
    run_session_limited,
};
use crate::store::{OplogContentSyncStore, OplogSyncStore};
use crate::table_session::{
    TableSessionError, TableSessionReport, TableSyncStore, run_table_session,
};
use crate::table_wire::TABLE_SYNC_ALPN;
use crate::wire::{CONTENT_SYNC_ALPN, SYNC_ALPN};

/// Endpoint construction or connection setup failed, before a session could run.
#[derive(Debug)]
pub enum EndpointError {
    /// The configured relay URL did not parse.
    RelayUrl(String),
    /// Binding the endpoint failed (socket, TLS, relay handshake).
    Bind(String),
    /// Dialling a peer, or accepting an inbound connection, failed.
    Connect(String),
    /// A configured peer node id did not parse.
    PeerId(String),
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EndpointError::RelayUrl(m) => write!(f, "invalid relay url: {m}"),
            EndpointError::Bind(m) => write!(f, "binding the sync endpoint failed: {m}"),
            EndpointError::Connect(m) => write!(f, "sync connection setup failed: {m}"),
            EndpointError::PeerId(m) => write!(f, "invalid peer node id: {m}"),
        }
    }
}

impl std::error::Error for EndpointError {}

/// How long an ACCEPTOR waits for the dialer to close the connection before closing from its own
/// side. A QUIC `close()` discards in-flight stream data, so the side that STREAMED a response must
/// not close until the dialer has read it — the dialer closes once its `run_session` finished
/// reading, and this bounds the wait so a vanished dialer can never wedge the acceptor.
const GRACEFUL_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Bind a sync endpoint pinned to `relay_url`, with a stable node id derived from `secret_key` (the
/// 32-byte ed25519 seed of the transport identity — its own key, distinct from any account device
/// key). The same seed yields the same node id across launches, so a peer's ticket stays valid.
pub async fn build_endpoint(
    secret_key: [u8; 32],
    relay_url: &str,
) -> Result<Endpoint, EndpointError> {
    let relay_url =
        RelayUrl::from_str(relay_url.trim()).map_err(|e| EndpointError::RelayUrl(e.to_string()))?;
    Endpoint::builder(presets::Minimal)
        .alpns(vec![
            SYNC_ALPN.to_vec(),
            CONTENT_SYNC_ALPN.to_vec(),
            TABLE_SYNC_ALPN.to_vec(),
            ENROLL_ALPN.to_vec(),
        ])
        .relay_mode(RelayMode::custom([relay_url]))
        .secret_key(SecretKey::from_bytes(&secret_key))
        .bind()
        .await
        .map_err(|e| EndpointError::Bind(e.to_string()))
}

/// The iroh node id (public-key bytes) a `secret_key` yields — the exact id [`build_endpoint`]
/// would bind. Lets a caller check its own transport identity WITHOUT binding an endpoint (no
/// socket, no relay traffic), e.g. to gate on roster-effectiveness before paying for a bind.
pub fn node_id_from_secret(secret_key: [u8; 32]) -> [u8; 32] {
    *SecretKey::from_bytes(&secret_key).public().as_bytes()
}

/// Build a dialable [`EndpointAddr`] from a peer's node id (the 64-char lowercase hex form
/// `endpoint.id()` prints; standard base32 is also accepted) and the shared relay URL. The
/// device-side sync driver configures server peers by node id and reaches each through the pinned
/// relay — the CLI stays iroh-free by going through here.
pub fn peer_addr(node_id: &str, relay_url: &str) -> Result<EndpointAddr, EndpointError> {
    let id =
        EndpointId::from_str(node_id.trim()).map_err(|e| EndpointError::PeerId(e.to_string()))?;
    let relay =
        RelayUrl::from_str(relay_url.trim()).map_err(|e| EndpointError::RelayUrl(e.to_string()))?;
    Ok(EndpointAddr::new(id).with_relay_url(relay))
}

/// Build a dialable [`EndpointAddr`] from a peer's node id BYTES — the form an
/// [`crate::EnrollmentTicket`] carries — and the shared relay URL. The byte-oriented sibling of
/// [`peer_addr`], so the enrollment CLI can dial a ticket's inviter without naming an iroh type.
pub fn peer_addr_from_bytes(
    node_id: &[u8; 32],
    relay_url: &str,
) -> Result<EndpointAddr, EndpointError> {
    let id = EndpointId::from_bytes(node_id).map_err(|e| EndpointError::PeerId(e.to_string()))?;
    let relay =
        RelayUrl::from_str(relay_url.trim()).map_err(|e| EndpointError::RelayUrl(e.to_string()))?;
    Ok(EndpointAddr::new(id).with_relay_url(relay))
}

/// The peers one device-side sync pass should dial, plus the configured ids it could not use.
#[derive(Debug, Default, Clone)]
pub struct DiscoveredPeers {
    /// Each dialable address paired with the node-id string naming it, for logging. Configured
    /// peers come first and win on collision, so a pass that discovers nothing behaves exactly as
    /// it did before discovery existed.
    pub peers: Vec<(String, EndpointAddr)>,
    /// Configured ids that did not parse, each already logged.
    ///
    /// Deliberately NOT recoverable as `configured.len() - peers.len()`: discovery can add peers,
    /// and two configured spellings of one node collapse into a single entry without either being
    /// unresolved. A caller that subtracts under-counts its errors and reports a healthy-looking
    /// pass over an all-typo peer list.
    pub unresolved_configured: usize,
}

/// Resolve the peers a device-side sync should dial: the explicitly configured ones, plus whatever
/// the account advertises to the peer-discovery service.
///
/// Configured peers are first-class and unchanged — discovery is purely additive, and `discovery:
/// None` reduces this to the configured resolver. A configured id that does not parse is logged
/// and counted rather than aborting the pass, so one typo cannot suppress every other peer.
///
/// `open_announcement` recovers a node id from a sealed announcement, or `None` for one this
/// device cannot read. It is a parameter rather than something this crate does itself because
/// sealing is the op-log crate's concern; passing a closure that always returns `None` reduces this
/// to the configured-peer resolver.
///
/// **Everything is compared on the raw 32 bytes, never the display string.**
/// [`iroh::EndpointId::from_str`] accepts 64-char lowercase hex (the `Display` form) OR standard
/// base32, and uppercases before base32-decoding, so three distinct strings can name one peer —
/// while the config layer only trims and de-duplicates literally. Comparing strings would dial such
/// a peer twice per pass (two full multi-ALPN reconciles) and double-count it in `ok`/`errors`.
pub async fn discover_peers(
    configured_peers: &[String],
    relay_url: &str,
    discovery: Option<crate::discovery::DiscoveryExchange<'_>>,
    open_announcement: &dyn Fn(&[u8]) -> Option<[u8; 32]>,
) -> DiscoveredPeers {
    let mut peers: Vec<(String, EndpointAddr)> = Vec::new();
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut unresolved_configured = 0usize;

    for peer in configured_peers {
        match peer_addr(peer, relay_url) {
            Ok(addr) =>
                if seen.insert(*addr.id.as_bytes()) {
                    peers.push((peer.clone(), addr));
                } else {
                    tracing::debug!(
                        peer,
                        "skipping a configured sync peer another entry already names"
                    );
                },
            Err(error) => {
                tracing::warn!(peer, %error, "skipping a configured sync peer with an invalid node id");
                unresolved_configured += 1;
            },
        }
    }

    let Some(discovery) = discovery else {
        return DiscoveredPeers { peers, unresolved_configured };
    };
    // Read the local id BEFORE the exchange consumes the params. Self-exclusion is not optional:
    // advertising this node is precisely what puts it in the set it then fetches back.
    let local_node = *discovery.endpoint.id().as_bytes();
    let outcome = crate::discovery::exchange(discovery).await;
    if let Some(degraded) = &outcome.degraded {
        // Never fatal — the configured peers are dialed regardless. See the discovery module docs.
        tracing::warn!(
            degraded,
            "peer discovery degraded; continuing with the peers already resolved"
        );
    }
    // Announcements arrive sealed. Opening happens HERE rather than inside the exchange because it
    // needs a database connection, which is not `Sync` and so cannot cross the await inside the
    // spawned advertise loop that shares that code.
    //
    // Failures are INDIVIDUAL throughout the loop below. Failing the batch would let one bad
    // entry, which anyone able to compute the tag may publish, hide every good one.
    // The peer cap is applied HERE, to announcements that actually resolved, and not in the
    // exchange to raw payloads. Capping payloads would let anyone able to compute the tag suppress
    // every real advertiser with a handful of unopenable entries — see `MAX_ANNOUNCEMENTS`.
    let mut admitted = 0usize;
    for payload in &outcome.announcements {
        if admitted >= crate::discovery::MAX_ANNOUNCEMENTS {
            tracing::debug!(
                cap = crate::discovery::MAX_ANNOUNCEMENTS,
                "discovered the most peers one pass admits; ignoring the rest"
            );
            break;
        }
        // One that will not open is skipped without spending cap budget: it is sealed to a roster
        // this device has left, malformed, or from a newer version.
        let Some(node) = open_announcement(payload) else { continue };
        if node == local_node || !seen.insert(node) {
            continue;
        }
        match peer_addr_from_bytes(&node, relay_url) {
            // The label comes off the parsed id rather than a second fallible conversion.
            Ok(addr) => {
                peers.push((addr.id.to_string(), addr));
                admitted += 1;
            },
            // `from_bytes` rejects a non-canonical point. Dropped individually: anyone who can
            // compute the tag can publish garbage, and one bad entry must not hide the good ones.
            Err(error) =>
                tracing::warn!(%error, "dropping a discovered peer with an unusable node id"),
        }
    }
    DiscoveredPeers { peers, unresolved_configured }
}

/// The dialable node-id string for a peer's node id BYTES — the inverse of the byte form a ticket
/// or discovery record carries, in the lowercase-hex shape `[sync] server_peers` accepts. Lets the
/// enrollment CLI print a joinable peer id without naming an iroh type.
pub fn node_id_to_string(node_id: &[u8; 32]) -> Result<String, EndpointError> {
    EndpointId::from_bytes(node_id)
        .map(|id| id.to_string())
        .map_err(|e| EndpointError::PeerId(e.to_string()))
}

/// This endpoint's dialable address — hand it (or a ticket wrapping it) to a peer so it can
/// [`connect_and_sync`] back.
pub fn endpoint_addr(endpoint: &Endpoint) -> EndpointAddr {
    endpoint.addr()
}

/// Dial an owner over the dedicated enrollment ALPN, verify the founder-signed receipt, ingest its
/// account bootstrap, and adopt that accepted genesis locally before returning. The next Closed
/// account-log session can therefore mutually authorize without a fresh-device bootstrap deadlock.
pub async fn connect_and_enroll(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    database: &Connection,
    expected_account: AccountId,
    request: &EnrollmentRequest,
    now_ms: i64,
) -> Result<EnrollmentReceipt, InviteError> {
    // The budget and held-entry inventory are always recomputed from the local store — the
    // acceptor measures its exact receipt against them before consuming the nonce — so the
    // values the caller set are irrelevant.
    //
    // The declaration is a point-in-time read: a competing LOCAL write that shrinks capacity
    // before adoption could still burn the nonce. Do NOT fix that by holding the database's
    // writer reservation across the network exchange — a slow or malicious owner can stretch
    // the transfer (per-chunk progress windows) and starve every local writer past its busy
    // timeout. The caller MUST instead serialize capacity-consuming local writes across this
    // call — the same per-database sync-session lock `sync serve` and device sync already
    // respect — which the enrollment CLI flow holds for the whole enrollment.
    let mut request = request.clone();
    request.expected_account = expected_account;
    // The QUIC connection authenticates as THIS endpoint's transport identity, so the request
    // must name it — a caller-supplied value is either redundant or a guaranteed WrongNode.
    request.transport_node_id = *endpoint.id().as_bytes();
    request.budget = rag_rat_oplog::enrollment_budget(database, expected_account, now_ms)?;
    request.held_entry_hashes =
        rag_rat_oplog::held_account_entry_hashes(database, expected_account)?;
    validate_enrollment_request_identity(database, expected_account, &request, now_ms)?;
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, endpoint.connect(peer, ENROLL_ALPN))
        .await
        .map_err(|_| InviteError::Storage(anyhow::anyhow!("enrollment dial timed out")))?
        .map_err(|error| InviteError::Storage(error.into()))?;
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.open_bi())
        .await
        .map_err(|_| InviteError::Storage(anyhow::anyhow!("opening enrollment stream timed out")))?
        .map_err(|error| InviteError::Storage(error.into()))?;
    let receipt = run_enrollment_dialer(&mut recv, &mut send, expected_account, &request).await?;
    let genesis_hash = rag_rat_oplog::verify_enrollment_device_add(
        &receipt.account_entries,
        expected_account,
        receipt.device_add_hash,
        &receipt.device_add_signed,
        request.ed25519_pubkey,
        request.x25519_pubkey,
    )
    .map_err(|error| InviteError::Malformed(format!("invalid enrollment receipt: {error}")))?;
    let fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(request.ed25519_pubkey).into());
    rag_rat_oplog::adopt_enrollment_bootstrap(database, rag_rat_oplog::EnrollmentBootstrap {
        account_entries: &receipt.account_entries,
        account_id: expected_account,
        genesis_hash,
        device_fingerprint: fingerprint,
        device_add_hash: receipt.device_add_hash,
        now_ms,
    })?;
    // Best-effort maintenance AFTER the durable adoption: retry parked rows the receipt's keys
    // may now resolve. Kept out of the one-time adoption transaction so a newly resolvable
    // parked sibling can never enter its fold, and a maintenance failure cannot invalidate the
    // completed enrollment (normal sync retries the same queues later).
    if let Err(error) =
        rag_rat_oplog::retry_enrollment_pre_verify(database, expected_account, now_ms)
    {
        tracing::warn!(%error, "post-enrollment pre-verify retry failed");
    }
    conn.close(0u32.into(), b"done");
    Ok(receipt)
}

/// Refuse a request assembled for a different store before making network contact: redemption is
/// one-shot, and adopting a receipt whose device keys are not locally held would leave Closed sync
/// unable to authenticate or decrypt the delivered stream-key wraps.
fn validate_enrollment_request_identity(
    database: &Connection,
    expected_account: AccountId,
    request: &EnrollmentRequest,
    now_ms: i64,
) -> Result<(), InviteError> {
    if let Some(existing_account) = rag_rat_oplog::read_local_account(database)?
        && existing_account != expected_account
    {
        return Err(InviteError::Malformed(
            "enrollment account does not match the store's existing local account".into(),
        ));
    }
    let local = rag_rat_oplog::local_device(database, now_ms)?;
    if request.ed25519_pubkey != local.ed25519_public_key() {
        return Err(InviteError::Malformed(
            "enrollment request ed25519 key does not match the local device identity".into(),
        ));
    }
    if request.x25519_pubkey != local.x25519_public_key() {
        return Err(InviteError::Malformed(
            "enrollment request X25519 key does not match the local device identity".into(),
        ));
    }
    Ok(())
}

/// Close an enrollment connection after the exchange. The dialer acks the response the moment it
/// is DECODED, so the ack byte is the delivery signal for both outcomes: once it lands, close
/// immediately — an enrolled receipt needs no graceful-close wait, and a refused peer
/// (unauthenticated; any random nonce reaches a refusal) is bounded by [`RESPONSE_ACK_TIMEOUT`]
/// instead of holding the serial accept loop for [`GRACEFUL_CLOSE_TIMEOUT`]. An invite-holding
/// peer that never acks still gets the graceful-close fallback so its streamed receipt lands.
async fn close_enrollment_connection(
    conn: iroh::endpoint::Connection,
    recv: &mut iroh::endpoint::RecvStream,
    enrolled: bool,
) {
    let mut ack = [0u8; 1];
    let delivered =
        matches!(timeout(RESPONSE_ACK_TIMEOUT, recv.read_exact(&mut ack)).await, Ok(Ok(_)))
            && ack == [RESPONSE_ACK];
    if enrolled && !delivered {
        let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    }
    conn.close(0u32.into(), b"done");
}

/// Accept one owner-side enrollment connection and atomically redeem its invite.
pub async fn accept_enrollment(
    endpoint: &Endpoint,
    database: &Connection,
    now_ms: impl Fn() -> i64,
) -> Result<EnrollmentReceipt, InviteError> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| InviteError::Storage(anyhow::anyhow!("endpoint closed")))?;
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, incoming)
        .await
        .map_err(|_| InviteError::Storage(anyhow::anyhow!("enrollment handshake timed out")))?
        .map_err(|error| InviteError::Storage(error.into()))?;
    if conn.alpn() != ENROLL_ALPN {
        conn.close(0u32.into(), b"wrong-alpn");
        return Err(InviteError::Malformed("connection did not negotiate enrollment ALPN".into()));
    }
    let remote_node = *conn.remote_id().as_bytes();
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| InviteError::Storage(anyhow::anyhow!("peer opened no enrollment stream")))?
        .map_err(|error| InviteError::Storage(error.into()))?;
    let outcome =
        run_enrollment_acceptor(&mut recv, &mut send, database, remote_node, now_ms).await?;
    let enrolled = matches!(outcome, EnrollmentAcceptorOutcome::Enrolled(..));
    close_enrollment_connection(conn, &mut recv, enrolled).await;
    match outcome {
        EnrollmentAcceptorOutcome::Enrolled(receipt, _) => Ok(receipt),
        EnrollmentAcceptorOutcome::Refused(error) => Err(error),
    }
}

/// Dial `peer`, authorize each other under `policy`, then run one sync session, returning what
/// moved. The auth handshake (#881) runs BEFORE any inventory: the dialer presents its binding,
/// then verifies the acceptor's before revealing anything, so a poisoned address never leaks the
/// account log to an impostor.
pub async fn connect_and_sync<S: SyncStore + NodeAuth>(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    alpn: &[u8],
    store: &mut S,
    policy: AuthPolicy,
    now_ms: i64,
) -> Result<SessionReport, SyncFailure> {
    let local_node = *endpoint.id().as_bytes();
    // The `alpn` selects the STREAM the dialer wants — [`SYNC_ALPN`] for the account log,
    // [`CONTENT_SYNC_ALPN`] for `/3` content — and must match `store`'s type. The acceptor routes
    // to the matching store by the negotiated ALPN.
    //
    // Bound every peer-controlled wait explicitly (mirroring `accept_and_sync`), rather than
    // inheriting a transport dependency's idle default: a dead or unreachable configured peer must
    // fail this dial promptly — a device-side sync holds the per-database session lock while it
    // runs.
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, endpoint.connect(peer, alpn))
        .await
        .map_err(|_| SyncFailure::Endpoint(EndpointError::Connect("dial timed out".into())))?
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    let remote_node = *conn.remote_id().as_bytes();
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.open_bi())
        .await
        .map_err(|_| {
            SyncFailure::Endpoint(EndpointError::Connect("opening a stream timed out".into()))
        })?
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    let (capabilities, _admission) = run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
        role: AuthRole::Dialer,
        account_id: store.account_id(),
        local_node,
        remote_node,
        policy,
        now_ms,
        pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
    })
    .await
    .map_err(SyncFailure::Auth)?;
    let report = run_session(store, send, recv, AuthRole::Dialer, capabilities)
        .await
        .map_err(SyncFailure::Session)?;
    // The role-ordered completion acknowledgement proves the acceptor consumed everything we
    // pushed before replying, and we consumed its whole stream before sending our acknowledgement.
    // The dialer therefore closes only after both directions are delivered.
    conn.close(0u32.into(), b"done");
    Ok(report)
}

/// Dial the dedicated table-sync ALPN, run the existing mutual account auth under closed-roster
/// policy, then reconcile the bounded manifest intersection.
pub async fn connect_and_table_sync<S: TableSyncStore + NodeAuth>(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    store: &mut S,
    now_ms: i64,
) -> Result<TableSessionReport, SyncFailure> {
    let local_node = *endpoint.id().as_bytes();
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, endpoint.connect(peer, TABLE_SYNC_ALPN))
        .await
        .map_err(|_| {
            SyncFailure::Endpoint(EndpointError::Connect("table-sync dial timed out".into()))
        })?
        .map_err(|error| SyncFailure::Endpoint(EndpointError::Connect(error.to_string())))?;
    let remote_node = *conn.remote_id().as_bytes();
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.open_bi())
        .await
        .map_err(|_| {
            SyncFailure::Endpoint(EndpointError::Connect(
                "opening a table-sync stream timed out".into(),
            ))
        })?
        .map_err(|error| SyncFailure::Endpoint(EndpointError::Connect(error.to_string())))?;
    let (capabilities, _admission) = run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
        role: AuthRole::Dialer,
        account_id: store.account_id(),
        local_node,
        remote_node,
        policy: AuthPolicy::Closed,
        now_ms,
        pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
    })
    .await
    .map_err(SyncFailure::Auth)?;
    let report = run_table_session(store, send, recv, AuthRole::Dialer, capabilities)
        .await
        .map_err(SyncFailure::TableSession)?;
    conn.close(0u32.into(), b"done");
    Ok(report)
}

/// The most times a dialer re-runs a sync session against ONE peer before giving up on convergence
/// for this pass. A cooperative account reaches a fixpoint in a handful of rounds; a peer still not
/// dry after this many re-syncs is withholding an authorizer or is pathologically active, so stop
/// with `converged = false` and let the next maintenance pass continue — device-side sync re-runs
/// the loop on its cadence, so a capped pass only defers the remainder, it never loses it.
pub const MAX_RECONCILE_ROUNDS: usize = 8;

/// What a multi-round reconciliation moved (aggregated across its rounds). `converged` is true when
/// the loop stopped on a fully quiet round — nothing stored, sent, or received in either direction,
/// so both peers hold the union of what each offered. It is false when the round cap was hit first:
/// the store may still be incomplete and a later pass should continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    pub rounds: usize,
    pub entries_newly_stored: usize,
    pub entries_sent: usize,
    pub converged: bool,
}

/// Whether reconciliation should run another round, given the round just completed (#878). A single
/// `run_session` can report `Done` while a store is still incomplete — an adversarial sender
/// streaming dependents before their authorizer overruns the pre-verify eviction budget (only the
/// survivors promote), and even a cooperative large account may not reach a fixpoint in one pass.
/// Re-running converges: each store grows monotonically and every hello advertises parked entries
/// (#877), so a re-sent dependent promotes once its authorizer is present on that side.
///
/// The fixpoint is a fully QUIET round — nothing stored, sent, OR received. All three matter:
/// `sent` catches a PUSH that made the acceptor evict (the confirmation quiet round proves the
/// re-pushed entries finally landed), `received` catches a peer that still has data for us, and
/// `stored` catches local promotion progress. This is reliable because a store under
/// [`MAX_HELLO_HASHES`] (65_536) advertises its COMPLETE inventory — parked entries included — so
/// the counters reflect real gaps, not redelivery, for every store D targets. A store OVER that cap
/// advertises only a bounded inventory, so a peer may re-offer already-held entries every round and
/// the session never fully quiets; the round cap then stops it with `converged = false`, the honest
/// outcome given the deliberate absence of a remainder-reconcile protocol for such stores.
fn reconcile_step(report: &SessionReport, rounds_done: usize, max_rounds: usize) -> ReconcileStep {
    let quiet = report.entries_newly_stored == 0
        && report.entries_sent == 0
        && report.entries_received == 0;
    if quiet {
        ReconcileStep::Stop { converged: true }
    } else if rounds_done >= max_rounds {
        // Still moving at the cap: the store may not be complete yet — a later maintenance pass
        // continues from where this left off.
        ReconcileStep::Stop { converged: false }
    } else {
        ReconcileStep::Continue
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileStep {
    Continue,
    Stop { converged: bool },
}

/// Dial `peer` repeatedly, running one [`connect_and_sync`] session per round until the transfer
/// reaches a fixpoint or the round cap — the multi-round reconciliation #878 needs so a single
/// session's `Done` is never mistaken for a complete store. The serve acceptor already loops
/// accepting connections, so this dialer-side loop is all it takes: no wire change. `now_ms` is
/// read fresh each round so every re-dialed session stamps the time it actually ran (a long
/// reconciliation must not authorize against a pre-loop timestamp).
pub async fn connect_and_reconcile<S: SyncStore + NodeAuth>(
    endpoint: &Endpoint,
    peer: EndpointAddr,
    alpn: &[u8],
    store: &mut S,
    policy: AuthPolicy,
    now_ms: impl Fn() -> i64,
    max_rounds: usize,
) -> Result<ReconcileReport, SyncFailure> {
    let mut entries_newly_stored = 0;
    let mut entries_sent = 0;
    let mut rounds = 0;
    loop {
        let report =
            connect_and_sync(endpoint, peer.clone(), alpn, store, policy, now_ms()).await?;
        rounds += 1;
        entries_newly_stored += report.entries_newly_stored;
        entries_sent += report.entries_sent;
        if let ReconcileStep::Stop { converged } = reconcile_step(&report, rounds, max_rounds) {
            return Ok(ReconcileReport { rounds, entries_newly_stored, entries_sent, converged });
        }
    }
}

/// Re-run table sessions until the same fully-quiet fixpoint used by account/content sync.
pub async fn connect_and_table_reconcile<S: TableSyncStore + NodeAuth>(
    endpoint: &Endpoint,
    peer: EndpointAddr,
    store: &mut S,
    now_ms: impl Fn() -> i64,
    max_rounds: usize,
) -> Result<ReconcileReport, SyncFailure> {
    let mut entries_newly_stored = 0;
    let mut entries_sent = 0;
    let mut rounds = 0;
    loop {
        let report = connect_and_table_sync(endpoint, peer.clone(), store, now_ms()).await?;
        rounds += 1;
        entries_newly_stored += report.entries_newly_stored;
        entries_sent += report.entries_sent;
        let session = SessionReport {
            entries_sent: report.entries_sent,
            entries_received: report.entries_received,
            entries_newly_stored: report.entries_newly_stored,
        };
        let step = if report.continuation_pending {
            if rounds >= max_rounds {
                ReconcileStep::Stop { converged: false }
            } else {
                ReconcileStep::Continue
            }
        } else {
            reconcile_step(&session, rounds, max_rounds)
        };
        if let ReconcileStep::Stop { converged } = step {
            return Ok(ReconcileReport { rounds, entries_newly_stored, entries_sent, converged });
        }
    }
}

/// Accept ONE inbound connection and run a session against it. D4's `sync serve` loops this;
/// keeping it single-shot here keeps the store's `!Send` connection on one task (no spawn).
///
/// Every pre-session wait a peer controls — the handshake and opening the bidirectional stream — is
/// bounded by [`DEFAULT_IDLE_TIMEOUT`]. `run_session`'s own idle timeout only starts once the
/// stream is open, so without these a peer that connects and then stalls (never opening a stream)
/// would hold this single-session server forever, blocking every later peer.
///
/// `now_ms` is a CLOCK, read once a peer has connected — never a timestamp captured before the
/// accept wait. A server may idle arbitrarily long between connections, so the auth phase must
/// stamp and verify bindings against the time the connection actually arrived (see the read below).
pub async fn accept_and_sync<S: SyncStore + NodeAuth>(
    endpoint: &Endpoint,
    store: &mut S,
    policy: AuthPolicy,
    now_ms: impl Fn() -> i64,
) -> Result<SessionReport, SyncFailure> {
    let local_node = *endpoint.id().as_bytes();
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| SyncFailure::Endpoint(EndpointError::Connect("endpoint closed".into())))?;
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, incoming)
        .await
        .map_err(|_| SyncFailure::Endpoint(EndpointError::Connect("handshake timed out".into())))?
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    let remote_node = *conn.remote_id().as_bytes();
    // This single-stream acceptor serves ONLY the account-log ALPN. The endpoint binds the content
    // ALPN too (for `accept_and_dispatch`), so a content client could negotiate it and land here —
    // reject it rather than run a content connection against the account-log store.
    if conn.alpn() != SYNC_ALPN {
        conn.close(0u32.into(), b"wrong-alpn");
        return Err(SyncFailure::Endpoint(EndpointError::Connect(format!(
            "this acceptor serves only the account-log ALPN, got {:?}",
            conn.alpn()
        ))));
    }
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| SyncFailure::Endpoint(EndpointError::Connect("peer opened no stream".into())))?
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    // Read the clock only now that a peer has connected — NOT before the accept wait above. A
    // long-idle server whose stamp/verify time predated the wait would treat a peer's freshly
    // minted binding (and its own) as future-skewed and reject the session.
    let now_ms = now_ms();
    // Authorize the dialer BEFORE run_session so no inventory (not even account confirmation)
    // leaves this peer until the remote passes our policy (#881).
    let (capabilities, _admission) = run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
        role: AuthRole::Acceptor,
        account_id: store.account_id(),
        local_node,
        remote_node,
        policy,
        now_ms,
        pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
    })
    .await
    .map_err(SyncFailure::Auth)?;
    let report = run_session(store, send, recv, AuthRole::Acceptor, capabilities)
        .await
        .map_err(SyncFailure::Session)?;
    // The acceptor sends the final completion acknowledgement. Keep the connection alive until the
    // dialer reads it and closes, bounded so a vanished dialer cannot wedge the server.
    let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    conn.close(0u32.into(), b"done");
    Ok(report)
}

/// Accept ONE inbound connection and run the session for the STREAM the peer negotiated: the
/// account log ([`SYNC_ALPN`] → `account_store`), `/3` content ([`CONTENT_SYNC_ALPN`] →
/// `content_store`), repo-scoped `/5` tables ([`TABLE_SYNC_ALPN`]), or owner-side enrollment
/// ([`ENROLL_ALPN`] → the account store's database).
/// The auth phase is account-level for normal sync; enrollment instead authenticates the requested
/// node by the QUIC transport identity and atomically adds it to the roster before normal auth can
/// admit it.
pub async fn accept_and_dispatch<C>(
    endpoint: &Endpoint,
    account_store: &mut OplogSyncStore<'_>,
    content_store: &mut C,
    policy: AuthPolicy,
    now_ms: impl Fn() -> i64 + Copy,
) -> Result<(Vec<u8>, SessionReport), SyncFailure>
where
    C: SyncStore,
{
    let local_node = *endpoint.id().as_bytes();
    let conn = accept_connection(endpoint).await?;
    // Unmetered convenience/test wrapper — the live serve loops (resident host, `sync serve`) call
    // `dispatch_connection` directly with the shared egress limiter.
    dispatch_connection(conn, local_node, account_store, content_store, policy, now_ms, None).await
}

/// Accept and complete the transport handshake for one inbound connection. Kept separate from
/// [`dispatch_connection`] so a resident host can keep accepting while prior sessions reconcile.
pub async fn accept_connection(endpoint: &Endpoint) -> Result<IrohConnection, SyncFailure> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| SyncFailure::Endpoint(EndpointError::Connect("endpoint closed".into())))?;
    timeout(DEFAULT_IDLE_TIMEOUT, incoming)
        .await
        .map_err(|_| SyncFailure::Endpoint(EndpointError::Connect("handshake timed out".into())))?
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))
}

/// Inbound connections admitted per second in steady state once the burst is spent. Sized to clear
/// legitimate multi-peer inbound (a joiner opens ~4 rapid connections — enrollment + account log +
/// content + table) while bounding a flood. Implementation-local constant, not config.
const ACCEPT_REFILL_PER_SEC: f64 = 8.0;
/// Maximum inbound connections admitted in one instantaneous burst.
const ACCEPT_BURST: f64 = 32.0;

/// Bytes served per second in steady state once the burst is spent — the sustained egress ceiling a
/// public serving host allows across ALL peers. A text knowledge base is small, so a legitimate
/// full pull clears the burst instantly; the ceiling bounds an anonymous peer that re-pulls to
/// drain upload bandwidth. Implementation-local constants, not config (a host can be given a knob
/// later).
const EGRESS_REFILL_BYTES_PER_SEC: f64 = 16.0 * 1024.0 * 1024.0;
/// Bytes servable in one instantaneous burst before the steady-state ceiling applies.
const EGRESS_BURST_BYTES: f64 = 64.0 * 1024.0 * 1024.0;

/// A GLOBAL inbound-connection rate limiter: one token bucket bounding total accept rate regardless
/// of peer identity. Per-peer-by-node-id limiting is the wrong lever — an iroh node id is a keypair
/// mintable in microseconds, so a flood rotates ids and evades any per-id bucket while making the
/// id map its own memory/eviction target. The Sybil-resistant bound is global and refused before
/// the handshake. In-memory and transient by design: a restart resetting the window to full burst
/// is correct for a live-traffic control (contrast the durable byte ceilings on the ingest paths).
#[derive(Debug)]
pub struct GlobalAcceptRateLimiter {
    tokens: f64,
    burst: f64,
    refill_per_sec: f64,
    last_ms: Option<i64>,
}

impl Default for GlobalAcceptRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalAcceptRateLimiter {
    pub fn new() -> Self {
        Self {
            tokens: ACCEPT_BURST,
            burst: ACCEPT_BURST,
            refill_per_sec: ACCEPT_REFILL_PER_SEC,
            last_ms: None,
        }
    }

    /// Refill by the time elapsed since the last call (capped at `burst`, so idle time never
    /// accrues unbounded credit), then spend one token. Returns `false` when the bucket is
    /// empty — the connection should be refused. `now_ms` is injected so refill is
    /// deterministically testable.
    pub fn allow(&mut self, now_ms: i64) -> bool {
        if let Some(last) = self.last_ms {
            let elapsed_secs = (now_ms - last).max(0) as f64 / 1000.0;
            self.tokens = (self.tokens + elapsed_secs * self.refill_per_sec).min(self.burst);
        }
        self.last_ms = Some(now_ms);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// A GLOBAL byte-rate limiter bounding total EGRESS (data served to peers) regardless of peer
/// identity — the anti-drain counterpart to [`GlobalAcceptRateLimiter`]. Global, not per-peer, for
/// the same Sybil reason: a per-id budget is evaded by rotating node ids. Shared across the host's
/// concurrent per-connection tasks (an `Arc<Mutex<_>>`), checked at each outgoing page. In-memory
/// and transient: a restart resetting to full burst is correct for live-traffic control.
#[derive(Debug)]
pub struct GlobalEgressLimiter {
    tokens: f64,
    burst: f64,
    refill_per_sec: f64,
    last_ms: Option<i64>,
}

impl Default for GlobalEgressLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalEgressLimiter {
    pub fn new() -> Self {
        Self {
            tokens: EGRESS_BURST_BYTES,
            burst: EGRESS_BURST_BYTES,
            refill_per_sec: EGRESS_REFILL_BYTES_PER_SEC,
            last_ms: None,
        }
    }

    /// Refill by elapsed time (capped at `burst`), then, IF any credit remains, spend `bytes` (the
    /// balance may go negative for an oversized page) and permit the page; otherwise refuse so the
    /// sender stops after the pages already sent. Permitting on ANY positive credit guarantees
    /// forward progress even for a page larger than the whole burst — a reader is never wedged,
    /// only throttled, and the unsent tail is re-offered by the next session's inventory diff.
    /// `now_ms` injected for deterministic tests.
    pub fn allow(&mut self, bytes: usize, now_ms: i64) -> bool {
        if let Some(last) = self.last_ms {
            let elapsed_secs = (now_ms - last).max(0) as f64 / 1000.0;
            self.tokens = (self.tokens + elapsed_secs * self.refill_per_sec).min(self.burst);
        }
        self.last_ms = Some(now_ms);
        if self.tokens > 0.0 {
            self.tokens -= bytes as f64;
            true
        } else {
            false
        }
    }
}

/// Accept one inbound connection, refusing it BEFORE the TLS handshake when the global accept-rate
/// `limiter` is exhausted. `Ok(None)` means the connection was refused by the rate limit — the
/// caller continues its accept loop; `Ok(Some(conn))` means admitted and handshaken. Refusing at
/// the `Incoming` stage costs no handshake CPU and reveals nothing to the peer.
pub async fn accept_connection_within_rate(
    endpoint: &Endpoint,
    limiter: &mut GlobalAcceptRateLimiter,
    now_ms: impl Fn() -> i64,
) -> Result<Option<IrohConnection>, SyncFailure> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| SyncFailure::Endpoint(EndpointError::Connect("endpoint closed".into())))?;
    // Read the clock only NOW that a peer has connected — `accept()` can idle arbitrarily long, and
    // a timestamp taken before the wait would under-credit the bucket's refill and wrongly
    // refuse a connection arriving after a load-then-idle stretch.
    if !limiter.allow(now_ms()) {
        incoming.refuse();
        return Ok(None);
    }
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, incoming)
        .await
        .map_err(|_| SyncFailure::Endpoint(EndpointError::Connect("handshake timed out".into())))?
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    Ok(Some(conn))
}

/// The serve scope an acceptor grants a peer (#407 E2b): narrow to [`ServeScope::PublicOnly`] iff a
/// `PublicRead` account admitted this peer by FALLBACK — i.e. an anonymous reader with no verified
/// binding. A verified member of a public account, and every `Open`/`Closed` session, serves the
/// full account. Derived from the SELECTED per-ALPN policy (tables are pinned `Closed`, so they
/// never reach public-only) and the auth admission outcome.
fn serve_scope_for(policy: AuthPolicy, admission: PeerAdmission) -> ServeScope {
    match (policy, admission) {
        (AuthPolicy::PublicRead, PeerAdmission::Fallback) => ServeScope::PublicOnly,
        _ => ServeScope::Full,
    }
}

/// Run the ALPN-selected sync session for an already-accepted connection.
pub async fn dispatch_connection<C>(
    conn: IrohConnection,
    local_node: [u8; 32],
    account_store: &mut OplogSyncStore<'_>,
    content_store: &mut C,
    policy: AuthPolicy,
    now_ms: impl Fn() -> i64 + Copy,
    egress: Option<std::sync::Arc<std::sync::Mutex<GlobalEgressLimiter>>>,
) -> Result<(Vec<u8>, SessionReport), SyncFailure>
where
    C: SyncStore,
{
    // The two stores MUST be for the same account: the auth phase authorizes the peer against the
    // account store's account, and a content connection then runs the content store — which would
    // serve the WRONG account's content if they differed. Our callers always pass same-account
    // stores; enforce it for the public generic API before any connection is accepted.
    if account_store.account_id() != content_store.account_id() {
        return Err(SyncFailure::Endpoint(EndpointError::Connect(
            "account and content stores are for different accounts".into(),
        )));
    }
    let remote_node = *conn.remote_id().as_bytes();
    let alpn = conn.alpn().to_vec();
    // Reject an unroutable ALPN BEFORE opening a stream or running auth — there is no reason to
    // complete a mutual handshake for a stream we can't serve. Unreachable today (iroh's TLS
    // refuses any ALPN `build_endpoint` didn't bind, and it binds exactly the routed ones), but
    // keeping the check ahead of auth means adding another bound ALPN without a route here
    // fails cleanly here instead of after the peer has completed authorization.
    if alpn.as_slice() != SYNC_ALPN
        && alpn.as_slice() != CONTENT_SYNC_ALPN
        && alpn.as_slice() != TABLE_SYNC_ALPN
        && alpn.as_slice() != ENROLL_ALPN
    {
        conn.close(0u32.into(), b"unknown-alpn");
        return Err(SyncFailure::Endpoint(EndpointError::Connect(format!(
            "peer negotiated an unknown ALPN {alpn:?}"
        ))));
    }
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| SyncFailure::Endpoint(EndpointError::Connect("peer opened no stream".into())))?
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    if alpn.as_slice() == ENROLL_ALPN {
        let enrollment_database = account_store.connection();
        // The acceptor consumes one of the enrollment database's OWN invites and authors the
        // DeviceAdd into ITS account, so unless that database's local account is exactly the one
        // the sync stores serve, a miswired dispatcher would enroll the device into an unrelated
        // account — and report that as a successful enrollment — while sync connections keep
        // serving the stores' account. Refuse BEFORE redemption (the irreversible boundary).
        let matches = enrollment_database_matches(enrollment_database, account_store.account_id())
            .map_err(|error| SyncFailure::Endpoint(EndpointError::Connect(error.to_string())))?;
        if !matches {
            conn.close(0u32.into(), b"enrollment-account-mismatch");
            return Err(SyncFailure::Endpoint(EndpointError::Connect(
                "enrollment database belongs to a different account than the sync stores".into(),
            )));
        }
        let outcome =
            run_enrollment_acceptor(&mut recv, &mut send, enrollment_database, remote_node, now_ms)
                .await
                .map_err(SyncFailure::Enrollment)?;
        let enrolled = matches!(outcome, EnrollmentAcceptorOutcome::Enrolled(..));
        close_enrollment_connection(conn, &mut recv, enrolled).await;
        return match outcome {
            EnrollmentAcceptorOutcome::Enrolled(_, _) => Ok((alpn, SessionReport::default())),
            EnrollmentAcceptorOutcome::Refused(error) => Err(SyncFailure::Enrollment(error)),
        };
    }
    // Read the clock only now that a peer has connected (see `accept_and_sync`).
    let auth_now_ms = now_ms();
    // Table streams are private account data. Open/bootstrap/public admission is only for the
    // account + content paths; a table manifest is never revealed to an unverified peer.
    let alpn_policy = if alpn.as_slice() == TABLE_SYNC_ALPN { AuthPolicy::Closed } else { policy };
    // The auth phase is store-agnostic (the binding is account-level), so authorize with the
    // account store BEFORE any inventory — no stream leaves this peer until it passes the policy.
    let (capabilities, admission) =
        run_auth_phase(&mut send, &mut recv, &*account_store, AuthConfig {
            role: AuthRole::Acceptor,
            account_id: account_store.account_id(),
            local_node,
            remote_node,
            policy: alpn_policy,
            now_ms: auth_now_ms,
            pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
        })
        .await
        .map_err(SyncFailure::Auth)?;
    // Narrow the serve to public-only for an anonymous (fallback-admitted) reader of a `PublicRead`
    // account (#407); a verified member — or any Open/Closed session — serves the full account. Set
    // on both stores; only the ALPN's store actually serves, and the store re-checks fully-public.
    let scope = serve_scope_for(alpn_policy, admission);
    account_store.set_serve_scope(scope);
    content_store.set_serve_scope(scope);
    // Route the session to the store the negotiated ALPN names (validated above, so the final
    // `else` is the table stream, not an unknown-ALPN fallthrough).
    // The account log and `/3` content are the anonymous-servable paths, so their egress is metered
    // against the shared budget. Table sync is pinned `Closed` (unreachable by an anonymous peer),
    // so it carries no anonymous egress and is left unmetered here.
    let report = if alpn.as_slice() == SYNC_ALPN {
        run_session_limited(
            account_store,
            send,
            recv,
            AuthRole::Acceptor,
            capabilities,
            DEFAULT_IDLE_TIMEOUT,
            egress,
            now_ms,
        )
        .await
        .map_err(SyncFailure::Session)?
    } else if alpn.as_slice() == CONTENT_SYNC_ALPN {
        run_session_limited(
            content_store,
            send,
            recv,
            AuthRole::Acceptor,
            capabilities,
            DEFAULT_IDLE_TIMEOUT,
            egress,
            now_ms,
        )
        .await
        .map_err(SyncFailure::Session)?
    } else {
        let mut table_store = crate::store::OplogTableSyncStore::new(
            account_store.connection(),
            AccountId::from_bytes(account_store.account_id()),
            now_ms,
        );
        let table =
            run_table_session(&mut table_store, send, recv, AuthRole::Acceptor, capabilities)
                .await
                .map_err(SyncFailure::TableSession)?;
        SessionReport {
            entries_sent: table.entries_sent,
            entries_received: table.entries_received,
            entries_newly_stored: table.entries_newly_stored,
        }
    };
    // Keep the acceptor alive until the dialer reads its final acknowledgement and closes.
    let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    conn.close(0u32.into(), b"done");
    Ok((alpn, report))
}

/// One account a multi-account host serves: the account-log + content stores (BOTH for that one
/// account) and its per-account admission [`AuthPolicy`]. A host holds a bounded slice of these; a
/// connection selects one by the account the dialer names.
///
/// Constructed only through [`HostedAccount::new`], which REFUSES a `sync`/`content` pair for
/// different accounts — the same invariant [`dispatch_connection`] enforces at runtime, lifted to
/// construction so a misaligned pair (a content store for account B behind account A's log) is
/// unrepresentable and can never serve one account's content to another's authenticated peer. The
/// caller is responsible for passing DISTINCT accounts in the hosted slice; a duplicate account id
/// is served by its first entry.
pub struct HostedAccount<'a> {
    sync: OplogSyncStore<'a>,
    content: OplogContentSyncStore<'a>,
    policy: AuthPolicy,
}

impl<'a> HostedAccount<'a> {
    /// Bind an account's `sync` + `content` stores and its admission `policy` for hosting. Errors
    /// if the two stores are for different accounts — the cross-account-content isolation
    /// guard.
    pub fn new(
        sync: OplogSyncStore<'a>,
        content: OplogContentSyncStore<'a>,
        policy: AuthPolicy,
    ) -> Result<Self, SyncFailure> {
        if sync.account_id() != content.account_id() {
            return Err(SyncFailure::Endpoint(EndpointError::Connect(
                "account and content stores are for different accounts".into(),
            )));
        }
        Ok(Self { sync, content, policy })
    }
}

/// Accept ONE inbound connection and run its session for whichever of the host's BOUNDED SET of
/// `accounts` the dialer names in its auth frame — one endpoint fronting N accounts (one store pair
/// each). The dialer's named account selects the store pair, then the negotiated ALPN routes
/// exactly as [`dispatch_connection`] does for the single-account case.
///
/// Isolation: the selected account's own stores serve the session, and every store rejects a
/// foreign account's entries at ingest, so a session for one account never reads or writes
/// another's. An account the host does not serve — or a peer that fails the selected account's
/// policy — is refused with the SAME uniform [`AuthError`](crate::AuthError) as a rejected binding,
/// so a peer cannot probe which accounts a host holds. No inventory leaves the host before
/// selection + the binding check succeed.
///
/// Per-account policy is honored (a public `Open` account and a private `Closed` one may share the
/// endpoint); table streams are always served under `Closed` regardless of the account's mode.
/// `ENROLL_ALPN` is refused here: enrollment names its account out-of-band before any auth frame,
/// and a host onboards accounts as the DIALER, not by accepting enrollment across its hosted set.
pub async fn dispatch_connection_multi(
    conn: IrohConnection,
    local_node: [u8; 32],
    accounts: &mut [HostedAccount<'_>],
    now_ms: impl Fn() -> i64 + Copy,
    egress: Option<std::sync::Arc<std::sync::Mutex<GlobalEgressLimiter>>>,
) -> Result<(Vec<u8>, SessionReport), SyncFailure> {
    let remote_node = *conn.remote_id().as_bytes();
    let alpn = conn.alpn().to_vec();
    // The multi host serves the account-log, content, and table streams. ENROLL_ALPN (and any
    // unbound ALPN) is refused BEFORE a stream opens — no route, no handshake.
    if alpn.as_slice() != SYNC_ALPN
        && alpn.as_slice() != CONTENT_SYNC_ALPN
        && alpn.as_slice() != TABLE_SYNC_ALPN
    {
        conn.close(0u32.into(), b"unknown-alpn");
        return Err(SyncFailure::Endpoint(EndpointError::Connect(format!(
            "multi-account host does not serve ALPN {alpn:?}"
        ))));
    }
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| SyncFailure::Endpoint(EndpointError::Connect("peer opened no stream".into())))?
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    // Read the clock only now that a peer has connected (see `accept_and_sync`).
    let auth_now_ms = now_ms();
    let table_alpn = alpn.as_slice() == TABLE_SYNC_ALPN;
    // Authorize FIRST, selecting the account from the dialer's frame — no inventory (not even which
    // account is served) leaves the host until selection + the binding check pass. `account_id` and
    // `policy` in the config are placeholders the selection overrides.
    let (selected_account, capabilities, admission) = run_auth_phase_selected(
        &mut send,
        &mut recv,
        AuthConfig {
            role: AuthRole::Acceptor,
            account_id: [0u8; 32],
            local_node,
            remote_node,
            policy: AuthPolicy::Closed,
            now_ms: auth_now_ms,
            pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
        },
        |peer_account| {
            accounts.iter().find(|account| account.sync.account_id() == *peer_account).map(
                |account| Selected {
                    account_id: *peer_account,
                    auth: &account.sync,
                    // Table streams are private account data — never Open, whatever the account's
                    // mode.
                    policy: if table_alpn { AuthPolicy::Closed } else { account.policy },
                },
            )
        },
    )
    .await
    .map_err(SyncFailure::Auth)?;
    // The selector's shared borrow has ended; take the selected store pair mutably for the session.
    let account = accounts
        .iter_mut()
        .find(|account| account.sync.account_id() == selected_account)
        .expect("run_auth_phase_selected returns only an account the selector accepted");
    // Narrow the serve to public-only for an anonymous reader of a `PublicRead` account (the
    // selected account's policy under this ALPN — tables pinned `Closed`), same as the
    // single-account dispatcher; the store re-checks fully-public before serving.
    let alpn_policy = if table_alpn { AuthPolicy::Closed } else { account.policy };
    let scope = serve_scope_for(alpn_policy, admission);
    account.sync.set_serve_scope(scope);
    account.content.set_serve_scope(scope);
    // As in `dispatch_connection`: the anonymous-servable account + content paths are
    // egress-metered; table sync is pinned `Closed` (no anonymous egress) and left unmetered.
    let report = if alpn.as_slice() == SYNC_ALPN {
        run_session_limited(
            &mut account.sync,
            send,
            recv,
            AuthRole::Acceptor,
            capabilities,
            DEFAULT_IDLE_TIMEOUT,
            egress,
            now_ms,
        )
        .await
        .map_err(SyncFailure::Session)?
    } else if alpn.as_slice() == CONTENT_SYNC_ALPN {
        run_session_limited(
            &mut account.content,
            send,
            recv,
            AuthRole::Acceptor,
            capabilities,
            DEFAULT_IDLE_TIMEOUT,
            egress,
            now_ms,
        )
        .await
        .map_err(SyncFailure::Session)?
    } else {
        let mut table_store = crate::store::OplogTableSyncStore::new(
            account.sync.connection(),
            AccountId::from_bytes(account.sync.account_id()),
            now_ms,
        );
        let table =
            run_table_session(&mut table_store, send, recv, AuthRole::Acceptor, capabilities)
                .await
                .map_err(SyncFailure::TableSession)?;
        SessionReport {
            entries_sent: table.entries_sent,
            entries_received: table.entries_received,
            entries_newly_stored: table.entries_newly_stored,
        }
    };
    let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    conn.close(0u32.into(), b"done");
    Ok((alpn, report))
}

/// Whether `enrollment_database`'s local account is exactly `account_id` — see the ENROLL_ALPN
/// branch of [`accept_and_dispatch`]. A database with no minted account cannot redeem anything,
/// so it does not match either.
fn enrollment_database_matches(
    enrollment_database: &Connection,
    account_id: [u8; 32],
) -> anyhow::Result<bool> {
    Ok(rag_rat_oplog::read_local_account(enrollment_database)?
        == Some(AccountId::from_bytes(account_id)))
}

/// A sync attempt that failed setting up the connection, authorizing the peer, or running the
/// session.
#[derive(Debug)]
pub enum SyncFailure {
    Endpoint(EndpointError),
    /// The dedicated owner-side enrollment exchange failed.
    Enrollment(InviteError),
    /// The node-authorization handshake refused the peer (or we could not authorize to it).
    Auth(crate::auth::AuthError),
    Session(SessionError),
    TableSession(TableSessionError),
}

impl std::fmt::Display for SyncFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncFailure::Endpoint(e) => write!(f, "{e}"),
            SyncFailure::Enrollment(e) => write!(f, "{e}"),
            SyncFailure::Auth(e) => write!(f, "{e}"),
            SyncFailure::Session(e) => write!(f, "{e}"),
            SyncFailure::TableSession(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SyncFailure {}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::auth::{LocalAuth, PeerAuthorization, PeerCapability};

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
        assert_eq!(
            serve_scope_for(AuthPolicy::PublicRead, PeerAdmission::Verified),
            ServeScope::Full
        );
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
        let quiet = SessionReport::default();
        // Nothing moved in either direction — the fixpoint.
        assert_eq!(reconcile_step(&quiet, 1, 8), ReconcileStep::Stop { converged: true });
        // Each direction of movement, on its own, keeps the loop going under the cap: `stored`
        // (local promotion), `received` (peer still has data), and `sent` (our push may have made
        // the acceptor evict — a quiet confirmation round must prove the re-push landed).
        let stored =
            SessionReport { entries_sent: 0, entries_received: 0, entries_newly_stored: 2 };
        let received =
            SessionReport { entries_sent: 0, entries_received: 5, entries_newly_stored: 0 };
        let sent = SessionReport { entries_sent: 3, entries_received: 0, entries_newly_stored: 0 };
        for report in [&stored, &received, &sent] {
            assert_eq!(reconcile_step(report, 1, 8), ReconcileStep::Continue);
        }
        // Still moving at the cap stops UN-converged so a later maintenance pass continues.
        assert_eq!(reconcile_step(&sent, 8, 8), ReconcileStep::Stop { converged: false });
        // A quiet round at the cap is still the converged fixpoint.
        assert_eq!(reconcile_step(&quiet, 8, 8), ReconcileStep::Stop { converged: true });
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
        assert_eq!(
            resolved.peers.len(),
            1,
            "the unparseable id is dropped, the valid one resolves"
        );
        assert_eq!(resolved.peers[0].0, valid, "the resolved entry keeps its node-id label");
        assert_eq!(
            resolved.unresolved_configured, 1,
            "the unparseable id is COUNTED, not silently forgotten — the driver seeds its error \
             tally from this and cannot recover it by subtraction once discovery adds peers"
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
                auth: TestStore::new(
                    account,
                    [],
                    PeerCapability::ReadWrite,
                    PeerCapability::ReadWrite,
                ),
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
                std::collections::hash_map::Entry::Occupied(_) =>
                    crate::session::Ingested::NoChange,
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
        assert!(
            !limiter.allow(1, NOW),
            "a further page at the same instant is refused (no credit)"
        );
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
            "the bucket refilled to the connect time admits the delayed connection: \
             {server_result:?}"
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
        let (capabilities, _admission) =
            run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
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
        let mut destination = TableTestStore::new(
            account,
            vec![shared.clone(), destination_only],
            [(shared.stream_id, dialer_entry.clone())],
        );
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
            SYNC_ALPN,
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
            SYNC_ALPN,
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
            SYNC_ALPN,
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
        for alpn in [SYNC_ALPN, CONTENT_SYNC_ALPN] {
            let database = database();
            let account_id = rag_rat_oplog::local_account(&database, NOW).unwrap();
            let account = account_id.to_bytes();
            let account_entries =
                rag_rat_oplog::account_entries_for_sync(&database, account_id).unwrap().len();
            let server_entry = test_entry(1);
            let stale_local_entry = test_entry(9);
            let mut account_store =
                crate::store::OplogSyncStore::new(&database, account_id, || NOW);
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
                if alpn == SYNC_ALPN { account_entries } else { 1 },
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
        let mut destination_store =
            crate::store::OplogSyncStore::new(&destination, account, || NOW);

        let server = accept_and_sync(&listener, &mut source_store, AuthPolicy::Open, || NOW);
        let client = connect_and_sync(
            &dialer,
            direct_addr(&listener),
            SYNC_ALPN,
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
            SYNC_ALPN,
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
            SYNC_ALPN,
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
            "the B-dialer restored account B — selection served B's store, not a fixed first \
             account",
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
            SYNC_ALPN,
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
            SYNC_ALPN,
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
        let mut destination_store =
            crate::store::OplogSyncStore::new(&destination, account, || NOW);

        let server = accept_and_sync(&listener, &mut destination_store, AuthPolicy::Open, || NOW);
        let client = connect_and_sync(
            &dialer,
            direct_addr(&listener),
            SYNC_ALPN,
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
        let mut content_store =
            crate::store::OplogContentSyncStore::new(&owner_db, account, || NOW);
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
        assert_eq!(alpn, ENROLL_ALPN, "the dispatcher routed the enrollment stream");
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
                .map_err(|e| InviteError::Storage(e.into()))?;
            let (send, _recv) = conn.open_bi().await.map_err(|e| InviteError::Storage(e.into()))?;
            drop(send);
            conn.closed().await;
            Ok::<(), InviteError>(())
        };
        let (server_r, client_r) = tokio::join!(server, client);
        assert!(matches!(server_r, Err(InviteError::Malformed(_))), "server: {server_r:?}");
        client_r.unwrap();
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
}
