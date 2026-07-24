//! The table→log sync engine: replicate derived/metadata table rows across an account's devices as
//! self-describing typed-CBOR row ops on a signed per-scope stream, folded by per-column
//! last-writer-wins.
//!
//! Transport-independent. The wire form ([`row_op`]) and the fold/produce pipeline are exercised by
//! driving two DB connections through an in-process loopback; the iroh transport is a later
//! milestone that plugs a `/4` sibling stream into `rag-rat-sync` (nothing here depends on it).
//!
//! This `mod.rs` is the curated index; the machinery lives in job-focused siblings. The re-export
//! surface widens as the apply/produce siblings land and force each export.

mod apply;
mod engine;
mod produce;
mod registry;
mod row_op;
mod scope_stream;
mod store;
