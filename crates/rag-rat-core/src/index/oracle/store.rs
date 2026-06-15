//! `oracle_runs` / `edge_oracle` persistence + the edge-candidate and symbol-mapping reads the
//! join needs. SQL helpers named for the domain question, per repo style.
//!
//! INVARIANT (load-bearing): nothing here ever UPDATEs `edges.resolution` / `edges.to_symbol_id`.
//! The heuristic resolution stays on the `edges` row untouched; the oracle's verdict lives only in
//! `edge_oracle`. This is what lets eval diff heuristic-vs-oracle. See `apply_oracle_tables`.

use rusqlite::{Connection, params};

use super::{OracleResolutionKind, OracleTool};
use crate::index::now_ms;

/// Render the **active-checkout** `files` predicate for an oracle scope read/write, binding the
/// commit-sha at SQL param `sha_param` and the worktree-id at `wt_param`. This is the SINGLE source
/// of the index's real checkout semantics for the oracle — the same rule
/// `rebuild.rs::clear_full_rebuild_tables` stages file ids by, expressed inline so any
/// `files`-anchored oracle query can `AND` it in.
///
/// SCOPE (load-bearing, #82 P0): a production `files` row carries EITHER `(commit_sha, '')` (clean,
/// `FileScope::commit`) OR `('', worktree_id)` (dirty overlay, `FileScope::worktree`) — NEVER both
/// (see `index/mod.rs::FileScope`, `incremental.rs::assign_file_scopes`). The old predicate
/// `files.commit_sha = ?sha AND files.worktree_id = ?wt` with BOTH non-empty therefore matched ZERO
/// rows on any real git checkout, silently writing 0 verdicts. A row is in the active checkout iff
/// the dirty overlay claims it (`worktree_id = wt`, overlay wins) OR the committed row does
/// (`commit_sha = sha`) AND no dirty overlay shadows that path. Both halves guard against the empty
/// sentinel so a non-git index (`commit_sha = ''`) doesn't degenerate into "every row matches".
pub(crate) fn active_checkout_file_predicate(sha_param: &str, wt_param: &str) -> String {
    format!(
        "((files.worktree_id = {wt_param} AND files.worktree_id != '') OR (files.commit_sha = \
         {sha_param} AND files.commit_sha != '' AND files.path NOT IN (SELECT path FROM files \
         WHERE worktree_id = {wt_param} AND worktree_id != '')))"
    )
}

