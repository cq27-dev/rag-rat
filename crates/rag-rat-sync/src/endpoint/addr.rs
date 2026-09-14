//! Endpoint construction and node-id / address conversion.

use std::str::FromStr;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};

use crate::enrollment::ENROLL_ALPN;
use crate::table_wire::TABLE_SYNC_ALPN;
use crate::wire::{CONTENT_SYNC_ALPN, SYNC_ALPN};

/// Endpoint construction or connection setup failed, before a session could run.
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    /// The configured relay URL did not parse.
    #[error("invalid relay url: {0}")]
    RelayUrl(String),
    /// Binding the endpoint failed (socket, TLS, relay handshake).
    #[error("binding the sync endpoint failed: {0}")]
    Bind(String),
    /// Dialling a peer, or accepting an inbound connection, failed.
    #[error("sync connection setup failed: {0}")]
    Connect(String),
    /// A configured peer node id did not parse.
    #[error("invalid peer node id: {0}")]
    PeerId(String),
}

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

/// Parse a peer's node id from any supported spelling (64-char lowercase hex or standard base32)
/// into its raw 32 bytes — the canonical comparison identity. Callers holding node-id STRINGS
/// must compare through here, never literally: three distinct strings can name one node (see
/// [`discover_peers`](super::peers::discover_peers)), so a string comparison silently treats one
/// peer as several.
pub fn parse_node_id(node_id: &str) -> Result<[u8; 32], EndpointError> {
    let id =
        EndpointId::from_str(node_id.trim()).map_err(|e| EndpointError::PeerId(e.to_string()))?;
    Ok(*id.as_bytes())
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
/// [`crate::InviteTicket`] carries — and the shared relay URL. The byte-oriented sibling of
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

/// The dialable node-id string for a peer's node id BYTES — the inverse of the byte form a ticket
/// or discovery record carries, in the lowercase-hex shape `[sync] server_peers` accepts. Lets the
/// enrollment CLI print a joinable peer id without naming an iroh type.
pub fn node_id_to_string(node_id: &[u8; 32]) -> Result<String, EndpointError> {
    EndpointId::from_bytes(node_id)
        .map(|id| id.to_string())
        .map_err(|e| EndpointError::PeerId(e.to_string()))
}

/// This endpoint's dialable address — hand it (or a ticket wrapping it) to a peer so it can
/// [`connect_and_sync`](super::dial::connect_and_sync) back.
pub fn endpoint_addr(endpoint: &Endpoint) -> EndpointAddr {
    endpoint.addr()
}
