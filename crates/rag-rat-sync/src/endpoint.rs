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

use std::str::FromStr;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey};

use crate::session::{SessionError, SessionReport, SyncStore, run_session};
use crate::wire::SYNC_ALPN;

/// Endpoint construction or connection setup failed, before a session could run.
#[derive(Debug)]
pub enum EndpointError {
    /// The configured relay URL did not parse.
    RelayUrl(String),
    /// Binding the endpoint failed (socket, TLS, relay handshake).
    Bind(String),
    /// Dialling a peer, or accepting an inbound connection, failed.
    Connect(String),
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EndpointError::RelayUrl(m) => write!(f, "invalid relay url: {m}"),
            EndpointError::Bind(m) => write!(f, "binding the sync endpoint failed: {m}"),
            EndpointError::Connect(m) => write!(f, "sync connection setup failed: {m}"),
        }
    }
}

impl std::error::Error for EndpointError {}

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
        .alpns(vec![SYNC_ALPN.to_vec()])
        .relay_mode(RelayMode::custom([relay_url]))
        .secret_key(SecretKey::from_bytes(&secret_key))
        .bind()
        .await
        .map_err(|e| EndpointError::Bind(e.to_string()))
}

/// This endpoint's dialable address — hand it (or a ticket wrapping it) to a peer so it can
/// [`connect_and_sync`] back.
pub fn endpoint_addr(endpoint: &Endpoint) -> EndpointAddr {
    endpoint.addr()
}

/// Dial `peer` and run one sync session against it, returning what moved.
pub async fn connect_and_sync<S: SyncStore>(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    store: &mut S,
) -> Result<SessionReport, SyncFailure> {
    let conn = endpoint
        .connect(peer, SYNC_ALPN)
        .await
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    let report = run_session(store, send, recv).await.map_err(SyncFailure::Session)?;
    // Best-effort graceful close; the session already exchanged an explicit Done both ways.
    conn.close(0u32.into(), b"done");
    Ok(report)
}

/// Accept ONE inbound connection and run a session against it. D4's `sync serve` loops this;
/// keeping it single-shot here keeps the store's `!Send` connection on one task (no spawn).
pub async fn accept_and_sync<S: SyncStore>(
    endpoint: &Endpoint,
    store: &mut S,
) -> Result<SessionReport, SyncFailure> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| SyncFailure::Endpoint(EndpointError::Connect("endpoint closed".into())))?;
    let conn =
        incoming.await.map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    let (send, recv) = conn
        .accept_bi()
        .await
        .map_err(|e| SyncFailure::Endpoint(EndpointError::Connect(e.to_string())))?;
    let report = run_session(store, send, recv).await.map_err(SyncFailure::Session)?;
    conn.close(0u32.into(), b"done");
    Ok(report)
}

/// A sync attempt that failed either setting up the connection or running the session.
#[derive(Debug)]
pub enum SyncFailure {
    Endpoint(EndpointError),
    Session(SessionError),
}

impl std::fmt::Display for SyncFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncFailure::Endpoint(e) => write!(f, "{e}"),
            SyncFailure::Session(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SyncFailure {}