/// The heuristic "before" resolution counts over the active checkout's edge candidates (those with
/// a callee range — the oracle's `edges_examined` population). `(total, resolved_in_corpus,
/// unresolved)`:
/// - `resolved_in_corpus` mirrors the join's [`super::join`] `heuristic_resolved_in_corpus`:
///   confidence `Exact`/`Syntactic` AND a non-NULL `to_symbol_id`. Same definition both sides, so
///   `resolved_after = resolved_before + upgraded + resolved_external` is consistent.
/// - `unresolved` is the low-confidence (`NameOnly`/`Ambiguous`) population (the oracle's
///   upgrade/recovery denominator).
pub(crate) fn resolution_before_counts(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<(u64, u64, u64)> {
    let scope = active_checkout_file_predicate("?1", "?2");
    let (total, resolved, unresolved): (i64, i64, i64) = conn.query_row(
        &format!(
            "
        SELECT
          COUNT(*),
          COUNT(*) FILTER (
            WHERE edges.confidence IN ('Exact', 'Syntactic') AND edges.to_symbol_id IS NOT NULL
          ),
          COUNT(*) FILTER (WHERE edges.confidence IN ('NameOnly', 'Ambiguous'))
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edges.callee_start_byte IS NOT NULL
          AND {scope}
        ",
        ),
        params![commit_sha, worktree_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok((
        u64::try_from(total).unwrap_or(0),
        u64::try_from(resolved).unwrap_or(0),
        u64::try_from(unresolved).unwrap_or(0),
    ))
}

/// The number of logical symbols enriched with a SCIP moniker for `tool` — the "symbols enriched"
/// signal (#70). `logical_symbol_monikers` is keyed `(logical_symbol_id, tool)` and is not
/// checkout-scoped (logical symbols are repo-global), so this is a straight per-tool count.
pub(crate) fn count_symbols_with_moniker(
    conn: &Connection,
    tool: OracleTool,
) -> anyhow::Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM logical_symbol_monikers WHERE tool = ?1",
        params![tool.as_db_str()],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

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
    let mut stmt = conn.prepare(&format!(
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
          AND {scope}
        ORDER BY files.path, edges.callee_start_byte
        ",
        scope = active_checkout_file_predicate("?1", "?2"),
    ))?;
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
    let mut stmt = conn.prepare(&format!(
        "
        SELECT symbols.id, symbols.start_byte, symbols.end_byte
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE files.path = ?1
          AND {scope}
        ORDER BY symbols.start_byte
        ",
        scope = active_checkout_file_predicate("?2", "?3"),
    ))?;
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
    let mut stmt = conn.prepare(&format!(
        "SELECT path FROM files WHERE {scope} AND kind != 'deleted'",
        scope = active_checkout_file_predicate("?1", "?2"),
    ))?;
    let rows = stmt.query_map(params![commit_sha, worktree_id], |row| row.get::<_, String>(0))?;
    let mut out = std::collections::HashSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}

/// The `files.sha256` of every file indexed in the active `(commit_sha, worktree_id)` checkout,
/// keyed by path. The moniker pass uses it as its index-vs-disk drift gate: a SCIP definition in a
/// document whose disk bytes no longer match the indexed content was byte-converted against a
/// coordinate space the symbol spans don't share, so its moniker must not be written. Tombstones
/// are excluded for the same reason as [`indexed_paths_in_scope`].
pub(crate) fn indexed_file_shas_in_scope(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT path, sha256 FROM files WHERE {scope} AND kind != 'deleted'",
        scope = active_checkout_file_predicate("?1", "?2"),
    ))?;
    let rows = stmt.query_map(params![commit_sha, worktree_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (path, sha) = row?;
        out.insert(path, sha);
    }
    Ok(out)
}

/// The logical symbol a member symbol belongs to, if grouped. Local copy of the
/// `logical_symbol_members` read so the oracle layer doesn't reach into `query::memory`.
pub(crate) fn logical_symbol_id_for_member(
    conn: &Connection,
    symbol_id: i64,
) -> anyhow::Result<Option<i64>> {
    use rusqlite::OptionalExtension as _;
    conn.query_row(
        "SELECT logical_symbol_id FROM logical_symbol_members WHERE symbol_id = ?1 LIMIT 1",
        [symbol_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Delete every `logical_symbol_monikers` row for `tool`. Called at the start of a run, inside the
/// run's transaction, so the moniker set is **authoritative** for the latest run: a symbol the
/// current `.scip` no longer defines must not keep a stale moniker, and dangling rows (whose
/// content-derived logical id died in a rebuild) are swept on every run. The clear is global for
/// the tool — `logical_symbols` itself is rebuilt unscoped from all symbols, so monikers follow
/// the same (single-checkout-in-practice) lifecycle rather than inventing a per-checkout scope the
/// parent table doesn't have.
pub(crate) fn clear_logical_symbol_monikers_for_tool(
    conn: &Connection,
    tool: OracleTool,
) -> anyhow::Result<()> {
    conn.execute("DELETE FROM logical_symbol_monikers WHERE tool = ?1", [tool.as_db_str()])?;
    Ok(())
}

/// Upsert one moniker row: this logical symbol's SCIP symbol string per `tool`, with the version
/// that produced it. PK `(logical_symbol_id, tool)` — one moniker per logical symbol per tool;
/// cfg-gated variants share the logical symbol, hence the moniker, by construction (#70).
pub(crate) fn write_logical_symbol_moniker(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    logical_symbol_id: i64,
    moniker: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO logical_symbol_monikers(logical_symbol_id, tool, tool_version, moniker, \
         computed_at)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(logical_symbol_id, tool) DO UPDATE SET
            tool_version = excluded.tool_version,
            moniker = excluded.moniker,
            computed_at = excluded.computed_at
        ",
        params![logical_symbol_id, tool.as_db_str(), tool_version, moniker, now_ms()],
    )?;
    Ok(())
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
        &format!(
            "
        DELETE FROM edge_oracle
        WHERE tool = ?1 AND tool_version = ?2
          AND edge_id IN (
            SELECT edges.id
            FROM edges
            JOIN files ON files.id = edges.source_file_id
            WHERE {scope}
          )
        ",
            scope = active_checkout_file_predicate("?3", "?4"),
        ),
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
/// Record a run stamped at `now_ms()`. Test-only convenience over [`record_oracle_run_at`]; every
/// production path threads the real start time through `record_oracle_run_at` (#145).
#[cfg(test)]
pub(crate) fn record_oracle_run(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    status: &str,
    stats_json: &str,
) -> anyhow::Result<i64> {
    record_oracle_run_at(
        conn,
        tool,
        tool_version,
        commit_sha,
        worktree_id,
        now_ms(),
        status,
        stats_json,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_oracle_run_at(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    started_at_ms: i64,
    status: &str,
    stats_json: &str,
) -> anyhow::Result<i64> {
    // `started_at_ms` is the moment the run actually BEGAN (the pre-spawn snapshot), passed in by
    // the caller — NOT `now_ms()` at completion. The auto-run staleness gate compares this
    // against the index's last-change clock; stamping completion time made a run that
    // overlapped a watcher reindex look fresher than the edits it skipped, wedging the gate at
    // NotStale (#145).
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
            started_at_ms,
            status,
            stats_json
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The `tool_version` of the most recent run for `tool` **in the active checkout**, if any. This is
/// the version the surfacing reads (the `Compiler` tier) key on: query output should show the
/// verdicts the last `oracle run` for this checkout produced. Scoped to `(commit_sha, worktree_id)`
/// so a sibling worktree's run can't dictate this checkout's displayed tool version.
///
/// Ordered by `id DESC` = INSERTION order = COMPLETION order (the row is written at the end of
/// `run::run`, under the write lock). NOT `started_at DESC`: since #145 `started_at` is the run's
/// START, which is no longer monotonic with completion — two overlapping runs (a manual + the
/// background auto-run) can finish in the opposite order they started, and the authoritative
/// `edge_oracle` writer is the one that finished LAST (highest id), not the one that started last.
pub(crate) fn latest_run_tool_version(
    conn: &Connection,
    tool: OracleTool,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Option<String>> {
    let version = conn
        .query_row(
            "
            SELECT tool_version FROM oracle_runs
            WHERE tool = ?1 AND commit_sha = ?2 AND worktree_id = ?3
            ORDER BY id DESC
            LIMIT 1
            ",
            params![tool.as_db_str(), commit_sha, worktree_id],
            |row| row.get::<_, String>(0),
        )
        .ok();
    Ok(version)
}

/// The `started_at` (Unix-epoch ms) of the LAST-COMPLETED run for `tool` **in the active
/// checkout**, or `None` when no run exists. The staleness clock the background auto-fresh oracle
/// compares against the index's `indexed_at_ms` (see [`crate::index::oracle::auto_run_decision`]):
/// a run that started after the last index change means its verdicts are current. Scoped to
/// `(commit_sha, worktree_id)` — the sibling of [`latest_run_tool_version`], ordered the same way
/// (`id DESC` = completion order, so the start time returned belongs to the run whose verdicts are
/// actually live; see that fn for why started_at ordering is wrong since #145).
pub(crate) fn latest_run_started_at(
    conn: &Connection,
    tool: OracleTool,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Option<i64>> {
    let started_at = conn
        .query_row(
            "
            SELECT started_at FROM oracle_runs
            WHERE tool = ?1 AND commit_sha = ?2 AND worktree_id = ?3
            ORDER BY id DESC
            LIMIT 1
            ",
            params![tool.as_db_str(), commit_sha, worktree_id],
            |row| row.get::<_, i64>(0),
        )
        .ok();
    Ok(started_at)
}

/// Whether ANY oracle run exists in the active checkout, across all tools — one query to short out
/// the per-tool [`latest_run_tool_version`] probes on the dominant "no oracle ever" path (where the
/// table is empty, so this returns instantly). Scoped to `(commit_sha, worktree_id)`.
pub(crate) fn any_run_in_scope(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<bool> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM oracle_runs WHERE commit_sha = ?1 AND worktree_id = ?2 LIMIT 1",
            params![commit_sha, worktree_id],
            |_| Ok(()),
        )
        .ok()
        .is_some();
    Ok(exists)
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
///
/// Returns a `String` (not a `const`) because the active-checkout file predicate
/// ([`active_checkout_file_predicate`]) is an OR-of-overlay-vs-committed expression, not a fixed
/// `AND` — #82 P0. Slots `?3`/`?4` (commit_sha / worktree_id) flow into it; `?1`/`?2`
/// (tool / version) gate the side table. Callers append ` AND <extra>` exactly as before — the
/// statement still ends in a trailing `WHERE`, so a JOIN can't legally follow.
pub(crate) fn edge_oracle_scope_join() -> String {
    format!(
        "
    FROM edge_oracle
    JOIN edges ON edges.id = edge_oracle.edge_id
    JOIN files ON files.id = edges.source_file_id
    WHERE edge_oracle.tool = ?1 AND edge_oracle.tool_version = ?2
      AND {scope}",
        scope = active_checkout_file_predicate("?3", "?4"),
    )
}

/// Count `edge_oracle` rows for `(tool, tool_version)` **within the active checkout**, optionally
/// filtered to a single `kind`. The one scoped count helper behind both the total and the per-kind
/// status/metric reads — every caller routes through [`edge_oracle_scope_join`], so the scope can
/// never be re-spelled (and thus can't be forgotten) per query.
pub(crate) fn count_edge_oracle_scoped(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    kind: Option<OracleResolutionKind>,
) -> anyhow::Result<u64> {
    let scope_join = edge_oracle_scope_join();
    let count: i64 = match kind {
        Some(kind) => conn.query_row(
            &format!("SELECT COUNT(*){scope_join} AND edge_oracle.kind = ?5"),
            params![tool.as_db_str(), tool_version, commit_sha, worktree_id, kind.as_db_str()],
            |row| row.get(0),
        )?,
        None => conn.query_row(
            &format!("SELECT COUNT(*){scope_join}"),
            params![tool.as_db_str(), tool_version, commit_sha, worktree_id],
            |row| row.get(0),
        )?,
    };
    Ok(u64::try_from(count).unwrap_or(0))
}

/// The CURRENT-content predicate appended to [`edge_oracle_scope_join`] for any READ that surfaces
/// a verdict in query output (the `Compiler` tier). A verdict is valid for display ONLY when BOTH:
///
/// 1. **The callsite file is unchanged** — `edge_oracle.file_sha == files.sha256` for the edge's
///    current source file. A drifted/changed callsite's `file_sha` differs, so the verdict is
///    filtered out and the edge reverts to heuristic display (`oracle-stale`), never `Compiler`.
/// 2. **The resolved definition is unchanged** (in-corpus verdicts only) — the resolved symbol
///    `edge_oracle.resolved_symbol_id` STILL EXISTS **in the active checkout scope**. The callsite
///    gate (1) alone misses *definition* drift: an `Upgrade`/`Confirm` keeps surfacing after the
///    resolved *def* file changed or its symbol was deleted/reinserted, because the callsite file
///    is untouched so its sha still matches (#82 finding 3). Since `symbols.id` is AUTOINCREMENT,
///    reindexing the def file mints NEW ids and the old `resolved_symbol_id` dangles — so an
///    `EXISTS` against `symbols.id` reverts the verdict to heuristic the moment the def file is
///    reindexed. `resolved-external` verdicts (`resolved_symbol_id IS NULL`) skip this clause —
///    there is no in-corpus def to drift.
///
///    SCOPE (load-bearing, #82 P2): the EXISTS must join `symbols -> files` and apply the
///    active-checkout predicate, NOT check the RAW `symbols` table. A dirty def file makes the
///    indexer insert a worktree-scoped symbol row while leaving the old commit-scoped symbols
///    shadowed-but-present — so a raw `EXISTS (symbols.id = resolved_symbol_id)` still finds the
///    stale id and keeps surfacing a `Compiler` verdict pointing at the pre-edit target (the
///    callsite sha is unchanged). Scoping the EXISTS to the active checkout means a shadowed
///    commit-scoped def no longer counts, so the verdict reverts to heuristic.
///
/// This is the read-side mirror of the run-time content-integrity gate (run.rs) and the staleness
/// key `(file_sha, tool, tool_version)`.
///
/// (The eval/status COUNTS in [`count_edge_oracle_scoped`] deliberately do NOT apply this — they
/// describe the persisted verdict population for precision/recall, which is content-addressed by
/// the run, not the live display. Only the surfacing reads gate on currency.)
///
/// Returns a `String` (not a `const`) because the scope-aware EXISTS embeds
/// [`active_checkout_file_predicate`] (`?3`/`?4`). The leading space + `AND` are kept so it appends
/// cleanly after [`edge_oracle_scope_join`]'s trailing `WHERE`.
pub(crate) fn edge_oracle_current_predicate() -> String {
    format!(
        " AND edge_oracle.file_sha = files.sha256 AND (edge_oracle.resolved_symbol_id IS NULL OR \
         EXISTS (SELECT 1 FROM symbols JOIN files ON files.id = symbols.file_id WHERE symbols.id \
         = edge_oracle.resolved_symbol_id AND {scope}))",
        scope = active_checkout_file_predicate("?3", "?4"),
    )
}

/// One current, in-scope oracle verdict for an edge, surfaced in graph/impact query output as the
/// `Compiler` tier. `package` is the external dependency name for `resolved-external` verdicts
/// (`scip_symbol`'s package component), `None` for in-corpus resolutions.
#[derive(Debug, Clone)]
pub(crate) struct EdgeOracleVerdict {
    pub(crate) kind: OracleResolutionKind,
    /// The qualified name of the verdict's in-corpus `resolved_symbol_id` (joined at read time),
    /// `None` when the verdict is external (`resolved_symbol_id IS NULL`) or the resolved symbol
    /// no longer exists. An `Upgrade`'s target is hydrated from THIS name — never the
    /// heuristic edge's stale/heuristic target — and an `Upgrade` whose resolved symbol can't
    /// be surfaced (`None`) is NOT promoted to `compiler`: we won't attach the tier to a
    /// target we can't name (#82 finding 2). The def-drift gate in
    /// [`edge_oracle_current_predicate`] already filters a deleted/reinserted resolved symbol,
    /// so in practice this is `None` only for externals.
    pub(crate) resolved_qualified_name: Option<String>,
    pub(crate) scip_symbol: String,
    pub(crate) tool: OracleTool,
    pub(crate) tool_version: String,
}

impl EdgeOracleVerdict {
    /// The provenance string surfaced as `resolution_reason` when this verdict upgrades an edge to
    /// the `Compiler` tier: `scip:<tool>@<version>` (the #61 design's reason format).
    pub(crate) fn resolution_reason(&self) -> String {
        format!("scip:{}@{}", self.tool.as_db_str(), self.tool_version)
    }

    /// `resolved-external(<package>)` when the oracle placed this callee in a dependency outside
    /// the corpus, deriving `<package>` from the SCIP symbol's package component; `None` for
    /// in-corpus verdicts. The display string surfaced in query output for
    /// unresolved-but-externally-resolved edges.
    pub(crate) fn resolved_external_label(&self) -> Option<String> {
        if self.kind != OracleResolutionKind::ResolvedExternal {
            return None;
        }
        let package = super::join::package_of(&self.scip_symbol)?;
        Some(format!("resolved-external({package})"))
    }
}

/// Fetch the CURRENT, in-scope oracle verdicts for a set of edge ids (the read-side join that
/// surfaces the `Compiler` tier in `trace_callees` / `find_callers` / `impact_surface`). Scoped to
/// the active `(commit_sha, worktree_id)` via [`edge_oracle_scope_join`] AND gated to current
/// content via [`edge_oracle_current_predicate`], so a drifted file's verdict is never returned
/// (its edge reverts to heuristic display). Returns at most one row per edge id (PK
/// `(edge_id, tool, tool_version)` and the single-tool scope).
///
/// SCOPE (load-bearing): this is the ONLY place a surfacing read of `edge_oracle` lives — it routes
/// through the shared scope predicate so the checkout scope can't be dropped, exactly like the
/// metric reads. Callers pass `edge_ids` already produced by the heuristic traversal (themselves
/// scoped), so the returned verdicts are a subset, never an expansion.
pub(crate) fn current_oracle_verdicts_for_edges(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    edge_ids: &[i64],
) -> anyhow::Result<std::collections::HashMap<i64, EdgeOracleVerdict>> {
    let mut out = std::collections::HashMap::new();
    if edge_ids.is_empty() {
        return Ok(out);
    }
    // Bind the variable-length id list after the fixed ?1..?4 scope slots. Chunk to stay under
    // SQLite's bound-variable limit on large traversals.
    for chunk in edge_ids.chunks(900) {
        let placeholders =
            (0..chunk.len()).map(|i| format!("?{}", i + 5)).collect::<Vec<_>>().join(", ");
        // The resolved symbol's qualified name is pulled via a correlated subquery (not a trailing
        // JOIN) so the shared `edge_oracle_scope_join()`/`edge_oracle_current_predicate()` strings
        // — which end in a WHERE — stay the single source of the scope+current predicate. A
        // deleted/reinserted
        // resolved symbol yields NULL here, so the `Upgrade` target can't be surfaced and the hop
        // is left heuristic (#82 finding 2).
        let sql = format!(
            "SELECT edge_oracle.edge_id, edge_oracle.kind, edge_oracle.resolved_symbol_id, \
             (SELECT qualified_name FROM symbols WHERE symbols.id = \
             edge_oracle.resolved_symbol_id), edge_oracle.scip_symbol{scope_join}{current} AND \
             edge_oracle.edge_id IN ({placeholders})",
            scope_join = edge_oracle_scope_join(),
            current = edge_oracle_current_predicate(),
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(tool.as_db_str().to_string()),
            Box::new(tool_version.to_string()),
            Box::new(commit_sha.to_string()),
            Box::new(worktree_id.to_string()),
        ];
        for id in chunk {
            params.push(Box::new(*id));
        }
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let edge_id: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            // Column 2 (`resolved_symbol_id`) is selected so the def-drift EXISTS clause in the
            // CURRENT predicate can reference it, but the verdict carries the joined NAME, not the
            // raw id.
            let resolved_qualified_name: Option<String> = row.get(3)?;
            let scip_symbol: String = row.get(4)?;
            Ok((edge_id, kind, resolved_qualified_name, scip_symbol))
        })?;
        for row in rows {
            let (edge_id, kind, resolved_qualified_name, scip_symbol) = row?;
            let Some(kind) = OracleResolutionKind::from_db_str(&kind) else {
                continue;
            };
            out.insert(edge_id, EdgeOracleVerdict {
                kind,
                resolved_qualified_name,
                scip_symbol,
                tool,
                tool_version: tool_version.to_string(),
            });
        }
    }
    Ok(out)
}

/// Fetch ALL current, in-scope oracle verdicts for `(tool, tool_version)` in this checkout, keyed
/// by `edge_id` → `(kind, resolved_symbol_id)`. The single-scan sibling of
/// [`current_oracle_verdicts_for_edges`] (which filters to a bounded id list): symbol-importance
/// ranking ([`crate::query::pagerank`]) walks the WHOLE edge graph, so it needs the verdict set in
/// one pass rather than a query per edge. Routes through the shared [`edge_oracle_scope_join`] +
/// [`edge_oracle_current_predicate`] so the scope+currency gate can't be re-spelled — the only raw
/// `FROM edge_oracle` lives here (#81). `resolved_symbol_id` is returned (not just its name)
/// because the ranker needs the id to retarget an `Upgrade` edge.
pub(crate) fn current_oracle_verdicts_all(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<std::collections::HashMap<i64, (OracleResolutionKind, Option<i64>)>> {
    let sql = format!(
        "SELECT edge_oracle.edge_id, edge_oracle.kind, \
         edge_oracle.resolved_symbol_id{scope_join}{current}",
        scope_join = edge_oracle_scope_join(),
        current = edge_oracle_current_predicate(),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows =
        stmt.query_map(params![tool.as_db_str(), tool_version, commit_sha, worktree_id], |row| {
            let edge_id: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let resolved_symbol_id: Option<i64> = row.get(2)?;
            Ok((edge_id, kind, resolved_symbol_id))
        })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (edge_id, kind, resolved_symbol_id) = row?;
        let Some(kind) = OracleResolutionKind::from_db_str(&kind) else {
            continue;
        };
        out.insert(edge_id, (kind, resolved_symbol_id));
    }
    Ok(out)
}

/// One row of the `compare_graph_to_scip` diagnostic: an edge and the SCIP verdict that
/// contradicts / agrees with its heuristic resolution, with enough edge context to render the
/// disagreement. The compare tool filters to `Contradict` kinds; the total it scans is every
/// current verdict in scope.
#[derive(Debug, Clone)]
pub(crate) struct EdgeOracleComparison {
    pub(crate) edge_id: i64,
    pub(crate) kind: OracleResolutionKind,
    pub(crate) edge_kind: String,
    pub(crate) heuristic_confidence: String,
    pub(crate) heuristic_target: Option<String>,
    pub(crate) callee_name: Option<String>,
    /// Our `symbols.id` when the compiler resolved this callee to an IN-CORPUS symbol (an
    /// in-corpus `Contradict`: the compiler picked a different in-corpus target), `None` when
    /// it placed the callee in a dependency. `compare_graph_to_scip` labels
    /// `resolved_external` ONLY when this is `None` — a Rust SCIP symbol carries a
    /// crate/package component even for the LOCAL crate, so deriving `resolved-external` from
    /// `scip_symbol` alone would mislabel an in-corpus contradiction as
    /// `resolved-external(<local-crate>)` (#82 finding 1).
    pub(crate) resolved_symbol_id: Option<i64>,
    pub(crate) scip_symbol: String,
    pub(crate) callsite_path: String,
    pub(crate) callsite_line: i64,
}

/// Load every CURRENT, in-scope `edge_oracle` verdict joined to its edge's heuristic resolution —
/// the data `compare_graph_to_scip` diffs (it keeps the `Contradict` rows). Scoped to the active
/// `(commit_sha, worktree_id)` via [`edge_oracle_scope_join`] AND gated to current content via
/// [`edge_oracle_current_predicate`], so a drifted/dirty file's verdict is never reported as a
/// disagreement (it reverted to heuristic display). The heuristic target's qualified name comes
/// from the `to_symbols` join (the `edges.to_symbol_id` the heuristic picked).
pub(crate) fn current_oracle_comparisons(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Vec<EdgeOracleComparison>> {
    // The heuristic target's qualified name is fetched via a correlated subquery rather than a
    // trailing LEFT JOIN, so the shared `edge_oracle_scope_join` string (which already ends in a
    // WHERE) stays the single source of the scope predicate — a JOIN can't legally follow a WHERE.
    let sql = format!(
        "SELECT edge_oracle.edge_id, edge_oracle.kind, edges.edge_kind, edges.confidence, (SELECT \
         qualified_name FROM symbols WHERE symbols.id = edges.to_symbol_id), edges.to_name, \
         edge_oracle.resolved_symbol_id, edge_oracle.scip_symbol, files.path, \
         COALESCE(NULLIF(edges.source_start_line, 0), 1) {scope_join}{current} ORDER BY \
         files.path, edges.source_start_line",
        scope_join = edge_oracle_scope_join(),
        current = edge_oracle_current_predicate(),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows =
        stmt.query_map(params![tool.as_db_str(), tool_version, commit_sha, worktree_id], |row| {
            let kind: String = row.get(1)?;
            Ok((
                row.get::<_, i64>(0)?,
                kind,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })?;
    let mut out = Vec::new();
    for row in rows {
        let (
            edge_id,
            kind,
            edge_kind,
            heuristic_confidence,
            heuristic_target,
            callee_name,
            resolved_symbol_id,
            scip_symbol,
            callsite_path,
            callsite_line,
        ) = row?;
        let Some(kind) = OracleResolutionKind::from_db_str(&kind) else {
            continue;
        };
        out.push(EdgeOracleComparison {
            edge_id,
            kind,
            edge_kind,
            heuristic_confidence,
            heuristic_target,
            callee_name,
            resolved_symbol_id,
            scip_symbol,
            callsite_path,
            callsite_line,
        });
    }
    Ok(out)
}

/// Prune `oracle_runs` rows whose `(commit_sha, worktree_id)` is NOT live — the gc companion to the
/// `edge_oracle` FK cascade (which already drops verdict rows when their edges are deleted). Unlike
/// `edge_oracle`, `oracle_runs` is keyed by `(commit_sha, worktree_id)` directly, not by an edge,
/// so nothing cascades it; a dead checkout's run rows would otherwise linger after gc drops its
/// edges/files. Refuses to prune when both live sets are empty (mirrors `prune_to_live`), so a
/// missing live set never wipes all run history. Returns the number of rows deleted.
///
/// A run survives iff its commit is live OR its worktree overlay is live — the SAME survival rule
/// `prune_to_live` applies to `files`, so a run and the edges it produced are pruned together.
pub(crate) fn prune_oracle_runs_outside_scope(
    conn: &Connection,
    live_commits: &[String],
    live_worktrees: &[String],
) -> anyhow::Result<u64> {
    if live_commits.is_empty() && live_worktrees.is_empty() {
        return Ok(0);
    }
    let commit_list = sql_quoted_list(live_commits);
    let worktree_list = sql_quoted_list(live_worktrees);
    let deleted = conn.execute(
        &format!(
            "DELETE FROM oracle_runs
             WHERE commit_sha NOT IN ({commit_list})
               AND worktree_id NOT IN ({worktree_list})"
        ),
        [],
    )?;
    Ok(u64::try_from(deleted).unwrap_or(0))
}

/// Render a slice of strings as a SQL `IN (...)` value list with single-quote escaping. Inputs are
/// commit shas / worktree ids (hex / path-derived), never user free-text, but we escape `'` anyway
/// so the list can't break the statement. An empty slice yields `''` (a value nothing matches),
/// which is correct: `prune_oracle_runs_outside_scope` only reaches here when at least one of the
/// two lists is non-empty, and a row survives if EITHER its commit or its worktree is live.
fn sql_quoted_list(values: &[String]) -> String {
    if values.is_empty() {
        return "''".to_string();
    }
    values
        .iter()
        .map(|value| format!("'{}'", value.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}
