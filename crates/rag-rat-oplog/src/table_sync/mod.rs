//! The table→log sync engine: replicate derived/metadata table rows across an account's devices as
//! self-describing typed-CBOR row ops on a signed per-scope stream, folded by WHOLE-ROW
//! last-writer-wins.
//!
//! Transport-independent. The wire form ([`row_op`]) and fold/produce pipeline plug into
//! `rag-rat-sync` through the production [`transport`] seams.
//!
//! This `mod.rs` is the curated index; the machinery lives in job-focused siblings. The re-export
//! surface widens as the apply/produce siblings land and force each export.

mod apply;
mod diagnostics;
mod engine;
mod produce;
mod refold;
mod registry;
mod retention;
mod row_op;
mod schema_facts;
mod scope_stream;
mod store;
mod transport;

/// Largest signed table-entry envelope accepted by storage and the `/5` transport.
pub const TABLE_SYNC_ENTRY_MAX_BYTES: usize = 64 * 1024;

/// The most bytes the signed envelope adds around an op's bytes — domain tags, stream id,
/// predecessor hash, a full-width lamport, the device fingerprint, the signature and every CBOR
/// header — pinned by `an_envelope_never_adds_more_than_the_overhead_bound` in `store.rs`. What a
/// restatement can pack into one entry is the transport limit less this.
pub(crate) const TABLE_SYNC_ENTRY_OVERHEAD_MAX: usize = 320;

pub use apply::LocalWriterMemo;
pub use diagnostics::{
    TableSyncDiagnosticQuery, TableSyncRowCause, TableSyncRowDiagnostic, table_sync_row_diagnostics,
};
#[cfg(test)]
pub(crate) use refold::refold_stale_projections_against;
/// The store-open forward-compat seam: replay entries retained but not projected when they
/// arrived.
pub use refold::refold_stale_table_sync_projections;
#[cfg(test)]
pub(crate) use registry::{ColumnSpec, TableSpec, ValueType};
#[cfg(test)]
pub(crate) use row_op::{Cell, RowOp, StatedDelete, TypedValue};
#[cfg(test)]
pub(crate) use scope_stream::{ScopeId, scope_stream_id};
pub(crate) use store::enqueue_readoption_work;
#[cfg(test)]
pub(crate) use store::{
    PendingReason, author_row_entry, mark_entry_pending, record_stream_context,
};
pub use transport::{
    TableSyncChainCursor, TableSyncChainEntry, TableSyncChainHead, TableSyncEntryStart,
    TableSyncFrontier, TableSyncIngestOutcome, TableSyncReceived, TableSyncStream,
    scope_retention_budget, table_sync_author_pending, table_sync_chain_entries,
    table_sync_chain_frontier, table_sync_chain_page_after, table_sync_compact_overdue,
    table_sync_ingest, table_sync_supported_streams, table_sync_validate_stream,
};
