//! Background precompute of the clone-edge graph (#286): the resumable, generation-staged build
//! that writes `clone_edges` so `find_clones` reads a persisted graph instead of recomputing the
//! super-linear SourcererCC candidate pairs every query (it does not finish in 240s on a
//! 118k-function index). The read side (the `find_clones` fast path) is Phase C.
//!
//! Generation-staged: each build writes a fresh `clone_graph_generations` row; reads serve the
//! latest `Complete` generation (the `clone_graph_live_generation` meta key); the pointer flips
//! atomically on completion so a half-built generation is never served. The build streams symbols
//! in `symbol_id` order and emits each clone pair from its SMALLER endpoint exactly once (the
//! SourcererCC structure), so it is chunkable + cursor-resumable under a `max_seconds` budget and
//! dedup is structural. It reuses the EXACT candidate-gen primitives of the parent `clones` module
//! (`sub_block_tokens`, `overlap`, `verified_clone`) so the persisted set equals the live
//! `candidate_pairs_from_bags` set (the Phase-C parity test pins this). Edges are CONTENT-ANCHORED
//! on `(path, start_byte, file_sha)` (the #248 rule — no `symbol_id` FK); reads resolve back to
//! live symbols.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use rag_rat_base::time::now_ms;
use rag_rat_clones::NORM_VERSION;
use rusqlite::{Connection, params};
use serde::Serialize;

use super::THETA;
// The current-df bag load stays test-only here: production build passes read the pinned epoch.
#[cfg(test)]
use super::substrate::load_scoped_baseline_bags;
use super::substrate::{
    SymbolBag, load_clone_df_epoch, load_scoped_baseline_bags_with_df, overlap, sub_block_tokens,
    verified_clone,
};
use crate::index::IndexDatabase;

mod build;
mod read;
mod schedule;
mod storage;

pub(super) use build::make_edge;
pub(super) use read::precomputed_pairs_if_eligible;
pub(super) use storage::{
    clone_df_epoch_exists, clone_df_epoch_serves, clone_generation_scope_clause, insert_edge_rows,
    insert_posting_groups, live_generation_row,
};

/// θ the graph is precomputed at — the default `find_clones` threshold. Queries at θ ≥ this read
/// the stored edges (filtering the exact gate inputs); θ below falls back to the live path.
pub(crate) const CLONE_PRECOMPUTE_THETA: f64 = THETA;

const DEFAULT_BATCH_SIZE: usize = 512;

/// Per-repo meta keys for the #472 quiet-window gate: the stale content revision under
/// observation and when it was first observed (epoch ms).
const CLONE_GRAPH_QUIET_REVISION_META: &str = "clone_graph_quiet_candidate_revision";
const CLONE_GRAPH_QUIET_SINCE_META: &str = "clone_graph_quiet_candidate_since_ms";

/// Soft per-pass budget + checkpoint granularity for one
/// [`IndexDatabase::reconcile_clone_edges_pass`].
#[derive(Debug, Clone)]
pub struct CloneEdgeOptions {
    /// Stop the pass once this many seconds elapse (after ≥1 batch, so progress is always made);
    /// `None` runs the pass to completion.
    pub max_seconds: Option<u64>,
    pub batch_size: usize,
    /// Rebuild even when the live generation already matches the current content.
    pub force: bool,
}

impl Default for CloneEdgeOptions {
    fn default() -> Self {
        Self { max_seconds: None, batch_size: DEFAULT_BATCH_SIZE, force: false }
    }
}

/// Outcome of a precompute pass (or loop). `status`: `Current` (skip — already fresh), `Complete`
/// (the generation finished and is now live), or `Partial` (budget tripped mid-build; resume next).
#[derive(Debug, Clone, Serialize)]
pub struct CloneEdgeReport {
    pub status: String,
    pub generation: i64,
    pub symbols_total: u64,
    pub symbols_processed: u64,
    pub edges_written: u64,
    pub source_revision: String,
    pub elapsed_ms: u64,
}

