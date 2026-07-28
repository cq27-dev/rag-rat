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

use std::str::FromStr;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use rag_rat_oplog::{self, AccountId, DeviceFingerprint};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tokio::time::timeout;

use crate::auth::{
    AuthConfig, AuthPolicy, AuthRole, DEFAULT_PRE_AUTH_TIMEOUT, NodeAuth, run_auth_phase,
};
use crate::enrollment::{
    ENROLL_ALPN, EnrollmentAcceptorOutcome, EnrollmentReceipt, EnrollmentRequest, InviteError,
    RESPONSE_ACK, RESPONSE_ACK_TIMEOUT, run_enrollment_acceptor, run_enrollment_dialer,
};
use crate::session::{DEFAULT_IDLE_TIMEOUT, SessionError, SessionReport, SyncStore, run_session};
use crate::store::OplogSyncStore;
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
        .alpns(vec![SYNC_ALPN.to_vec(), CONTENT_SYNC_ALPN.to_vec(), ENROLL_ALPN.to_vec()])
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

/// Build a dialable [`EndpointAddr`] from a peer's node id (the z-base-32 form `endpoint.id()`
/// prints) and the shared relay URL. The device-side sync driver configures server peers by node id
/// and reaches each through the pinned relay — the CLI stays iroh-free by going through here.
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

/// Resolve the peers a device-side sync should dial for `account_id` into dialable addresses, each
/// paired with the node-id string that names it (for logging). This is the single seam the sync
/// driver iterates: today it maps each explicitly configured node id through the pinned relay,
/// logging and skipping any that do not parse, so a misconfigured entry surfaces in the logs rather
/// than aborting the whole pass.
///
/// `account_id` is unused by the explicit-config resolver — it is the parameter a relay-backed
/// resolver keys on. When peers of an account are online together they should auto-discover and
/// sync directly instead of every device carrying a static peer list; because the driver only ever
/// iterates this function, that lands as an implementation swap here, not a driver rewrite. A
/// discovered peer fills the same `(node_id, addr)` shape a configured one does.
// TODO(discovery, #988): a relay-side account-keyed resolver plugs in here — query the pinned
// relay for the endpoints advertising `account_id` and compose them with the configured peers.
pub fn discover_peers(
    account_id: AccountId,
    configured_peers: &[String],
    relay_url: &str,
) -> Vec<(String, EndpointAddr)> {
    let _ = account_id; // reserved for the relay-backed resolver (see the TODO above)
    configured_peers
        .iter()
        .filter_map(|peer| match peer_addr(peer, relay_url) {
            Ok(addr) => Some((peer.clone(), addr)),
            Err(error) => {
                tracing::warn!(peer, %error, "skipping a configured sync peer with an invalid node id");
                None
            },
        })
        .collect()
}

/// The dialable node-id string for a peer's node id BYTES — the inverse of the byte form a ticket
/// or discovery record carries, in the z-base-32 shape `[sync] server_peers` accepts. Lets the
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
    let capabilities = run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
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
    let capabilities = run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
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
/// `content_store`), or owner-side enrollment ([`ENROLL_ALPN`] → the account store's database).
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
    // The two stores MUST be for the same account: the auth phase authorizes the peer against the
    // account store's account, and a content connection then runs the content store — which would
    // serve the WRONG account's content if they differed. Our callers always pass same-account
    // stores; enforce it for the public generic API before any connection is accepted.
    if account_store.account_id() != content_store.account_id() {
        return Err(SyncFailure::Endpoint(EndpointError::Connect(
            "account and content stores are for different accounts".into(),
        )));
    }
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
    let alpn = conn.alpn().to_vec();
    // Reject an unroutable ALPN BEFORE opening a stream or running auth — there is no reason to
    // complete a mutual handshake for a stream we can't serve. Unreachable today (iroh's TLS
    // refuses any ALPN `build_endpoint` didn't bind, and it binds exactly the two routed ones),
    // but keeping the check ahead of auth means adding a third bound ALPN without a route here
    // fails cleanly here instead of after the peer has completed authorization.
    if alpn.as_slice() != SYNC_ALPN
        && alpn.as_slice() != CONTENT_SYNC_ALPN
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
    let now_ms = now_ms();
    // The auth phase is store-agnostic (the binding is account-level), so authorize with the
    // account store BEFORE any inventory — no stream leaves this peer until it passes the policy.
    let capabilities = run_auth_phase(&mut send, &mut recv, &*account_store, AuthConfig {
        role: AuthRole::Acceptor,
        account_id: account_store.account_id(),
        local_node,
        remote_node,
        policy,
        now_ms,
        pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
    })
    .await
    .map_err(SyncFailure::Auth)?;
    // Route the session to the store the negotiated ALPN names (validated to be one of the two
    // routed ALPNs above, so the `else` is the content stream, not an unknown-ALPN fallthrough).
    let report = if alpn.as_slice() == SYNC_ALPN {
        run_session(account_store, send, recv, AuthRole::Acceptor, capabilities).await
    } else {
        run_session(content_store, send, recv, AuthRole::Acceptor, capabilities).await
    }
    .map_err(SyncFailure::Session)?;
    // Keep the acceptor alive until the dialer reads its final acknowledgement and closes.
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
}

impl std::fmt::Display for SyncFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncFailure::Endpoint(e) => write!(f, "{e}"),
            SyncFailure::Enrollment(e) => write!(f, "{e}"),
            SyncFailure::Auth(e) => write!(f, "{e}"),
            SyncFailure::Session(e) => write!(f, "{e}"),
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

    fn database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        conn
    }

    #[test]
    fn discover_peers_resolves_valid_ids_and_drops_invalid_ones() {
        let valid = node_id_to_string(&node_id_from_secret([7u8; 32])).unwrap();
        let peers = discover_peers(
            AccountId::from_bytes([1u8; 32]),
            &[valid.clone(), "not-a-node-id".to_string()],
            "https://relay.example",
        );
        assert_eq!(peers.len(), 1, "the unparseable id is dropped, the valid one resolves");
        assert_eq!(peers[0].0, valid, "the resolved entry keeps its node-id label");
    }

    #[test]
    fn node_id_string_round_trips_through_bytes() {
        let bytes = node_id_from_secret([9u8; 32]);
        let text = node_id_to_string(&bytes).unwrap();
        // `peer_addr` parses the same z-base-32 form, so the string is a valid dial id, and
        // `peer_addr_from_bytes` reaches the same address from the raw bytes a ticket carries.
        assert!(peer_addr(&text, "https://relay.example").is_ok());
        assert!(peer_addr_from_bytes(&bytes, "https://relay.example").is_ok());
    }

    struct TestStore {
        account: [u8; 32],
        entries: HashMap<[u8; 32], Vec<u8>>,
        local_capability: PeerCapability,
        peer_authorization: PeerAuthorization,
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
            }
        }
    }

    impl SyncStore for TestStore {
        fn account_id(&self) -> [u8; 32] {
            self.account
        }

        fn snapshot(&self) -> anyhow::Result<Vec<([u8; 32], Vec<u8>)>> {
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

    fn test_entry(seed: u8) -> ([u8; 32], Vec<u8>) {
        ([seed; 32], vec![seed; 40])
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
                .alpns(vec![SYNC_ALPN.to_vec(), CONTENT_SYNC_ALPN.to_vec(), ENROLL_ALPN.to_vec()])
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
