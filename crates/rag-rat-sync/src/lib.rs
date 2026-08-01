//! Peer/p2p transport for the rag-rat op log (phase D, #406).
//!
//! Phase C built the stationary machine — a signed, hash-chained op log with a deterministic,
//! device-independent fold, and ingest seams that verify raw signed bytes from any source. This
//! crate is the wire: an iroh QUIC session that exchanges those bytes between peers and feeds each
//! received entry back through the same ingest seam, so a synced entry passes exactly the checks a
//! local write does. The transport adds movement, never trust.
//!
//! Layers, bottom up:
//! - [`wire`] — the frozen CBOR frame protocol (hello / entries / done / ack).
//! - [`codec`] — length-prefixed framing over any async byte stream (iroh in production, an
//!   in-memory duplex in tests).
//! - [`session`] — the symmetric state machine and the [`session::SyncStore`] seam.
//! - [`store`] — the op-log-backed [`session::SyncStore`].
//! - [`endpoint`] — the iroh endpoint that binds the ALPN over a pinned relay and runs a session
//!   per connection.

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
pub mod wire;

pub use auth::{
    AuthConfig, AuthError, AuthPolicy, AuthRole, LocalAuth, NodeAuth, PeerAuthorization,
    PeerCapability, SessionCapabilities, run_auth_phase,
};
pub use endpoint::{
    DiscoveredPeers, EndpointError, MAX_RECONCILE_ROUNDS, ReconcileReport, SyncFailure,
    accept_and_dispatch, accept_and_sync, accept_enrollment, build_endpoint, connect_and_enroll,
    connect_and_reconcile, connect_and_sync, connect_and_table_reconcile, connect_and_table_sync,
    discover_peers, endpoint_addr, node_id_from_secret, node_id_to_string, peer_addr,
    peer_addr_from_bytes,
};
pub use enrollment::{
    ENROLL_ALPN, EnrollmentReceipt, EnrollmentRequest, EnrollmentTicket, InviteError, InviteSpec,
    mint_invite, redeem_invite, run_enrollment_acceptor, run_enrollment_dialer,
};
/// The dialable address type every peer-facing helper here hands back.
///
/// Re-exported so a caller can NAME what `peer_addr` returns and what `DiscoveryExchange`
/// wants without taking an iroh dependency of its own — the CLI has none, and keeping it that
/// way is what makes this crate the single place the transport is chosen.
pub use iroh::EndpointAddr;
pub use session::{
    DEFAULT_IDLE_TIMEOUT, Ingested, MAX_SESSION_ENTRIES, SessionError, SessionReport, SyncStore,
    run_session, run_session_with_idle_timeout,
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
