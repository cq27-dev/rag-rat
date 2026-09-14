//! Peer/p2p transport for the rag-rat op log (phase D, #406).
//!
//! Phase C built the stationary machine — a signed, hash-chained op log with a deterministic,
//! device-independent fold, and ingest seams that verify raw signed bytes from any source. This
//! crate is the wire: an iroh QUIC session that exchanges those bytes between peers and feeds each
//! received entry back through the same ingest seam, so a synced entry passes exactly the checks a
//! local write does. The transport adds movement, never trust.
//!
//! Layers, bottom up. The account-log and content lanes share one frame protocol; the table lane
//! has its own, module for module:
//! - [`wire`] / [`table_wire`] — the frozen CBOR frame protocols: hello / entries / done / ack for
//!   the account-log and content lanes, manifest / chain inventory / entries / done / ack for the
//!   table lane.
//! - [`codec`] / [`table_codec`] — length-prefixed framing over any async byte stream (iroh in
//!   production, an in-memory duplex in tests).
//! - [`auth`] — the mutual node-authorization handshake every lane runs before any inventory.
//! - [`session`] / [`table_session`] — the lane state machines and their store seams,
//!   [`session::SyncStore`] and [`table_session::TableSyncStore`]: two concurrent symmetric halves
//!   for the account-log and content lanes, a strictly role-ordered exchange for the table lane.
//! - [`store`] — the op-log-backed implementations of both store seams.
//! - [`enrollment`] — the one-time invite exchange (device pairing and writer grants) on its own
//!   ALPN.
//! - [`discovery`] — account-keyed peer discovery over a shared announcement service.
//! - [`endpoint`] — the iroh endpoint that binds every ALPN over a pinned relay and dispatches each
//!   connection to its lane.

pub mod auth;
pub mod codec;
pub mod discovery;
pub mod endpoint;
pub mod enrollment;
pub mod session;
pub mod store;
pub mod table_codec;
pub mod table_session;
pub mod table_wire;
#[cfg(test)]
mod testing;
pub mod wire;

pub use auth::{
    AuthConfig, AuthError, AuthPolicy, AuthRole, LocalAuth, NodeAuth, PeerAdmission,
    PeerAuthorization, PeerCapability, Selected, SessionCapabilities, run_auth_phase,
    run_auth_phase_selected,
};
pub use endpoint::{
    DiscoveredPeers, EndpointError, GlobalAcceptRateLimiter, GlobalEgressLimiter, HostedAccount,
    MAX_RECONCILE_ROUNDS, ReconcileReport, SyncAlpn, SyncFailure, accept_and_dispatch,
    accept_and_sync, accept_connection, accept_connection_within_rate, accept_enrollment,
    build_endpoint, connect_and_enroll, connect_and_reconcile, connect_and_redeem_writer,
    connect_and_sync, connect_and_table_reconcile, connect_and_table_sync, discover_peers,
    dispatch_connection, dispatch_connection_multi, endpoint_addr, node_id_from_secret,
    node_id_to_string, parse_node_id, peer_addr, peer_addr_from_bytes,
};
pub use enrollment::{
    ENROLL_ALPN, EnrollmentReceipt, EnrollmentRequest, InviteError, InviteSpec, InviteTicket,
    InviteTicketKind, WriterGrantReceipt, WriterInviteSpec, mint_invite, mint_writer_invite,
    redeem_invite, run_enrollment_acceptor, run_enrollment_dialer, run_writer_grant_dialer,
};
/// The dialable address type every peer-facing helper here hands back.
///
/// Re-exported so a caller can NAME what `peer_addr` returns and what `DiscoveryExchange`
/// wants without taking an iroh dependency of its own — the CLI has none, and keeping it that
/// way is what makes this crate the single place the transport is chosen.
pub use iroh::EndpointAddr;
pub use session::{
    DEFAULT_IDLE_TIMEOUT, Ingested, MAX_SESSION_ENTRIES, ServeScope, SessionError, SessionLimits,
    SessionReport, SyncStore, run_session, run_session_limited,
};
pub use store::{OplogContentSyncStore, OplogSyncStore, OplogTableSyncStore};
pub use table_session::{
    ChainEntry, ChainStart, TableSessionError, TableSessionReport, TableSyncStore,
    run_table_session,
};
pub use table_wire::{
    ChainFrontier, ChainHead, FrontierState, Manifest, ManifestItem, TABLE_SYNC_ALPN, TableFrame,
    TableWireError,
};
pub use wire::{CONTENT_SYNC_ALPN, Frame, SYNC_ALPN, WireError};
