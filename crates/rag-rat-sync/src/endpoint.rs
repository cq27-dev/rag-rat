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
//! under [`AuthPolicy::Open`] any peer is admitted (for a future public read-only knowledge base).
//! The ONBOARDING case — admitting a not-yet-roster device via a one-time invite token
//! ([`AuthPolicy::InviteToken`]) — is not implemented in this slice and fails closed; it is the
//! pairing flow (`sync init` / `sync pair` / `sync join`) that the CLI slice wires up.

use std::str::FromStr;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use tokio::time::timeout;

use crate::auth::{
    AuthConfig, AuthPolicy, AuthRole, DEFAULT_PRE_AUTH_TIMEOUT, NodeAuth, run_auth_phase,
};
use crate::session::{DEFAULT_IDLE_TIMEOUT, SessionError, SessionReport, SyncStore, run_session};
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
        .alpns(vec![SYNC_ALPN.to_vec(), CONTENT_SYNC_ALPN.to_vec()])
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

/// This endpoint's dialable address — hand it (or a ticket wrapping it) to a peer so it can
/// [`connect_and_sync`] back.
pub fn endpoint_addr(endpoint: &Endpoint) -> EndpointAddr {
    endpoint.addr()
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
    run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
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
    let report = run_session(store, send, recv).await.map_err(SyncFailure::Session)?;
    // The DIALER drives the close: our `run_session` returned only after reading the acceptor's
    // finished streams, so closing now can't truncate our own read. The acceptor waits for this
    // close (bounded) before closing its side, so any entries it streamed reach us first. The
    // reverse (entries WE pushed that the acceptor is still reading) can still truncate on this
    // close — bounded and self-healing (next cadence re-sends the diff); see issue #926.
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
    run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
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
    let report = run_session(store, send, recv).await.map_err(SyncFailure::Session)?;
    // As the ACCEPTOR we may have STREAMED entries the dialer is still reading; wait for the dialer
    // to close first so those frames are delivered, then close (see `GRACEFUL_CLOSE_TIMEOUT`).
    let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    conn.close(0u32.into(), b"done");
    Ok(report)
}

/// Accept ONE inbound connection and run the session for the STREAM the peer negotiated: the
/// account log ([`SYNC_ALPN`] → `account_store`) or `/3` content ([`CONTENT_SYNC_ALPN`] →
/// `content_store`). The auth phase is account-level — `account_store` provides the account id and
/// the node binding for both — so only the post-auth session store differs by ALPN. `sync serve`
/// loops this; the returned ALPN lets the caller log which stream ran. Shares the accept + auth
/// shape with [`accept_and_sync`], which stays as the single-stream (account-log) form for callers
/// that never serve content.
pub async fn accept_and_dispatch<A, C>(
    endpoint: &Endpoint,
    account_store: &mut A,
    content_store: &mut C,
    policy: AuthPolicy,
    now_ms: impl Fn() -> i64,
) -> Result<(Vec<u8>, SessionReport), SyncFailure>
where
    A: SyncStore + NodeAuth,
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
    if alpn.as_slice() != SYNC_ALPN && alpn.as_slice() != CONTENT_SYNC_ALPN {
        conn.close(0u32.into(), b"unknown-alpn");
        return Err(SyncFailure::Endpoint(EndpointError::Connect(format!(
            "peer negotiated an unknown ALPN {alpn:?}"
        ))));
    }
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| SyncFailure::Endpoint(EndpointError::Connect("peer opened no stream".into())))?
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    // Read the clock only now that a peer has connected (see `accept_and_sync`).
    let now_ms = now_ms();
    // The auth phase is store-agnostic (the binding is account-level), so authorize with the
    // account store BEFORE any inventory — no stream leaves this peer until it passes the policy.
    run_auth_phase(&mut send, &mut recv, &*account_store, AuthConfig {
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
        run_session(account_store, send, recv).await
    } else {
        run_session(content_store, send, recv).await
    }
    .map_err(SyncFailure::Session)?;
    // As the ACCEPTOR we may have STREAMED entries the dialer is still reading; wait for the dialer
    // to close first so those frames are delivered, then close (see `GRACEFUL_CLOSE_TIMEOUT`).
    let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    conn.close(0u32.into(), b"done");
    Ok((alpn, report))
}

/// A sync attempt that failed setting up the connection, authorizing the peer, or running the
/// session.
#[derive(Debug)]
pub enum SyncFailure {
    Endpoint(EndpointError),
    /// The node-authorization handshake refused the peer (or we could not authorize to it).
    Auth(crate::auth::AuthError),
    Session(SessionError),
}

impl std::fmt::Display for SyncFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncFailure::Endpoint(e) => write!(f, "{e}"),
            SyncFailure::Auth(e) => write!(f, "{e}"),
            SyncFailure::Session(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SyncFailure {}
