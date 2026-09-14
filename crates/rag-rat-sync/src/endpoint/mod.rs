//! The iroh endpoint adapter (phase D, #406).
//!
//! Binds a QUIC endpoint that speaks [`SYNC_ALPN`](crate::SYNC_ALPN) over a pinned relay, and runs
//! one [`run_session`](crate::run_session) per connection. iroh's stream types implement
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
//! [`accept_and_sync`] run the mutual node-authorization handshake
//! ([`run_auth_phase`](crate::run_auth_phase)) BEFORE any inventory is exchanged. Under
//! [`AuthPolicy::Closed`](crate::AuthPolicy::Closed) a peer is admitted only if it presents a
//! signed binding proving its authenticated node id belongs to a roster device of the account;
//! under [`AuthPolicy::Open`](crate::AuthPolicy::Open) an acceptor admits any dialer to read but
//! rejects its entry frames without a write-capable roster role. A fresh dialer may accept entries
//! from the transport-pinned server it explicitly selected so it can restore the roster; storage
//! still verifies every entry. The ONBOARDING case uses the separate
//! [`ENROLL_ALPN`](crate::ENROLL_ALPN) exchange: an owner atomically redeems a one-time invite into
//! a roster `DeviceAdd` before normal sync authentication can admit the new device.

mod accept;
mod addr;
mod dial;
mod dispatch;
mod enroll;
mod peers;
#[cfg(test)]
mod tests;

pub use accept::{
    GlobalAcceptRateLimiter, GlobalEgressLimiter, accept_and_sync, accept_connection,
    accept_connection_within_rate,
};
pub use addr::{
    EndpointError, build_endpoint, endpoint_addr, node_id_from_secret, node_id_to_string,
    parse_node_id, peer_addr, peer_addr_from_bytes,
};
pub use dial::{
    MAX_RECONCILE_ROUNDS, ReconcileReport, connect_and_reconcile, connect_and_sync,
    connect_and_table_reconcile, connect_and_table_sync,
};
pub use dispatch::{
    HostedAccount, SyncAlpn, SyncFailure, accept_and_dispatch, dispatch_connection,
    dispatch_connection_multi,
};
pub use enroll::{accept_enrollment, connect_and_enroll, connect_and_redeem_writer};
pub use peers::{DiscoveredPeers, discover_peers};