/// Reindex-stable identity of a symbol endpoint: `(path, start_byte, file_sha)`.
pub(super) type Anchor = (String, i64, String);

pub(super) struct EdgeRow {
    a_path: String,
    a_start_byte: i64,
    a_file_sha: String,
    b_path: String,
    b_start_byte: i64,
    b_file_sha: String,
    overlap: i64,
    a_token_len: i64,
    b_token_len: i64,
    similarity: f64,
    edge_source: &'static str,
}

/// The sub-block postings for ONE walked symbol (#296 phase 2): its content anchor plus its
/// `sub_block_tokens` set. Each `PostingGroup` expands to one `clone_subblock_postings` row per
/// token at flush time. Grouping is deliberate — it clones the `(path, file_sha)` strings ONCE per
/// symbol instead of once per token (a large function has hundreds of sub-block tokens).
pub(super) struct PostingGroup {
    pub(super) anchor: Anchor,
    pub(super) tokens: Vec<i64>,
}

pub(super) struct GenerationRow {
    pub(super) generation: i64,
    pub(super) source_revision: String,
    pub(super) normalizer_version: i64,
    cursor_symbol_id: i64,
    edges_written: u64,
    /// Whether this generation is POSTINGS-AWARE (#296 phase 2): its `clone_subblock_postings` are
    /// written in-band by a postings-aware binary. Set to 1 at Building creation and preserved
    /// through `Complete`. A generation created before the feature has `postings_written = 0`.
    /// Because a postings-aware binary writes every walked symbol's postings BEFORE advancing the
    /// cursor, a *Complete* postings-aware generation is fully populated — so the live-generation
    /// completeness gate (`pending_clone_graph` / the Phase-0 skip) reads this as "postings
    /// complete", and a `Building` one reads it as "resumable without a postings gap" (review R2).
    pub(super) postings_written: bool,
    /// Files absorbed by in-place deltas since this generation's full build (#473) — the df-drift
    /// signal: past [`CLONE_GRAPH_DRIFT_REBUILD_FILES`] the quiet gate schedules a full rebuild to
    /// restore sub-block selectivity (df is frozen at the build's epoch, so long-lived generations
    /// slowly lose candidate-pruning efficiency — never correctness).
    pub(super) delta_files_applied: i64,
}

/// How many delta-absorbed files a live generation tolerates before the background tail owes a
/// FULL rebuild (df epoch refresh + fresh postings). Drift degrades candidate-generation
/// efficiency only — edges stay exact — so this is a performance valve, sized generously: a
/// typical editing session touches far fewer files between natural quiet windows.
pub(super) const CLONE_GRAPH_DRIFT_REBUILD_FILES: i64 = 256;

impl IndexDatabase {
    /// Precompute the clone-edge graph to completion under the caller's write lock (loops resumable
    /// passes until the generation is `Complete` or already `Current`). `max_seconds` bounds a
    /// SINGLE pass (checkpoint granularity); the loop still runs to completion.
    pub fn precompute_clone_graph(
        &self,
        max_seconds: Option<u64>,
    ) -> anyhow::Result<CloneEdgeReport> {
        loop {
            let report = self.reconcile_clone_edges_pass(&CloneEdgeOptions {
                max_seconds,
                ..CloneEdgeOptions::default()
            })?;
            if report.status != "Partial" {
                return Ok(report);
            }
        }
    }

    /// One budgeted clone-graph pass for the watcher / `maintenance` tail (#286): a single
    /// resumable pass bounded by the remaining shared pass budget. A thin wrapper over
    /// [`Self::reconcile_clone_edges_pass`] so those callers needn't name `CloneEdgeOptions`.
    pub fn reconcile_clone_edges_with_budget(
        &self,
        max_seconds: Option<u64>,
    ) -> anyhow::Result<CloneEdgeReport> {
        self.reconcile_clone_edges_pass(&CloneEdgeOptions {
            max_seconds,
            ..CloneEdgeOptions::default()
        })
    }
}

#[cfg(test)]
pub(super) mod tests;
