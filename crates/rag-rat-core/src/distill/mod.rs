//! Distilled-record pipeline substrate. The deterministic extraction half (#703) reads the
//! papertrail mirror and git history, assembles numbered units, mines mechanical fix/anchor data,
//! and enqueues eligible threads. The model boundary (#704) is isolated behind a strict typed
//! output contract and an explicit, observable response ladder; no queue drain runs implicitly.
//!
//! Crate placement: this lives in `rag-rat-core` (not `rag-rat-papertrail`) because anchor mining
//! reads the symbol index, and `rag-rat-query` — where symbol reads live — depends on
//! `rag-rat-papertrail`; a distill module in papertrail would be a dependency cycle. The persisted
//! enums it writes live in `rag-rat-papertrail` (the lowest common crate the Phase-3 readers
//! share).

mod candidates;
mod drain;
mod extract;
// Stable rung tokens are also the durable run-column vocabulary; not every conversion is needed by
// the runtime yet, but the round-trip contract remains tested.
#[allow(dead_code)]
mod output;
mod prompts;
mod run_stats;
mod units;
mod validate;

pub use drain::DistillDrainReport;
pub(crate) use drain::{drain, pending_count};
pub use extract::ExtractReport;
pub(crate) use extract::{enqueue_eligible, extract};
