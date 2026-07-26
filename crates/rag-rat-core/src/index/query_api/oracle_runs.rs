//! Oracle-run query surface on `IndexDatabase`: run the SCIP/compiler oracle (tool-driven or from a
//! SCIP file), snapshot/probe tool availability, and read run status, versions, and eval metrics.

use std::path::Path;

use rag_rat_base::time::now_ms;
use rag_rat_oracle::{
    self, OracleEvalMetrics, OracleReport, OracleStatus, OracleTool, RecallCalls,
};

use super::*;

/// The two indexed-sha snapshots that arm the oracle's content-drift gates (#82/#83): `production`
/// is the scip-vs-disk pin taken at the tool subprocess's exit; `pre_spawn` is the indexed-sha pin
/// taken before the spawn, covering the subprocess interior the post-exit pin can't see. A
/// pre-built `--scip` arms neither ([`OracleShaSnapshots::default`]). Named so the two same-typed
/// snapshots can't be passed in the wrong order (the positional pair was transposable).
#[derive(Debug, Clone, Copy, Default)]
pub struct OracleShaSnapshots<'a> {
    pub production: Option<&'a std::collections::HashMap<String, String>>,
    pub pre_spawn: Option<&'a std::collections::HashMap<String, String>>,
}

impl IndexDatabase {
    /// Run a SCIP-oracle pass from a pre-built `.scip` over the current (active commit/worktree)
    /// edge candidates, writing `edge_oracle` verdicts. The heuristic resolution on the `edges`
    /// row is never touched. Phase 1 (#68): eval-only, no CLI/MCP surface. Requires a `source_root`
    /// (the checkout whose bytes back the SCIP document position-encoding conversion).
    /// `production_sha` is the per-document disk-hash snapshot a tool-driven run captured the
    /// instant its `.scip` was produced (`Some`), arming the scip-vs-disk content gate (#82
    /// TOCTOU); a pre-built `--scip` has no production moment and passes `None`.
    /// The `shas.pre_spawn` snapshot is taken before the tool subprocess was spawned (see
    /// [`Self::oracle_pre_spawn_snapshot`]), arming the pre-spawn gate that covers the subprocess
    /// interior (#83); a pre-built `--scip` has no spawn and passes
    /// [`OracleShaSnapshots::default`].
    pub fn run_oracle(
        &self,
        tool: OracleTool,
        tool_version: &str,
        scip_bytes: &[u8],
        shas: OracleShaSnapshots<'_>,
    ) -> anyhow::Result<OracleReport> {
        self.run_oracle_at(tool, tool_version, scip_bytes, shas, now_ms())
    }

