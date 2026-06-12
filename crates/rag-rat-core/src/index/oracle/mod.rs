//! SCIP-oracle subsystem (#61 / #68 phase 1).
//!
//! Consumes a pre-built SCIP index (`rust-analyzer scip`, `scip-typescript`, …) and uses it as a
//! *resolution oracle* for the tree-sitter graph: it confirms / contradicts heuristic edge
//! resolutions and recovers low-confidence / unresolved ones with compiler-grade data. The pass is
//! batch, diff-friendly, and opt-in — the same architectural slot as the embeddings `reconcile`.
//!
//! Phase 1 is **eval-only**: no CLI command, no MCP tool (those are #69). It reads a `.scip`,
//! joins occurrences against edge candidates, writes `edge_oracle` side rows, and emits
//! precision/recall metrics. The heuristic resolution on the `edges` row is **never** overwritten —
//! both coexist so eval can diff them (see [`crate::index::schema::apply_oracle_tables`]).
//!
//! Layout mirrors `index/ai/`:
//! - `scip.rs`  — `.scip` reader: per-document `position_encoding`-aware occurrence + definition
//!   maps.
//! - `join.rs`  — occurrence → edge join (identifier-token containment, not line equality).
//! - `run.rs`   — the pass over edge candidates, producing an [`OracleReport`].
//! - `status.rs`— status type surfaced like `local_ai_status`.
//! - `store.rs` — `oracle_runs` / `edge_oracle` read + write helpers.

mod join;
mod run;
mod scip;
mod status;
mod store;
#[cfg(test)]
mod tests;

use std::path::Path;

use run::OracleRunInput;
pub use run::{OracleEvalMetrics, RecallCalls};
use rusqlite::Connection;
use serde::Serialize;
pub use status::OracleStatus;

/// Run one oracle pass over the current edge candidates from a pre-built `.scip` and return its
/// [`OracleReport`]. Phase-1 public entry point (consumed by `eval`); no CLI/MCP surface yet (#69).
///
/// `checkout_root` is the source root whose bytes are read for per-document position-encoding
/// conversion (the `.scip` document paths are relative to it).
pub fn run_oracle(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    scip_bytes: &[u8],
    checkout_root: &Path,
) -> anyhow::Result<OracleReport> {
    run::run(conn, &OracleRunInput {
        tool,
        tool_version,
        commit_sha,
        worktree_id,
        scip_bytes,
        checkout_root,
    })
}

/// Heuristic-vs-oracle eval metrics for a tool/version, diffing `edge_oracle` against `edges`,
/// scoped to the active `(commit_sha, worktree_id)` checkout (the metric denominators must match
/// the `edge_join_candidates` scope the run wrote through). [`RecallCalls`] carries the run's
/// `(covered_calls, oracle_only_calls)` — BOTH occurrence-counted over the call population — so
/// recall compares like with like; they come from the run's [`OracleReport`].
pub fn oracle_eval_metrics(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    recall_calls: RecallCalls,
) -> anyhow::Result<OracleEvalMetrics> {
    run::eval_metrics(conn, tool, tool_version, commit_sha, worktree_id, recall_calls)
}

/// Persisted oracle status for a tool/version, scoped to the active `(commit_sha, worktree_id)`
/// checkout (the verdict counts must cover only this checkout, like the eval metrics).
pub fn oracle_status(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<OracleStatus> {
    status::status(conn, tool, tool_version, commit_sha, worktree_id)
}

/// The oracle tool that produced a SCIP index. Phase 1 ships only the Rust backend (consumed from a
/// pre-built `.scip`); the enum is the registry seam phase 2 (#69) extends. Persisted enum →
/// `as_db_str` / `from_db_str` per `rust-modern-style`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum OracleTool {
    RustAnalyzer,
}

impl OracleTool {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "rust-analyzer" => Some(Self::RustAnalyzer),
            _ => None,
        }
    }
}

