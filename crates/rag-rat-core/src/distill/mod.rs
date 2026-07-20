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
mod extract;
// Registered ahead of the separate queue-drain slice, which will consume this contract.
#[allow(dead_code)]
mod output;
mod prompts;
#[allow(dead_code)] // Curated seam for the separate queue-drain slice.
mod run_stats;
mod units;
mod validate;

pub use extract::ExtractReport;
pub(crate) use extract::{enqueue_eligible, extract};
// This is the curated crate-internal seam for the separate queue-drain slice.
#[allow(unused_imports)]
pub(crate) use output::{
    CitationId, DecisionOutput, LadderFailure, LadderResult, LadderStats, OutcomeOutput,
    OutputRung, RecordOutput, RejectedAlternativeOutput, run_output_ladder,
};