    /// As [`Self::run_oracle`], but records the run's `started_at` as `started_at_ms` — the moment
    /// the caller began the run (its pre-spawn snapshot), not completion time. The tool-driven path
    /// passes this so the auto-run staleness gate isn't wedged by a run that overlapped a watcher
    /// reindex (#145).
    pub fn run_oracle_at(
        &self,
        tool: OracleTool,
        tool_version: &str,
        scip_bytes: &[u8],
        shas: OracleShaSnapshots<'_>,
        started_at_ms: i64,
    ) -> anyhow::Result<OracleReport> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!(
                "index has no source_root metadata; rebuild required for the oracle pass"
            );
        };
        let root = root.to_path_buf();
        rag_rat_oracle::run_oracle_at(
            self.storage.connection(),
            tool,
            tool_version,
            &self.active_commit_sha,
            &self.active_worktree_id,
            scip_bytes,
            &root,
            shas.production,
            shas.pre_spawn,
            started_at_ms,
        )
    }

    /// The active checkout's indexed `(path -> files.sha256)` map — the pre-spawn snapshot the
    /// CLI takes BEFORE spawning the oracle tool (and before acquiring the index write lock; this
    /// is a cheap read-only query), so the join can reject any document the watcher reindexed
    /// across the entire spawn → join window (#83).
    pub fn oracle_pre_spawn_snapshot(
        &self,
    ) -> anyhow::Result<std::collections::HashMap<String, String>> {
        rag_rat_oracle::pre_spawn_snapshot(
            self.storage.connection(),
            &self.active_commit_sha,
            &self.active_worktree_id,
        )
    }

    /// Run the live oracle's per-pass resolution (#534) over `worklist` (repo-relative Rust
    /// paths the maintenance pass just reindexed, plus any backlog): resolve their callees
    /// through the resident LSP `session` and write `ra-lsp` verdicts + a backing run row in one
    /// transaction. The batch pass stays the canonical writer — live rows are a per-pass
    /// freshness patch, and there is NO authoritative clear. Best-effort for LSP-side failures
    /// (a dead server aborts the remaining worklist into `unfinished_paths`, never fails);
    /// `Err` is DB-only.
    pub fn run_live_oracle_pass(
        &self,
        session: &mut rag_rat_oracle::LiveOracleSession,
        worklist: &[String],
        max_requests: u64,
        started_at_ms: i64,
    ) -> anyhow::Result<rag_rat_oracle::LivePassReport> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!(
                "index has no source_root metadata; rebuild required for the live oracle pass"
            );
        };
        let root = root.to_path_buf();
        rag_rat_oracle::live_oracle_pass(
            self.storage.connection(),
            session,
            &rag_rat_oracle::LivePassInput {
                commit_sha: &self.active_commit_sha,
                worktree_id: &self.active_worktree_id,
                checkout_root: &root,
                worklist,
                max_requests,
                started_at_ms,
            },
        )
    }

    /// Heuristic-vs-oracle eval metrics (precision/recall/recovery) for a tool/version, diffing the
    /// persisted `edge_oracle` rows against the `edges` heuristic. [`RecallCalls`] is the
    /// `(covered_calls, oracle_only_calls)` pair reported by the most recent [`run_oracle`] — both
    /// occurrence-counted over the call population, so recall compares like with like.
    pub fn oracle_eval_metrics(
        &self,
        tool: OracleTool,
        tool_version: &str,
        recall_calls: RecallCalls,
    ) -> anyhow::Result<OracleEvalMetrics> {
        rag_rat_oracle::oracle_eval_metrics(
            self.storage.connection(),
            tool,
            tool_version,
            &self.active_commit_sha,
            &self.active_worktree_id,
            recall_calls,
        )
    }

    /// Assemble the typed before/after [`rag_rat_oracle::OracleResolutionReport`] (C2) for a
    /// just-completed run over this checkout: the heuristic "before" resolution counts +
    /// moniker tally are read from the index, the eval metrics are diffed from `edge_oracle`,
    /// and everything is stamped onto the C0 schema with the caller's `profile` + `provenance`.
    /// `run` is the just-produced [`OracleReport`] (its run-only counts can't be reconstructed
    /// from the side tables).
    pub fn resolution_report(
        &self,
        profile: &rag_rat_oracle::CorpusProfile,
        provenance: &rag_rat_oracle::RunProvenance,
        tool: OracleTool,
        run: &OracleReport,
    ) -> anyhow::Result<rag_rat_oracle::OracleResolutionReport> {
        rag_rat_oracle::resolution_report(
            self.storage.connection(),
            profile,
            provenance,
            tool,
            &self.active_commit_sha,
            &self.active_worktree_id,
            run,
        )
    }

    /// Run the oracle for a corpus report PROVISIONALLY and apply its health gate atomically: the
    /// pass + report assembly + gate run in ONE transaction that commits only if healthy, so a
    /// gate-failing run is rolled back whole — leaving the prior healthy verdicts/monikers/run row
    /// intact (Codex on #175). `provenance.tool_version` is the run's content-addressed version.
    /// Returns the report (always, for stdout) + the violations (empty = committed). The `shas` arm
    /// the content-drift gates exactly as [`Self::run_oracle_at`].
    pub fn run_oracle_report(
        &self,
        profile: &rag_rat_oracle::CorpusProfile,
        provenance: &rag_rat_oracle::RunProvenance,
        tool: OracleTool,
        scip_bytes: &[u8],
        shas: OracleShaSnapshots<'_>,
        started_at_ms: i64,
    ) -> anyhow::Result<(
        rag_rat_oracle::OracleResolutionReport,
        Vec<rag_rat_oracle::HealthViolation>,
    )> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!(
                "index has no source_root metadata; rebuild required for the oracle pass"
            );
        };
        let root = root.to_path_buf();
        rag_rat_oracle::run_oracle_report(
            self.storage.connection(),
            profile,
            provenance,
            tool,
            &self.active_commit_sha,
            &self.active_worktree_id,
            scip_bytes,
            &root,
            shas.production,
            shas.pre_spawn,
            started_at_ms,
        )
    }

    /// Persisted oracle status (verdict counts + last run) for a tool/version, scoped to this
    /// database's active `(commit_sha, worktree_id)` checkout.
    pub fn oracle_status(
        &self,
        tool: OracleTool,
        tool_version: &str,
    ) -> anyhow::Result<OracleStatus> {
        rag_rat_oracle::oracle_status(
            self.storage.connection(),
            tool,
            tool_version,
            &self.active_commit_sha,
            &self.active_worktree_id,
        )
    }

    /// `rag-rat oracle run [--tool <id>]` without a pre-built `--scip`: invoke the indexer to
    /// produce a `.scip` (to a caller-owned temp path), then run the phase-1 join over it. A
    /// missing or unrunnable tool returns [`rag_rat_oracle::OracleRunOutcome::Blocked`] with an
    /// install hint (the CLI prints it and exits 0) — never an error. Records an `oracle_runs`
    /// row on success.
    pub fn run_oracle_with_tool(
        &self,
        tool: OracleTool,
        scip_output: &Path,
    ) -> anyhow::Result<rag_rat_oracle::OracleRunOutcome> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!(
                "index has no source_root metadata; rebuild required for the oracle pass"
            );
        };
        let root = root.to_path_buf();
        rag_rat_oracle::run_oracle_with_tool(
            self.storage.connection(),
            tool,
            &root,
            scip_output,
            &self.active_commit_sha,
            &self.active_worktree_id,
        )
    }

    /// Run the oracle pass over a PRE-BUILT `.scip` for a tool, recording an `oracle_runs` row. The
    /// `--scip <path>` consumption path of `oracle run`; deterministic (no subprocess), so it's the
    /// tested end-to-end seam. `tool_version` labels the run (content-addressed staleness key).
    pub fn run_oracle_from_scip(
        &self,
        tool: OracleTool,
        tool_version: &str,
        scip_bytes: &[u8],
    ) -> anyhow::Result<rag_rat_oracle::OracleReport> {
        // A pre-built `--scip` carries no production moment or spawn we control, so neither the
        // scip-vs-disk nor the pre-spawn gate can arm — only the index-vs-disk gate applies.
        self.run_oracle(tool, tool_version, scip_bytes, OracleShaSnapshots::default())
    }

    /// Probe whether an oracle tool is installed, for `oracle status`. A `Blocked` probe is
    /// informational (the tool isn't installed), never an error.
    pub fn probe_oracle_tool(&self, tool: OracleTool) -> rag_rat_oracle::ToolAvailability {
        rag_rat_oracle::probe_oracle_tool(tool)
    }

    /// The `tool_version` of the most recent oracle run for `tool` in this checkout, or `None` when
    /// no run exists. The version `oracle status` reports verdict counts against.
    pub fn latest_oracle_run_version(&self, tool: OracleTool) -> anyhow::Result<Option<String>> {
        rag_rat_oracle::latest_run_tool_version(
            self.storage.connection(),
            tool,
            &self.active_commit_sha,
            &self.active_worktree_id,
        )
    }

    /// The `started_at` (Unix-epoch ms) of the most recent oracle run for `tool` in this checkout,
    /// or `None` when none exists — the staleness clock the background auto-fresh oracle (Phase
    /// 5) compares against the index's `indexed_at_ms` to decide whether verdicts are stale.
    pub fn latest_oracle_run_started_at(&self, tool: OracleTool) -> anyhow::Result<Option<i64>> {
        rag_rat_oracle::latest_run_started_at(
            self.storage.connection(),
            tool,
            &self.active_commit_sha,
            &self.active_worktree_id,
        )
    }
}
