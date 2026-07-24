//! Database layer for the rag-rat workspace: the SQLite schema (baseline + additive migration
//! ladder + repo adoption/registry), the storage status probe, the repo/index meta kv accessors,
//! and the chunk-text content store. Sits directly on rag-rat-base; domain crates depend on this
//! one and supply their derived-data builders through [`hooks::MigrationHooks`] — migrations
//! never link domain code downward.

pub mod chunk_text_store;
pub mod content_digest;
pub mod hooks;
pub mod meta;
pub mod schema;
pub mod storage;
pub mod text_compression;

pub use hooks::MigrationHooks;
