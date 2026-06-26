//! Oracle status — the serializable summary surfaced like `llm_status` (phase 2 wires it into
//! the MCP `llm_status`-style view; phase 1 just exposes the type + builder).

use rusqlite::Connection;
use serde::Serialize;

use super::OracleTool;
use super::run::{self, VerdictCounts};

/// Snapshot of the persisted `edge_oracle` verdicts for one tool/version, plus the last run's
/// status. Cheap to compute (count queries only); no join re-run.
#[derive(Debug, Clone, Serialize)]
pub struct OracleStatus {
    pub tool: String,
    pub tool_version: String,
    pub total_verdicts: u64,
    pub upgraded: u64,
    pub resolved_external: u64,
    pub confirmed: u64,
    pub contradicted: u64,
    pub last_run_status: Option<String>,
    pub last_run_commit_sha: Option<String>,
}

/// Build the status for a tool/version from the persisted side tables, scoped to the active
/// `(commit_sha, worktree_id)` checkout. The verdict counts go through the same scoped
/// `verdict_counts` the eval metrics use, so a sibling worktree's verdicts for the same
/// `(tool, tool_version)` never appear in this checkout's status (the same scope leak the round-3
/// metric fix closed — status is a read-only sibling of the metric path).
pub(crate) fn status(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<OracleStatus> {
    let VerdictCounts { total, upgraded, resolved_external, confirmed, contradicted } =
        run::verdict_counts(conn, tool, tool_version, commit_sha, worktree_id)?;
    let last_run = last_run_meta(conn, tool, tool_version, commit_sha, worktree_id)?;
    Ok(OracleStatus {
        tool: tool.as_db_str().to_string(),
        tool_version: tool_version.to_string(),
        total_verdicts: total,
        upgraded,
        resolved_external,
        confirmed,
        contradicted,
        last_run_status: last_run.as_ref().map(|run| run.0.clone()),
        last_run_commit_sha: last_run.map(|run| run.1),
    })
}

/// `(status, commit_sha)` of the most recent run for a tool/version **in the active checkout**, if
/// any. SCOPE (load-bearing): filtered to `(commit_sha, worktree_id)` so a sibling worktree's run
/// sharing the same `(tool, tool_version, commit_sha)` can't surface as THIS checkout's last run.
/// The verdict counts in `status` are already worktree-scoped; this keeps the run meta consistent
/// with them rather than describing a different checkout's pass.
fn last_run_meta(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Option<(String, String)>> {
    let row = conn
        .query_row(
            "
            SELECT status, commit_sha FROM oracle_runs
            WHERE tool = ?1 AND tool_version = ?2
              AND commit_sha = ?3 AND worktree_id = ?4
            ORDER BY id DESC
            LIMIT 1
            ",
            rusqlite::params![tool.as_db_str(), tool_version, commit_sha, worktree_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .ok();
    Ok(row)
}
