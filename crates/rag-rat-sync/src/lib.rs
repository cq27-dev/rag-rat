//! Peer/p2p transport for the rag-rat op log (phase D, #406).
//!
//! Phase C built the stationary machine — a signed, hash-chained op log with a deterministic,
//! device-independent fold, and ingest seams that verify raw signed bytes from any source. This
//! crate is the wire: an iroh QUIC session that exchanges those bytes between peers and feeds each
//! received entry back through the same ingest seam, so a synced entry passes exactly the checks a
//! local write does. The transport adds movement, never trust.
//!
//! Layers, bottom up:
//! - [`wire`] — the frozen CBOR frame protocol (hello / entries / done).
//! - [`codec`] — length-prefixed framing over any async byte stream (iroh in production, an
//!   in-memory duplex in tests).
//! - [`session`] — the symmetric state machine and the [`session::SyncStore`] seam.
//! - [`store`] — the op-log-backed [`session::SyncStore`].
//! - [`endpoint`] — the iroh endpoint that binds the ALPN over a pinned relay and runs a session
//!   per connection.

pub mod codec;
pub mod endpoint;
pub mod session;
pub mod store;
pub mod wire;

pub use endpoint::{
    EndpointError, SyncFailure, accept_and_sync, build_endpoint, connect_and_sync, endpoint_addr,
};
pub use session::{
    DEFAULT_IDLE_TIMEOUT, Ingested, MAX_SESSION_ENTRIES, SessionError, SessionReport, SyncStore,
    run_session, run_session_with_idle_timeout,
};
pub use store::{OplogContentSyncStore, OplogSyncStore};
pub use wire::{Frame, SYNC_ALPN, WireError};