/// What the oracle concluded about one edge candidate. The names mirror the #61 design taxonomy.
/// Persisted on `edge_oracle.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum OracleResolutionKind {
    /// An unresolved / `NameOnly` / `Ambiguous` edge whose callee the oracle resolved to a symbol
    /// inside the corpus — a recovery the heuristic missed or under-resolved.
    Upgrade,
    /// The oracle resolved the callee to a definition **outside** the corpus (an external
    /// dependency); `scip_symbol`'s package component names it. `resolved_symbol_id` is NULL.
    ResolvedExternal,
    /// An already-`Exact` / `Syntactic` edge whose oracle target agrees with the heuristic target.
    /// The precision signal.
    Confirm,
    /// An already-`Exact` / `Syntactic` edge whose oracle target **disagrees** with the heuristic
    /// target. Recorded for eval only — NEVER applied back to the heuristic `edges` row.
    Contradict,
}

impl OracleResolutionKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Upgrade => "upgrade",
            Self::ResolvedExternal => "resolved-external",
            Self::Confirm => "confirm",
            Self::Contradict => "contradict",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "upgrade" => Some(Self::Upgrade),
            "resolved-external" => Some(Self::ResolvedExternal),
            "confirm" => Some(Self::Confirm),
            "contradict" => Some(Self::Contradict),
            _ => None,
        }
    }
}

/// Outcome of an oracle pass, mirroring `ReconcileReport`'s shape (counts + a status string). One
/// per `run()`; persisted opaquely as `oracle_runs.stats_json` and surfaced via [`OracleStatus`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct OracleReport {
    /// Edge candidates examined (those carrying a callee byte range in the scoped source files).
    pub edges_examined: u64,
    /// Unresolved / low-confidence edges upgraded to an in-corpus symbol.
    pub upgraded: u64,
    /// Edges whose callee resolved to an external dependency.
    pub resolved_external: u64,
    /// Already-resolved edges the oracle confirmed.
    pub confirmed: u64,
    /// Already-resolved edges the oracle contradicted (recorded, never applied).
    pub contradicted: u64,
    /// Occurrences found by the oracle for which no matching edge candidate existed — the recall
    /// gap (edges the heuristic never emitted at all). The DENOMINATOR's missed side. Counted over
    /// the *call* population only (callable occurrences in indexed source whose def maps
    /// in-corpus).
    pub oracle_only_calls: u64,
    /// Distinct *call* occurrences a `calls_name` edge DID cover — the covered side of recall,
    /// counted over the SAME call-occurrence population as `oracle_only_calls` (both occurrence-
    /// deduped, both call-only), so `recall = covered_calls / (covered_calls + oracle_only_calls)`
    /// compares like with like. Non-call edge kinds (`references_type` / `uses_macro` / … ) carry
    /// a callee byte range and produce verdicts, but never count here (#81 finding 1).
    pub covered_calls: u64,
    /// Candidates skipped because the disk bytes read for position conversion no longer match the
    /// `file_sha` the edge candidate (and the `.scip`) were built against — the file drifted
    /// between the indexer run and the `.scip` build. A verdict from drifted content joins
    /// occurrence ranges (live disk bytes) against candidate ranges (indexed content) that no
    /// longer correspond, so it is silently wrong; we skip it rather than persist a bogus
    /// verdict (#81 finding 2). A non-zero count means `eval` should warn that some documents
    /// were out of sync.
    pub skipped_drifted: u64,
    /// `local N` occurrences skipped (function-local, no cross-file meaning).
    pub skipped_local: u64,
    /// Candidates whose callee position fell outside any occurrence (no oracle data).
    pub no_occurrence: u64,
    /// `edge_oracle` rows written this pass.
    pub rows_written: u64,
    pub status: String,
}

/// An oracle backend: produces (or consumes) a SCIP index for a checkout. Phase 1 implements only
/// the consume-existing-`.scip` path; the trait is the seam phase 2 (#69) fills with indexer
/// invocation. Kept minimal on purpose.
pub trait Oracle {
    fn tool(&self) -> OracleTool;
    fn tool_version(&self) -> &str;
}
