//! Deterministic distillation substrate (#703): the model-free half of the distilled-record
//! pipeline. It reads the papertrail mirror (items, comments, provider/text closing edges) and the
//! git-history file changes, decides which threads are eligible, assembles their numbered text-unit
//! input, mines mechanical fix commits / issue↔PR coalescing edges / anchor candidates, and
//! enqueues the eligible threads for the later LLM pass (#704). Everything here is deterministic
//! and testable without a model; the LLM never runs from this module.
//!
//! Crate placement: this lives in `rag-rat-core` (not `rag-rat-papertrail`) because anchor mining
//! reads the symbol index, and `rag-rat-query` — where symbol reads live — depends on
//! `rag-rat-papertrail`; a distill module in papertrail would be a dependency cycle. The persisted
//! enums it writes live in `rag-rat-papertrail` (the lowest common crate the Phase-3 readers
//! share).

mod candidates;
mod extract;
mod prompts;
mod units;
mod validate;

pub use extract::ExtractReport;
pub(crate) use extract::{enqueue_eligible, extract};
