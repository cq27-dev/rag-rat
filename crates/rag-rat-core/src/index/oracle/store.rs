//! `oracle_runs` / `edge_oracle` persistence + the edge-candidate and symbol-mapping reads the
//! join needs. SQL helpers named for the domain question, per repo style.
//!
//! INVARIANT (load-bearing): nothing here ever UPDATEs `edges.resolution` / `edges.to_symbol_id`.
//! The heuristic resolution stays on the `edges` row untouched; the oracle's verdict lives only in
//! `edge_oracle`. This is what lets eval diff heuristic-vs-oracle. See `apply_oracle_tables`.

use rusqlite::{Connection, params};

use super::{OracleResolutionKind, OracleTool};
use crate::index::now_ms;

/// An edge candidate to feed the oracle join: the callee identifier byte range (the SCIP key, #67)
/// plus enough context to write a verdict and to recognise agreement/disagreement.
#[derive(Debug, Clone)]
pub(crate) struct EdgeJoinCandidate {
    pub(crate) edge_id: i64,
    pub(crate) source_path: String,
    pub(crate) file_sha: String,
    pub(crate) callee_start_byte: i64,
    pub(crate) callee_end_byte: i64,
    pub(crate) confidence: String,
    /// The edge's kind (`calls_name`, `references_type`, `uses_macro`, …). Only `calls_name` edges
    /// belong to the *call* population the recall metric measures; non-call kinds also carry a
    /// callee byte range (so they join), but must NOT count toward the covered-call side of
    /// recall.
    pub(crate) edge_kind: String,
    /// The heuristic's resolved target symbol id, if any (for confirm/contradict).
    pub(crate) to_symbol_id: Option<i64>,
}

/// The edge kind that denotes a *call* — the only population the recall metric measures. Other
/// kinds carrying a callee byte range (`references_type` / `uses_macro` / `implements` /
/// `constructs`) join against SCIP occurrences too, but a confirmation on them is not a covered
/// *call* and must be excluded from the recall numerator (the population-mismatch finding, #81).
pub(crate) const CALL_EDGE_KIND: &str = "calls_name";

/// One symbol's identity + byte span within a file, for mapping a SCIP definition range back to our
/// symbol table by containment overlap.
#[derive(Debug, Clone)]
pub(crate) struct SymbolSpan {
    pub(crate) symbol_id: i64,
    pub(crate) start_byte: i64,
    pub(crate) end_byte: i64,
}

