//! The table→log sync engine: replicate derived/metadata table rows across an account's devices as
//! self-describing typed-CBOR row ops on a signed per-scope stream, folded by WHOLE-ROW
//! last-writer-wins.
//!
//! Transport-independent. The wire form ([`row_op`]) and the fold/produce pipeline are exercised by
//! driving two DB connections through an in-process loopback; the iroh transport is a later
//! milestone that plugs a `/5` sibling stream into `rag-rat-sync` (nothing here depends on it).
//!
//! This `mod.rs` is the curated index; the machinery lives in job-focused siblings. The re-export
//! surface widens as the apply/produce siblings land and force each export.

mod apply;
mod engine;
mod produce;
mod refold;
mod registry;
mod row_op;
mod schema_facts;
mod scope_stream;
mod store;
mod transport;

/// Largest signed table-entry envelope accepted by storage and the `/5` transport.
pub const TABLE_SYNC_ENTRY_MAX_BYTES: usize = 64 * 1024;

#[cfg(test)]
pub(crate) use refold::refold_stale_projections_against;
/// The store-open forward-compat seam: replay entries retained but not projected when they
/// arrived.
pub use refold::refold_stale_table_sync_projections;
#[cfg(test)]
pub(crate) use registry::{ColumnSpec, TableSpec, ValueType};
#[cfg(test)]
pub(crate) use row_op::{Cell, RowOp, TypedValue};
#[cfg(test)]
pub(crate) use scope_stream::scope_stream_id;
#[cfg(test)]
pub(crate) use store::{
    PendingReason, author_row_entry, mark_entry_pending, record_stream_context,
};
pub use transport::{
    TableSyncChainEntry, TableSyncChainHead, TableSyncEntryStart, TableSyncFrontier,
    TableSyncIngestOutcome, TableSyncStream, table_sync_chain_entries, table_sync_chain_frontier,
    table_sync_chain_page_after, table_sync_ingest, table_sync_supported_streams,
    table_sync_validate_stream,
};
