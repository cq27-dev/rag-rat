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

mod auto_run;
mod corpus;
mod join;
mod manifest;
mod report;
mod run;
mod scip;
mod status;
mod store;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;

pub use auto_run::{AutoRunDecision, AutoRunInputs, auto_run_decision};
pub use corpus::{
    HealthViolation, check_corpus_health, corpora_for_tier, corpus_by_id, load_corpora,
};
pub(crate) use join::package_of;
pub use manifest::{ToolAvailability, ToolManifest};
pub use report::{
    CorpusHealth, CorpusProfile, OracleResolutionReport, REPORT_SCHEMA_VERSION, ResolutionBefore,
    ResolutionDelta, RunProvenance,
};
use run::OracleRunInput;
pub use run::{OracleEvalMetrics, RecallCalls};
use rusqlite::Connection;
use serde::Serialize;
pub use status::OracleStatus;
pub(crate) use store::{EdgeOracleComparison, EdgeOracleVerdict};

/// Run one oracle pass over the current edge candidates from a pre-built `.scip` and return its
/// [`OracleReport`]. Phase-1 public entry point (consumed by `eval`); no CLI/MCP surface yet (#69).
///
/// `checkout_root` is the source root whose bytes are read for per-document position-encoding
/// conversion (the `.scip` document paths are relative to it).
///
/// `production_sha` is the per-document disk-hash snapshot captured when a TOOL produced this
/// `.scip` (`None` for a pre-built `--scip`). When present it arms the scip-vs-disk content gate
/// (#82 TOCTOU); see [`run::OracleRunInput::production_sha`].
///
/// `pre_spawn_sha` is the indexed `(path -> files.sha256)` snapshot taken BEFORE the tool
/// subprocess was spawned (`None` for a pre-built `--scip`). It arms the pre-spawn gate (#83),
/// which covers the subprocess INTERIOR the post-exit `production_sha` cannot; see
/// [`run::OracleRunInput::pre_spawn_sha`].
// The args mirror `OracleRunInput`'s fields one-to-one (this is the public thin adapter to it), so
// a params struct would only re-wrap what callers already pass positionally.
/// Run + record stamped at `now_ms()` — for callers without a controlled spawn moment (a pre-built
/// `--scip`, tests). The tool-driven path uses [`run_oracle_at`] with the real start time (#145).
#[allow(clippy::too_many_arguments)]
pub fn run_oracle(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    scip_bytes: &[u8],
    checkout_root: &Path,
    production_sha: Option<&HashMap<String, String>>,
    pre_spawn_sha: Option<&HashMap<String, String>>,
) -> anyhow::Result<OracleReport> {
    run_oracle_at(
        conn,
        tool,
        tool_version,
        commit_sha,
        worktree_id,
        scip_bytes,
        checkout_root,
        production_sha,
        pre_spawn_sha,
        super::now_ms(),
    )
}

/// As [`run_oracle`], but records `oracle_runs.started_at = started_at_ms` (the moment the run
/// began, captured before the tool subprocess) instead of completion time, so the auto-run
/// staleness gate isn't wedged by a run that overlapped a watcher reindex (#145).
#[allow(clippy::too_many_arguments)]
pub fn run_oracle_at(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    scip_bytes: &[u8],
    checkout_root: &Path,
    production_sha: Option<&HashMap<String, String>>,
    pre_spawn_sha: Option<&HashMap<String, String>>,
    started_at_ms: i64,
) -> anyhow::Result<OracleReport> {
    run::run(conn, &OracleRunInput {
        tool,
        tool_version,
        commit_sha,
        worktree_id,
        scip_bytes,
        checkout_root,
        production_sha,
        pre_spawn_sha,
        started_at_ms,
    })
}