/// Load every edge candidate that carries a callee identifier byte range, joined to its source
/// file's path + content sha. Only call-shaped edges set `callee_start_byte`, so the NULL filter
/// scopes this to exactly the rows the SCIP occurrence join can act on. Scoped to the active
/// commit/worktree via the `files` row the edge points at.
pub(crate) fn edge_join_candidates(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Vec<EdgeJoinCandidate>> {
    let mut stmt = conn.prepare(
        "
        SELECT edges.id,
               files.path,
               files.sha256,
               edges.callee_start_byte,
               edges.callee_end_byte,
               edges.confidence,
               edges.edge_kind,
               edges.to_symbol_id
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edges.callee_start_byte IS NOT NULL
          AND edges.callee_end_byte IS NOT NULL
          AND files.commit_sha = ?1
          AND files.worktree_id = ?2
        ORDER BY files.path, edges.callee_start_byte
        ",
    )?;
    let rows = stmt.query_map(params![commit_sha, worktree_id], |row| {
        Ok(EdgeJoinCandidate {
            edge_id: row.get(0)?,
            source_path: row.get(1)?,
            file_sha: row.get(2)?,
            callee_start_byte: row.get(3)?,
            callee_end_byte: row.get(4)?,
            confidence: row.get(5)?,
            edge_kind: row.get(6)?,
            to_symbol_id: row.get(7)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// All symbols defined in a file (by path, scoped to commit/worktree), with their byte spans, so a
/// SCIP definition range can be mapped to the enclosing symbol by overlap.
pub(crate) fn symbol_spans_for_path(
    conn: &Connection,
    path: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Vec<SymbolSpan>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.start_byte, symbols.end_byte
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE files.path = ?1
          AND files.commit_sha = ?2
          AND files.worktree_id = ?3
        ORDER BY symbols.start_byte
        ",
    )?;
    let rows = stmt.query_map(params![path, commit_sha, worktree_id], |row| {
        Ok(SymbolSpan { symbol_id: row.get(0)?, start_byte: row.get(1)?, end_byte: row.get(2)? })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The set of file paths rag-rat indexed in the active `(commit_sha, worktree_id)` checkout. Used
/// by the recall gap to drop occurrences originating in a SOURCE document outside the indexed
/// corpus.
///
/// SCOPE (load-bearing): an occurrence's `path` is the document the *call site* lives in. The
/// recall gap asks "did the heuristic miss a call the oracle saw?" — but no edge candidate can ever
/// cover a call from a file rag-rat never indexed (an excluded test/example/generated/dependency
/// source the `.scip` covers), so such an occurrence is not a miss, it is out of scope. Resolving
/// the callee *definition* to an indexed symbol (the other recall-gap filter) is NOT enough: the
/// def can be indexed while the occurrence's own source file is not. This set is the
/// occurrence-side scope, the mirror of the definition-side `symbol_spans_for_path` check — both
/// must hold for a recall gap.
pub(crate) fn indexed_paths_in_scope(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<std::collections::HashSet<String>> {
    // EXCLUDE tombstones: `mark_file_deleted` leaves a `kind='deleted'` row in `files` for a path
    // removed from the checkout (so incremental sync can detect the deletion). Its source is no
    // longer indexed, so an occurrence whose call site is that path can never be covered by an edge
    // candidate (`edge_join_candidates` won't emit one) — counting it would falsely inflate the
    // recall gap. The deleted row must NOT count as "indexed in scope".
    let mut stmt = conn.prepare(
        "SELECT path FROM files WHERE commit_sha = ?1 AND worktree_id = ?2 AND kind != 'deleted'",
    )?;
    let rows = stmt.query_map(params![commit_sha, worktree_id], |row| row.get::<_, String>(0))?;
    let mut out = std::collections::HashSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}

/// Delete the `edge_oracle` verdicts for a `(tool, tool_version)` scope **within the active
/// checkout**. Called at the start of a run so the pass is **authoritative** for its tool version:
/// an edge the prior `.scip` covered but the current one no longer yields a verdict for must NOT
/// keep its stale verdict (upsert alone only overwrites edges the current run revisits — it leaves
/// dropped edges' rows intact). Run inside the same transaction that writes the current verdicts so
/// the table is never observed mid-clear.
///
/// SCOPE (load-bearing): the DELETE is restricted to verdicts whose edge belongs to the run's
/// `(commit_sha, worktree_id)` checkout — the SAME `edge_oracle -> edges -> files` scope join
/// `edge_join_candidates` writes through. Without it, a run in one worktree would erase another
/// worktree's valid verdicts for the same `(tool, tool_version)` in a multi-checkout DB. The
/// authoritative-clear is per-checkout, not global.
pub(crate) fn clear_edge_oracle_for_tool(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "
        DELETE FROM edge_oracle
        WHERE tool = ?1 AND tool_version = ?2
          AND edge_id IN (
            SELECT edges.id
            FROM edges
            JOIN files ON files.id = edges.source_file_id
            WHERE files.commit_sha = ?3 AND files.worktree_id = ?4
          )
        ",
        params![tool.as_db_str(), tool_version, commit_sha, worktree_id],
    )?;
    Ok(())
}

/// A verdict to persist for one edge.
pub(crate) struct EdgeOracleRow<'a> {
    pub(crate) edge_id: i64,
    pub(crate) file_sha: &'a str,
    pub(crate) resolved_symbol_id: Option<i64>,
    pub(crate) scip_symbol: &'a str,
    pub(crate) kind: OracleResolutionKind,
}

/// Upsert one `edge_oracle` row. Keyed by `(edge_id, tool, tool_version)`; re-running the same tool
/// version overwrites the prior verdict (content addressed by `file_sha`). NEVER touches `edges`.
pub(crate) fn write_edge_oracle(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    row: &EdgeOracleRow<'_>,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO edge_oracle(
            edge_id, file_sha, tool, tool_version,
            resolved_symbol_id, scip_symbol, kind, computed_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ON CONFLICT(edge_id, tool, tool_version) DO UPDATE SET
            file_sha = excluded.file_sha,
            resolved_symbol_id = excluded.resolved_symbol_id,
            scip_symbol = excluded.scip_symbol,
            kind = excluded.kind,
            computed_at = excluded.computed_at
        ",
        params![
            row.edge_id,
            row.file_sha,
            tool.as_db_str(),
            tool_version,
            row.resolved_symbol_id,
            row.scip_symbol,
            row.kind.as_db_str(),
            now_ms(),
        ],
    )?;
    Ok(())
}

/// Record an oracle run, returning its row id. `stats_json` is an opaque `OracleReport` snapshot.
/// `worktree_id` scopes the run to the active checkout so the status read's `last_run_meta` can
/// distinguish this checkout's run from a sibling worktree's run under the same
/// `(tool, tool_version, commit_sha)`.
pub(crate) fn record_oracle_run(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    status: &str,
    stats_json: &str,
) -> anyhow::Result<i64> {
    conn.execute(
        "
        INSERT INTO oracle_runs(
            tool, tool_version, commit_sha, worktree_id, started_at, status, stats_json
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ",
        params![
            tool.as_db_str(),
            tool_version,
            commit_sha,
            worktree_id,
            now_ms(),
            status,
            stats_json
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The single canonical scope predicate every `edge_oracle` metric read joins through: restrict the
/// counted verdicts to those whose edge belongs to the active `(commit_sha, worktree_id)` checkout,
/// via `edge_oracle -> edges -> files`. This is the SAME join `edge_join_candidates` /
/// `clear_edge_oracle_for_tool` write through, so every numerator and denominator covers the same
/// checkout by construction.
///
/// SCOPE (load-bearing, DRY): do NOT re-spell this predicate per query. A new `edge_oracle` count
/// must call [`count_edge_oracle_scoped`] (passing the kind filter, if any) so it cannot forget the
/// scope — a global `WHERE tool = ? AND tool_version = ?` count mixes another worktree's verdicts
/// into this checkout's precision/recall (the round-3 Codex finding on `verdict_counts`). The
/// `?1..?4` bind slots are always `(tool, tool_version, commit_sha, worktree_id)`.
pub(crate) const EDGE_ORACLE_SCOPE_JOIN: &str = "
    FROM edge_oracle
    JOIN edges ON edges.id = edge_oracle.edge_id
    JOIN files ON files.id = edges.source_file_id
    WHERE edge_oracle.tool = ?1 AND edge_oracle.tool_version = ?2
      AND files.commit_sha = ?3 AND files.worktree_id = ?4";

/// Count `edge_oracle` rows for `(tool, tool_version)` **within the active checkout**, optionally
/// filtered to a single `kind`. The one scoped count helper behind both the total and the per-kind
/// status/metric reads — every caller routes through [`EDGE_ORACLE_SCOPE_JOIN`], so the scope can
/// never be re-spelled (and thus can't be forgotten) per query.
pub(crate) fn count_edge_oracle_scoped(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    kind: Option<OracleResolutionKind>,
) -> anyhow::Result<u64> {
    let count: i64 = match kind {
        Some(kind) => conn.query_row(
            &format!("SELECT COUNT(*){EDGE_ORACLE_SCOPE_JOIN} AND edge_oracle.kind = ?5"),
            params![tool.as_db_str(), tool_version, commit_sha, worktree_id, kind.as_db_str()],
            |row| row.get(0),
        )?,
        None => conn.query_row(
            &format!("SELECT COUNT(*){EDGE_ORACLE_SCOPE_JOIN}"),
            params![tool.as_db_str(), tool_version, commit_sha, worktree_id],
            |row| row.get(0),
        )?,
    };
    Ok(u64::try_from(count).unwrap_or(0))
}