/// Run the oracle for a corpus report PROVISIONALLY: execute the pass, assemble the typed report,
/// apply the corpus health gate, and COMMIT only if healthy. When the gate fails, the whole
/// transaction — INCLUDING the authoritative clear of the prior `(tool, tool_version)` verdicts and
/// the tool's monikers — is rolled back, so a rejected run leaves the previous healthy run's
/// verdicts / monikers / `oracle_runs` row completely intact (Codex on #175). This is why the
/// report path must NOT reuse the committing [`run_oracle_at`] + a post-hoc delete: the
/// authoritative clear at the run's start already destroys the prior state, so only
/// never-committing can preserve it.
///
/// `provenance.tool_version` is the run's content-addressed version — the single source for the run
/// row, the metric scope, and the report envelope. Returns the report (always, for stdout) and the
/// health violations (empty = committed; non-empty = rolled back).
#[allow(clippy::too_many_arguments)]
pub fn run_oracle_report(
    conn: &Connection,
    profile: &report::CorpusProfile,
    provenance: &report::RunProvenance,
    tool: OracleTool,
    commit_sha: &str,
    worktree_id: &str,
    scip_bytes: &[u8],
    checkout_root: &Path,
    production_sha: Option<&HashMap<String, String>>,
    pre_spawn_sha: Option<&HashMap<String, String>>,
    started_at_ms: i64,
) -> anyhow::Result<(report::OracleResolutionReport, Vec<HealthViolation>)> {
    let tx = conn.unchecked_transaction()?;
    let run = run::run_in_tx(conn, &OracleRunInput {
        tool,
        tool_version: &provenance.tool_version,
        commit_sha,
        worktree_id,
        scip_bytes,
        checkout_root,
        production_sha,
        pre_spawn_sha,
        started_at_ms,
    })?;
    let report = resolution_report(conn, profile, provenance, tool, commit_sha, worktree_id, &run)?;
    let violations = check_corpus_health(profile, &report);
    if violations.is_empty() {
        tx.commit()?;
    }
    // Unhealthy → `tx` drops uncommitted → the whole run (clear + writes + run row) rolls back.
    Ok((report, violations))
}

/// The indexed `(path -> files.sha256)` map for the active checkout — the pre-spawn snapshot
/// callers take BEFORE spawning the oracle tool (#83). Cheap (reads indexed shas; hashes
/// nothing), and read-only, so the CLI takes it before acquiring the index write lock.
pub fn pre_spawn_snapshot(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<HashMap<String, String>> {
    store::indexed_file_shas_in_scope(conn, commit_sha, worktree_id)
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

/// Assemble the typed before/after [`OracleResolutionReport`] (C2) for a just-completed run: read
/// the heuristic "before" resolution counts + the moniker tally from the index, compute the diffed
/// eval metrics, and map everything onto the C0 schema. `run` is the run's [`OracleReport`] (its
/// run-only counts — `skipped_drifted`, the recall call pair — aren't reconstructable from the side
/// tables). The SCIP `tool_version` is read from `provenance` (the single source for both the
/// report envelope and the metric scope key). Pure data: Markdown rendering is a glue concern, not
/// here.
pub fn resolution_report(
    conn: &Connection,
    profile: &report::CorpusProfile,
    provenance: &report::RunProvenance,
    tool: OracleTool,
    commit_sha: &str,
    worktree_id: &str,
    run: &OracleReport,
) -> anyhow::Result<report::OracleResolutionReport> {
    let (total_edges, resolved_in_corpus, unresolved) =
        store::resolution_before_counts(conn, commit_sha, worktree_id)?;
    let before = report::ResolutionBefore { total_edges, resolved_in_corpus, unresolved };
    let symbols_with_moniker =
        store::count_symbols_with_moniker(conn, tool, &provenance.tool_version)?;
    let recall = RecallCalls { covered: run.covered_calls, oracle_only: run.oracle_only_calls };
    let metrics =
        run::eval_metrics(conn, tool, &provenance.tool_version, commit_sha, worktree_id, recall)?;
    Ok(report::OracleResolutionReport::assemble(
        profile,
        provenance,
        run,
        &metrics,
        before,
        symbols_with_moniker,
    ))
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

/// The outcome of `oracle run`: either a completed pass with its report, or a `Blocked` probe
/// because the tool isn't installed. `Blocked` is a successful, no-op result — the CLI prints the
/// hint and exits 0 (the missing-embedding-model UX), never an error.
#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum OracleRunOutcome {
    /// The pass ran (consuming a pre-built `.scip` or one the tool produced) and persisted
    /// verdicts.
    Completed { tool: String, tool_version: String, report: OracleReport },
    /// The requested tool isn't runnable. Carries the install hint; no verdicts were written.
    Blocked { tool: String, program: String, hint: String },
}

/// The result of the LOCK-FREE half of a tool-driven oracle run: either the tool produced a `.scip`
/// (carrying its probed version + the serialized bytes), or it was [`OracleRunOutcome::Blocked`].
/// Splitting this from the join lets the CLI run the slow `rust-analyzer scip` subprocess WITHOUT
/// the index write lock held — only the subsequent probe-recheck + join/write need to serialize
/// against the watcher/indexer (#82 P3). The `production_sha` snapshot carried on `Produced` lets
/// the join narrow the resulting lock-free window: a document the watcher reindexes between this
/// production and the join is skipped (scip-vs-disk gate, #82 TOCTOU), never mis-joined. The
/// snapshot is best-effort (taken at subprocess exit, not at rust-analyzer's own reads), so a
/// mid-subprocess edit remains uncovered — see the `production_sha` field on `Produced` below.
pub enum ScipProduction {
    /// The tool ran and wrote a readable `.scip`; `bytes` is its content, `version` the probed
    /// tool version (content-addressed staleness key). `production_sha` is `relative_path -> hex
    /// sha256` of each document's disk bytes read the instant the subprocess finished — the
    /// scip-vs-disk content pin the join uses to reject a document the watcher reindexed in the
    /// lock-free window AFTER production (#82 TOCTOU). A file unreadable at snapshot time is
    /// simply absent (its candidate then fails the `Some(_) == disk` gate and is skipped, the
    /// safe direction). The window INSIDE the subprocess — an edit between the tool reading a
    /// source and this post-exit snapshot — is covered by the separate pre-spawn indexed-sha
    /// snapshot the caller takes before spawning (#83); the two gates compose to span the whole
    /// spawn → join window.
    Produced { version: String, bytes: Vec<u8>, production_sha: HashMap<String, String> },
    /// The tool isn't runnable / can't produce SCIP — the `oracle run` Blocked UX.
    Blocked { tool: String, program: String, hint: String },
}

/// Probe the tool and, if runnable, invoke `<tool> scip` to produce a `.scip` at `scip_output`,
/// returning its bytes — the LOCK-FREE half of a tool-driven run. Takes NO DB connection, so the
/// caller can run this before acquiring the index write lock (the rust-analyzer subprocess is the
/// slow part and must not starve the watcher). A missing/unrunnable tool yields
/// [`ScipProduction::Blocked`] — never an error.
pub fn produce_scip_with_tool(
    tool: OracleTool,
    checkout_root: &Path,
    scip_output: &Path,
) -> anyhow::Result<ScipProduction> {
    let manifest = ToolManifest::for_tool(tool);
    let version = match manifest.probe() {
        ToolAvailability::Available { version, .. } => version,
        ToolAvailability::Blocked { tool, program, hint } => {
            return Ok(ScipProduction::Blocked { tool, program, hint });
        },
    };
    // Tool-specific prerequisite (scip-clang needs a compile_commands.json at the root). Missing →
    // Blocked + hint, exit 0 — never a subprocess error (#71).
    if let Some(hint) = manifest.prerequisite_blocked(checkout_root) {
        return Ok(ScipProduction::Blocked {
            tool: tool.as_db_str().to_string(),
            program: manifest.program.to_string(),
            hint,
        });
    }
    let mut command = manifest.scip_command(checkout_root, scip_output);
    // Forward the tool's stdout to OUR stderr, never inherit it onto our stdout: the CLI emits the
    // run's JSON report on stdout, and a tool that prints progress to stdout (scip-clang's
    // `[N/M] Indexed …`) would otherwise corrupt that JSON. A piped child + a copy thread keeps
    // the progress live on stderr (where rust-analyzer's already goes) while stdout stays clean.
    command.stdout(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to invoke {}: {err}", manifest.program))?;
    let piped = child.stdout.take();
    let forwarder = std::thread::spawn(move || {
        if let Some(mut out) = piped {
            let mut err = std::io::stderr();
            let _ = std::io::copy(&mut out, &mut err);
        }
    });
    let status = child
        .wait()
        .map_err(|err| anyhow::anyhow!("failed waiting for {}: {err}", manifest.program))?;
    let _ = forwarder.join();
    if !status.success() {
        anyhow::bail!("{} scip exited with status {status}", manifest.program);
    }
    let bytes = std::fs::read(scip_output).map_err(|err| {
        anyhow::anyhow!(
            "{} produced no readable index at {}: {err}",
            manifest.program,
            scip_output.display()
        )
    })?;
    // Snapshot each document's disk hash NOW, the instant the subprocess finished — this is the
    // content the `.scip` describes. The join later pins its occurrence offsets to these hashes so
    // a file the watcher reindexes in the lock-free window before the join is skipped instead
    // of mis-joined (#82 TOCTOU). Reading the doc list from the `.scip` (not the whole tree)
    // keeps the snapshot to exactly the files the oracle can speak to. An unreadable file is
    // omitted (its candidate then fails the gate and is skipped — the safe direction).
    let production_sha = snapshot_document_disk_hashes(&bytes, checkout_root);
    Ok(ScipProduction::Produced { version, bytes, production_sha })
}

/// Hash each `.scip` document's CURRENT disk bytes under `checkout_root`, returning `relative_path
/// -> hex sha256`. Backs the scip-vs-disk content gate (#82 TOCTOU): the caller captures this right
/// after the tool exits so the join can verify a document hasn't drifted since production. Hashes
/// in the same space as `files.sha256` / the join's `disk_sha` (via [`run::hex_sha256`]) so the
/// three compare directly. Unreadable documents (and an unparseable `.scip`, which can't have
/// produced usable verdicts anyway) are simply absent from the map.
fn snapshot_document_disk_hashes(
    scip_bytes: &[u8],
    checkout_root: &Path,
) -> HashMap<String, String> {
    let Ok(paths) = scip::ScipIndex::document_relative_paths(scip_bytes) else {
        return HashMap::new();
    };
    let mut out = HashMap::with_capacity(paths.len());
    for path in paths {
        if let Ok(bytes) = std::fs::read(checkout_root.join(&path)) {
            out.insert(path, run::hex_sha256(&bytes));
        }
    }
    out
}

/// Invoke an oracle tool to produce a `.scip` for `checkout_root`, then run the phase-1 join over
/// it — OR, when the tool is absent, return [`OracleRunOutcome::Blocked`] with the install hint
/// (never an error exit). The produced index is written to `scip_output` (a caller-owned temp path)
/// and consumed in place. `tool_version` is the probed version string (content-addressed staleness
/// key), so a tool upgrade invalidates prior verdicts.
///
/// This couples production + join under one connection (used by the public DB API + tests). The CLI
/// `oracle run` path deliberately does NOT use this: it calls [`produce_scip_with_tool`] BEFORE the
/// write lock, then [`run_oracle`] under the lock, so the subprocess doesn't hold the lock (#82
/// P3).
pub fn run_oracle_with_tool(
    conn: &Connection,
    tool: OracleTool,
    checkout_root: &Path,
    scip_output: &Path,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<OracleRunOutcome> {
    // Snapshot the indexed shas BEFORE spawning the tool — the pre-spawn gate (#83) needs the state
    // from before the subprocess could observe any source.
    let pre_spawn_sha = pre_spawn_snapshot(conn, commit_sha, worktree_id)?;
    // Stamp the run's start right after that snapshot (the indexed state it covers) and before the
    // subprocess: later than the indexed_at it's based on, yet before any mid-run reindex, so the
    // auto-run staleness gate is neither falsely rerun nor wrongly skipped. (#145 + #146 review)
    let started_at_ms = super::now_ms();
    match produce_scip_with_tool(tool, checkout_root, scip_output)? {
        ScipProduction::Blocked { tool, program, hint } =>
            Ok(OracleRunOutcome::Blocked { tool, program, hint }),
        ScipProduction::Produced { version, bytes, production_sha } => {
            let report = run_oracle_at(
                conn,
                tool,
                &version,
                commit_sha,
                worktree_id,
                &bytes,
                checkout_root,
                Some(&production_sha),
                Some(&pre_spawn_sha),
                started_at_ms,
            )?;
            Ok(OracleRunOutcome::Completed {
                tool: tool.as_db_str().to_string(),
                tool_version: version,
                report,
            })
        },
    }
}

/// Probe a tool's availability without running it — backs `oracle status`'s "is the tool
/// installed?" line. A `Blocked` probe is informational, not an error.
pub fn probe_oracle_tool(tool: OracleTool) -> ToolAvailability {
    ToolManifest::for_tool(tool).probe()
}

/// A short content fingerprint (first 12 hex chars of the SHA-256) of a pre-built `.scip`'s bytes.
/// The `--scip` run-id keys on `scip-file:{basename}@{fingerprint}` so two DIFFERENT indexes that
/// happen to share a basename (`index.scip` from two trees) don't collide onto one
/// content-addressed `tool_version` — which would let a stale run's verdicts masquerade as the new
/// fixture's (#82 P3). 12 hex chars (48 bits) is ample for a human-run fixture namespace.
pub fn scip_content_fingerprint(scip_bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(scip_bytes);
    let mut out = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Fetch the CURRENT, in-scope oracle verdicts for a set of edge ids — the read-side join that
/// surfaces the `Compiler` tier in graph/impact output. Routes through the scoped + current store
/// helper (`edge_oracle.file_sha == files.sha256`), so a drifted file's edge never surfaces
/// `Compiler` (it reverts to heuristic display). `edge_ids` come from the heuristic traversal, so
/// the result is a subset.
pub(crate) fn current_oracle_verdicts_for_edges(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    edge_ids: &[i64],
) -> anyhow::Result<std::collections::HashMap<i64, EdgeOracleVerdict>> {
    store::current_oracle_verdicts_for_edges(
        conn,
        tool,
        tool_version,
        commit_sha,
        worktree_id,
        edge_ids,
    )
}

/// Fetch ALL current, in-scope oracle verdicts for `(tool, tool_version)` in this checkout, keyed
/// by `edge_id` → `(kind, resolved_symbol_id)`. The whole-graph sibling of
/// [`current_oracle_verdicts_for_edges`], for symbol-importance ranking which walks every edge (one
/// scan, not a query per edge). Same scope + currency gate via the store helper.
pub(crate) fn current_oracle_verdicts_all(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<std::collections::HashMap<i64, (OracleResolutionKind, Option<i64>)>> {
    store::current_oracle_verdicts_all(conn, tool, tool_version, commit_sha, worktree_id)
}

/// Whether ANY oracle run exists in the active checkout (across all tools) — the cheap existence
/// probe that short-circuits the per-tool [`latest_run_tool_version`] calls when nothing ever ran.
pub(crate) fn any_run_in_scope(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<bool> {
    store::any_run_in_scope(conn, commit_sha, worktree_id)
}

/// Prune `oracle_runs` rows for dead `(commit_sha, worktree_id)` contexts — the gc companion to the
/// `edge_oracle` FK cascade. See [`store::prune_oracle_runs_outside_scope`]. Returns rows deleted.
pub fn prune_oracle_runs_outside_scope(
    conn: &Connection,
    live_commits: &[String],
    live_worktrees: &[String],
) -> anyhow::Result<u64> {
    store::prune_oracle_runs_outside_scope(conn, live_commits, live_worktrees)
}

/// The `tool_version` the surfacing reads (the `Compiler` tier) should key on for `tool` in the
/// active checkout: the most recent run's version, or `None` when no run exists. Surfacing query
/// output keys on the last run's verdicts.
pub fn latest_run_tool_version(
    conn: &Connection,
    tool: OracleTool,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Option<String>> {
    store::latest_run_tool_version(conn, tool, commit_sha, worktree_id)
}

/// Every oracle tool that has at least one run in this checkout, paired with its latest
/// `tool_version`. The multi-language surfacing seam (#176): the graph read paths
/// (`enrich_hops_with_oracle`, `compare_graph_to_scip`) iterate THIS rather than hardcoding
/// `RustAnalyzer`, so a repo indexed in several languages surfaces each backend's verdicts on its
/// own edges. An edge belongs to one language, so at most one tool ever has a verdict for it — the
/// per-tool verdict sets are disjoint and merge cleanly. Tools with no run in scope are skipped.
pub fn latest_runs_in_scope(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Vec<(OracleTool, String)>> {
    let mut runs = Vec::new();
    for &tool in OracleTool::ALL {
        if let Some(version) = store::latest_run_tool_version(conn, tool, commit_sha, worktree_id)?
        {
            runs.push((tool, version));
        }
    }
    Ok(runs)
}

/// The `started_at` (Unix-epoch ms) of the most recent run for `tool` in the active checkout, or
/// `None` when no run exists — the staleness clock the background auto-fresh oracle compares
/// against the index's `indexed_at_ms`. See [`auto_run_decision`].
pub fn latest_run_started_at(
    conn: &Connection,
    tool: OracleTool,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Option<i64>> {
    store::latest_run_started_at(conn, tool, commit_sha, worktree_id)
}

/// Load the CURRENT, in-scope `edge_oracle` verdicts joined to their heuristic edge resolution —
/// the data `compare_graph_to_scip` diffs (it keeps `Contradict` rows). Scoped + current via the
/// store helper, so drifted/dirty rows never appear as disagreements.
pub(crate) fn current_oracle_comparisons(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Vec<EdgeOracleComparison>> {
    store::current_oracle_comparisons(conn, tool, tool_version, commit_sha, worktree_id)
}

/// The oracle tool that produced a SCIP index. Phase 1 ships only the Rust backend (consumed from a
/// pre-built `.scip`); the enum is the registry seam phase 2 (#69) extends. Persisted enum →
/// `as_db_str` / `from_db_str` per `rust-modern-style`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum OracleTool {
    /// `rust-analyzer scip` — Rust (phase 2, #69).
    RustAnalyzer,
    /// `scip-clang` — C / C++ (phase 4, #71). Consumes a `compile_commands.json` compilation
    /// database rather than a source root, and is the SCIP emitter directly (no `scip`
    /// subcommand), so its probe + invocation differ from rust-analyzer's — see `ToolManifest`.
    ScipClang,
    /// `scip-python index` — Python (#164 B6). Resolves imports against the project's INSTALLED
    /// dependencies, so the corpus must install them (a venv) first; an unresolved environment
    /// shows up as a near-zero moniker count the report's health gate catches.
    ScipPython,
    /// `scip-typescript index` — TypeScript / TSX (#71). Like scip-python, resolves cross-package
    /// references against the project's installed `node_modules`, so the corpus must `npm install`
    /// first; an unresolved environment shows up as a low moniker count the health gate catches.
    ScipTypescript,
}

impl OracleTool {
    /// Every known oracle tool, for "report on all tools" surfaces (`oracle status` with no
    /// `--tool`). Later language backends (#72 Kotlin) extend this alongside the enum.
    pub const ALL: &[OracleTool] =
        &[Self::RustAnalyzer, Self::ScipClang, Self::ScipPython, Self::ScipTypescript];

    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::ScipClang => "scip-clang",
            Self::ScipPython => "scip-python",
            Self::ScipTypescript => "scip-typescript",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "rust-analyzer" => Some(Self::RustAnalyzer),
            "scip-clang" => Some(Self::ScipClang),
            "scip-python" => Some(Self::ScipPython),
            "scip-typescript" => Some(Self::ScipTypescript),
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
    /// `logical_symbol_monikers` rows written this pass — distinct logical symbols whose moniker
    /// an in-corpus SCIP definition supplied (#70). Several defs can map to one logical symbol
    /// (fields/variants map up to the enclosing symbol); only the deterministic best (shortest)
    /// moniker is written, so this counts rows, not defs.
    pub monikers_written: u64,
    pub status: String,
}

/// An oracle backend: produces (or consumes) a SCIP index for a checkout. Phase 1 implements only
/// the consume-existing-`.scip` path; the trait is the seam phase 2 (#69) fills with indexer
/// invocation. Kept minimal on purpose.
pub trait Oracle {
    fn tool(&self) -> OracleTool;
    fn tool_version(&self) -> &str;
}
