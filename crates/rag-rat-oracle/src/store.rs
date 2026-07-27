//! `oracle_runs` / `edge_oracle` persistence + the edge-candidate and symbol-mapping reads the
//! join needs. SQL helpers named for the domain question, per repo style.
//!
//! INVARIANT (load-bearing): nothing here ever UPDATEs `edges.resolution` / `edges.to_symbol_id`.
//! The heuristic resolution stays on the `edges` row untouched; the oracle's verdict lives only in
//! `edge_oracle`. This is what lets eval diff heuristic-vs-oracle. See `apply_oracle_tables`.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use rag_rat_base::time::now_ms;
use rusqlite::{Connection, params};

use super::{OracleResolutionKind, OracleTool};

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
          AND edges.callee_end_byte IS NOT NULL
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

/// The number of logical symbols enriched with a SCIP moniker for `(tool, tool_version)` — the
/// "symbols enriched" signal (#70). Scoped to `tool_version` (not just `tool`) so the count
/// describes the SAME run as the report's `edge_oracle`-derived metrics: a later same-tool run with
/// a different `tool_version` must not bleed its monikers into this report. Also scoped to the
/// ACTIVE repo: `logical_symbol_monikers` gained its own `repo_id` column in V042 (A5), so the
/// count filters it directly via the periphery scope clause — otherwise a sibling repo's monikers
/// inflate this report's enriched-symbol count.
pub(crate) fn count_symbols_with_moniker(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
) -> anyhow::Result<u64> {
    let repo_clause = oracle_repo_scope_clause(conn, "logical_symbol_monikers")?;
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM logical_symbol_monikers
             WHERE tool = ?1 AND tool_version = ?2{repo_clause}"
        ),
        params![tool.as_db_str(), tool_version],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// The ` AND <qualifier>.repo_id = '…'` predicate for the oracle periphery tables (`oracle_runs` /
/// `edge_oracle` / `logical_symbol_monikers`, all scoped in the A5 migration), or `""` on the
/// pre-A5 schema. Probing `oracle_runs` is authoritative for the set (all three gain `repo_id` in
/// the same migration). See `schema::periphery_repo_scope`.
pub(super) fn oracle_repo_scope_clause(
    conn: &Connection,
    qualifier: &str,
) -> anyhow::Result<String> {
    let scope = rag_rat_db::schema::periphery_repo_scope(conn, "oracle_runs")?;
    Ok(rag_rat_db::schema::periphery_repo_scope_clause(&scope, qualifier))
}

/// The active `repo_id` to STAMP on an oracle-periphery write (`oracle_runs` / `edge_oracle` /
/// `logical_symbol_monikers`), or `None` on the pre-A5 schema. Embedded as a per-call literal in
/// the writers so their bound params (and the ON CONFLICT target column list) are the only things
/// that change with scoping.
fn oracle_repo_scope(conn: &Connection) -> anyhow::Result<Option<String>> {
    Ok(rag_rat_db::schema::periphery_repo_scope(conn, "oracle_runs")?)
}

/// An edge candidate to feed the oracle join: the callee identifier byte range (the SCIP key, #67)
/// plus enough context to write a verdict and to recognise agreement/disagreement.
#[derive(Debug, Clone)]
pub(crate) struct EdgeJoinCandidate {
    /// The live edge rowid. Since #248 the write path keys verdicts by the CONTENT fields below
    /// (not this rowid), so production no longer reads `edge_id` — it is retained as the
    /// candidate's live identity and asserted by `edge_join_candidates` ordering tests.
    /// `allow(dead_code)` because the lib build (no test cfg) sees no reader.
    #[allow(dead_code)]
    pub(crate) edge_id: i64,
    pub(crate) source_path: String,
    pub(crate) file_sha: String,
    /// The call SITE byte range (`edges_data.source_start_byte`/`source_end_byte`). Part of the
    /// content key (#248): the call site disambiguates two callee-range-equal edges (a
    /// `calls_name` and a `references_type` on the same identifier token, or repeated calls).
    /// Both default to 0 on edges that predate source spans, but the resolvable population the
    /// oracle rows carries real spans, so the content key stays unique.
    pub(crate) source_start_byte: i64,
    pub(crate) source_end_byte: i64,
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

/// The edge kinds whose callee byte range covers a CALL-HEAD identifier the clone-refine
/// anti-unify classifier can treat as a callee position (#275): free-fn/method calls
/// (`calls_name`) and macro invocations (`uses_macro` — `opens_call_head` treats a macro head
/// exactly like a call head, and the Rust extractor stamps the callee range on the macro-name
/// identifier). Spelled as a SQL list fragment for the two moniker-collapse reads.
/// `references_type` / `constructs` / `implements` rows also join SCIP occurrences (they carry a
/// callee range) but are never callee positions in the classifier, so they stay excluded.
pub(crate) const CALL_HEAD_EDGE_KINDS_SQL: &str = "('calls_name', 'uses_macro')";

/// The gating tail every clone-refine moniker read appends after its `edge_oracle` anchor match
/// (`source_path` + `file_sha`): rows a collapse may trust are exactly those that are
///
/// 1. **call-HEAD kinds** ([`CALL_HEAD_EDGE_KINDS_SQL`]) — the only positions the classifier can
///    reopen as a callee;
/// 2. **from the LATEST completed run of their tool in the active checkout** — superseded
///    `tool_version` rows are intentionally left behind by [`clear_edge_oracle_for_tool`] (it
///    clears only the current `(tool, tool_version)` scope), and a run in a sibling checkout says
///    nothing about this one. The correlated subquery is the SQL spelling of
///    [`latest_run_tool_version`] (`id DESC` = completion order — see that fn for why `started_at`
///    ordering is wrong); a tool with no run in this checkout matches nothing (`= NULL` is never
///    true), so its rows drop;
/// 3. **backed by a still-live resolved definition** ([`edge_oracle_def_current_predicate`]) — the
///    same def-drift gate the surfacing reads apply: a callsite file can be unchanged while the
///    resolved def was deleted/reindexed, and a moniker for a target the current index no longer
///    contains must not prove two callees identical.
/// 4. **still decorating a LIVE edge** ([`edge_oracle_live_edge_predicate`]) — the content-key join
///    the surfacing path makes an INNER JOIN. A verdict whose content key no longer maps to a live
///    edge (a row surviving a reindex after the extractor stopped emitting that call edge) is a
///    dangling verdict; the surfacing reads exclude it via [`edge_oracle_scope_join`], so the
///    collapse must too. This loses no real collapse: the classifier only ever looks up callee
///    spans that are call-head tokens, which is exactly where the extractor emits an edge.
///
/// `commit_slot`/`worktree_slot` are the caller's bind-parameter names for the active checkout
/// (e.g. `"?3"`, `"?4"`); they feed the runs subquery, the def-drift EXISTS, and the live-edge
/// EXISTS. Shared by [`current_callee_monikers`] and the refine-mode probe
/// (`oracle_callee_coverage_exists`) so the probe and the fetch express ONE currency discipline —
/// a probe looser than the fetch would key baseline-identical refinements into the scip cache
/// namespace (needless oracle-run churn).
pub fn callee_moniker_current_clause(
    conn: &Connection,
    commit_slot: &str,
    worktree_slot: &str,
) -> anyhow::Result<String> {
    let runs_repo_clause = oracle_repo_scope_clause(conn, "oracle_runs")?;
    Ok(format!(
        " AND edge_oracle.edge_kind IN {CALL_HEAD_EDGE_KINDS_SQL} AND edge_oracle.tool_version = \
         (SELECT oracle_runs.tool_version FROM oracle_runs WHERE oracle_runs.tool = \
         edge_oracle.tool AND oracle_runs.commit_sha = {commit_slot} AND oracle_runs.worktree_id \
         = {worktree_slot}{runs_repo_clause} ORDER BY oracle_runs.id DESC LIMIT \
         1){def_current}{live_edge}",
        def_current = edge_oracle_def_current_predicate(commit_slot, worktree_slot),
        live_edge = edge_oracle_live_edge_predicate(commit_slot, worktree_slot),
    ))
}

/// The CURRENT SCIP callee resolutions for one file, keyed by the callee-identifier byte range —
/// the clone-refine moniker-collapse input (#275, Plan 3). `file_sha` must be the sha256 of the
/// EXACT bytes the caller's byte offsets were derived from: `edge_oracle` spans were computed
/// against the content hashed into each row's `file_sha`, so filtering on it is what makes the
/// span join sound (a drifted file simply matches nothing — the conservative no-collapse
/// fallback). `commit_sha`/`worktree_id` scope the currency gate
/// ([`callee_moniker_current_clause`]): only call-HEAD rows the LATEST run of each tool in the
/// active checkout stands behind, whose resolved definition still exists, are returned. Uses
/// `idx_edge_oracle_anchor` (source_path prefix).
///
/// Two further exclusions keep a false collapse impossible:
/// - `local N` monikers are DOCUMENT-scoped (unique only within one file), so cross-file equality
///   would identify two different functions — never returned.
/// - a span where two rows (different tools / tool versions) disagree on the moniker has no
///   trustworthy identity — dropped entirely.
pub fn current_callee_monikers(
    conn: &Connection,
    source_path: &str,
    file_sha: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<HashMap<(usize, usize), String>> {
    let repo_clause = oracle_repo_scope_clause(conn, "edge_oracle")?;
    let current_clause = callee_moniker_current_clause(conn, "?3", "?4")?;
    let mut stmt = conn.prepare(&format!(
        "SELECT callee_start_byte, callee_end_byte, scip_symbol
         FROM edge_oracle
         WHERE source_path = ?1 AND file_sha = ?2{repo_clause}{current_clause}"
    ))?;
    let rows = stmt.query_map(params![source_path, file_sha, commit_sha, worktree_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?))
    })?;
    let mut monikers: HashMap<(usize, usize), String> = HashMap::new();
    let mut conflicted: HashSet<(usize, usize)> = HashSet::new();
    for row in rows {
        let (start, end, symbol) = row?;
        let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
            continue;
        };
        if super::scip::is_local_symbol(&symbol) {
            continue;
        }
        let span = (start, end);
        if conflicted.contains(&span) {
            continue;
        }
        match monikers.entry(span) {
            Entry::Vacant(slot) => {
                slot.insert(symbol);
            },
            Entry::Occupied(existing) if *existing.get() == symbol => {},
            Entry::Occupied(existing) => {
                existing.remove();
                conflicted.insert(span);
            },
        }
    }
    Ok(monikers)
}

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
               edges.source_start_byte,
               edges.source_end_byte,
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
            source_start_byte: row.get(3)?,
            source_end_byte: row.get(4)?,
            callee_start_byte: row.get(5)?,
            callee_end_byte: row.get(6)?,
            confidence: row.get(7)?,
            edge_kind: row.get(8)?,
            to_symbol_id: row.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The `files.sha256` of ONE indexed path in the active checkout, or `None` when the path isn't
/// indexed here (tombstones excluded, as in [`indexed_file_shas_in_scope`]). The live oracle's
/// definition-side drift probe (#534): the LSP returns a bounded set of definition paths per
/// pass, so materializing the whole-checkout map would be O(repo files) per pass under the write
/// lock for no benefit.
pub(crate) fn indexed_file_sha_for_path(
    conn: &Connection,
    path: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Option<String>> {
    use rusqlite::OptionalExtension as _;
    conn.query_row(
        &format!(
            "SELECT sha256 FROM files WHERE path = ?1 AND kind != 'deleted' AND {scope}",
            scope = active_checkout_file_predicate("?2", "?3"),
        ),
        params![path, commit_sha, worktree_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

/// An `edge_oracle` row's full content key: `(source_start_byte, source_end_byte,
/// callee_start_byte, callee_end_byte, edge_kind)` — the identity the live pass's covered-skip
/// tracks (never the callee start alone; two edges can share one token).
pub(crate) type LiveEdgeKey = (i64, i64, i64, i64, String);

/// The full content keys of a file's edges already covered by a CURRENT live verdict for
/// `(tool, tool_version)` — current meaning the row's `file_sha` matches the file's indexed
/// content NOW (the content-addressed currency the live pass keys on, #534). The budget
/// continuation mechanism: a file a prior pass truncated resumes where it stopped, because
/// already-verdicted callees are skipped by the live pass. The key is the FULL edge identity
/// (source span + callee span + edge kind), never the callee start alone: two edges can share
/// one token (`calls_name` + `references_type`), and a start-byte key would let the first
/// written row mark BOTH covered and starve the other forever.
///
/// A verdict whose resolved DEFINITION no longer exists in the active checkout (the def file was
/// edited + reindexed between passes while THIS file's bytes held) is NOT counted covered — it
/// applies the same definition-current predicate the surfacing reads use, so continuation
/// re-resolves the edge instead of skipping it forever behind evidence the read path rejects.
pub(crate) fn live_covered_edges_for_path(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    source_path: &str,
    file_sha: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<std::collections::HashSet<LiveEdgeKey>> {
    let repo_clause = oracle_repo_scope_clause(conn, "edge_oracle")?;
    let def_current = edge_oracle_def_current_predicate("?5", "?6");
    let mut stmt = conn.prepare(&format!(
        "SELECT source_start_byte, source_end_byte, callee_start_byte, callee_end_byte, edge_kind
         FROM edge_oracle
         WHERE tool = ?1 AND tool_version = ?2 AND source_path = ?3 AND file_sha = \
         ?4{repo_clause}{def_current}"
    ))?;
    let rows = stmt.query_map(
        params![tool.as_db_str(), tool_version, source_path, file_sha, commit_sha, worktree_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        },
    )?;
    let mut out = std::collections::HashSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}

/// Migrate every `edge_oracle` row of `tool` from one `tool_version` to another, returning the
/// moved row count. The live oracle's version-transition path (#534): a respawn probing a NEW
/// `rust-analyzer --version` would otherwise make the first partial pass's run row the latest
/// for the whole checkout and gate every prior-version verdict out of currency — collapsing live
/// coverage to the handful of files the new session revisited. The rows are content-addressed
/// (`file_sha` still gates drift), so moving them under the new version preserves coverage; a
/// row the new version already wrote (same content key) is dropped first to keep the PK.
///
/// SCOPE (load-bearing): restricted to rows whose LIVE EDGE belongs to the active
/// `(commit_sha, worktree_id)` checkout — the SAME content join [`clear_edge_oracle_for_tool`]
/// uses. `edge_oracle` rows carry no commit/worktree columns (their scope is the live-edge
/// join), so a repo-wide UPDATE would relabel a SIBLING worktree's rows too while only THIS
/// checkout gets the new-version run row — and the sibling's currency gate, still selecting the
/// old version, would then hide all of its own verdicts.
pub(crate) fn migrate_live_verdicts_to_version(
    conn: &Connection,
    tool: OracleTool,
    from_version: &str,
    to_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<u64> {
    let repo_clause = oracle_repo_scope_clause(conn, "edge_oracle")?;
    let scope = active_checkout_file_predicate("?4", "?5");
    // The active-checkout, current-CONTENT live-edge predicate: the row's `file_sha` must equal
    // the checkout's CURRENT `files.sha256`, so a sibling worktree's row (same path + spans, but
    // its own content sha) is NOT treated as this checkout's — without the sha correlation a
    // repo-wide relabel would move the sibling's row while only this checkout records the
    // new-version run, hiding the sibling's verdicts behind its still-old currency gate.
    let active_current = format!(
        "EXISTS (
             SELECT 1
             FROM edges_data
             JOIN files ON files.id = edges_data.source_file_id
             JOIN name_strings ek ON ek.id = edges_data.edge_kind_id
             WHERE files.path = edge_oracle.source_path
               AND files.sha256 = edge_oracle.file_sha
               AND edges_data.source_start_byte = edge_oracle.source_start_byte
               AND edges_data.source_end_byte = edge_oracle.source_end_byte
               AND edges_data.callee_start_byte = edge_oracle.callee_start_byte
               AND edges_data.callee_end_byte = edge_oracle.callee_end_byte
               AND ek.value = edge_oracle.edge_kind
               AND edges_data.hidden = 0
               AND {scope})"
    );
    // Drop old-version rows whose content key the new version already occupies (the PK is
    // (repo_id?, tool, tool_version, source_path, spans…, edge_kind) — file_sha is NOT in it, so
    // the UPDATE below would otherwise hit a PK collision), restricted to rows current in THIS
    // checkout.
    conn.execute(
        &format!(
            "DELETE FROM edge_oracle
             WHERE tool = ?1 AND tool_version = ?2{repo_clause}
               AND (source_path, source_start_byte, source_end_byte,
                    callee_start_byte, callee_end_byte, edge_kind) IN (
                    SELECT source_path, source_start_byte, source_end_byte,
                           callee_start_byte, callee_end_byte, edge_kind
                    FROM edge_oracle
                    WHERE tool = ?1 AND tool_version = ?3{repo_clause})
               AND {active_current}"
        ),
        params![tool.as_db_str(), from_version, to_version, commit_sha, worktree_id],
    )?;
    let moved = conn.execute(
        &format!(
            "UPDATE edge_oracle SET tool_version = ?3
             WHERE tool = ?1 AND tool_version = ?2{repo_clause}
               AND {active_current}"
        ),
        params![tool.as_db_str(), from_version, to_version, commit_sha, worktree_id],
    )?;
    Ok(moved as u64)
}

/// [`edge_join_candidates`] restricted to a set of source paths — the live oracle's per-pass
/// worklist (#534): only the files the maintenance pass just reindexed. Same scope + ordering
/// discipline as the whole-checkout variant. Paths are queried in bounded chunks (one `IN` list
/// per chunk), so an accumulated backlog larger than SQLite's bound-variable limit can't fail
/// the prepare and wedge the backlog forever.
pub(crate) fn edge_join_candidates_for_paths(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
    paths: &[String],
) -> anyhow::Result<Vec<EdgeJoinCandidate>> {
    const PATH_CHUNK: usize = 500;
    let mut out = Vec::new();
    for chunk in paths.chunks(PATH_CHUNK) {
        out.extend(edge_join_candidates_in_paths(conn, commit_sha, worktree_id, chunk)?);
    }
    // Chunks concatenate in worklist order; the per-chunk ORDER BY keeps candidates grouped by
    // path, which is all the live pass's per-file grouping requires.
    Ok(out)
}

fn edge_join_candidates_in_paths(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
    paths: &[String],
) -> anyhow::Result<Vec<EdgeJoinCandidate>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    // Numbered params, NOT anonymous `?`: the scope predicate binds ?1/?2 (and an anonymous
    // parameter would take slot 1, colliding with them), so the path list numbers from ?3.
    let marks = (3..3 + paths.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
    let sql = format!(
        "
        SELECT edges.id,
               files.path,
               files.sha256,
               edges.source_start_byte,
               edges.source_end_byte,
               edges.callee_start_byte,
               edges.callee_end_byte,
               edges.confidence,
               edges.edge_kind,
               edges.to_symbol_id
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edges.callee_start_byte IS NOT NULL
          AND edges.callee_end_byte IS NOT NULL
          AND files.path IN ({marks})
          AND {scope}
        ORDER BY files.path, edges.callee_start_byte
        ",
        scope = active_checkout_file_predicate("?1", "?2"),
    );
    let mut stmt = conn.prepare(&sql)?;
    // ?1/?2 are the checkout scope; the path list binds from ?3.
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&commit_sha, &worktree_id];
    for path in paths {
        params.push(path);
    }
    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok(EdgeJoinCandidate {
            edge_id: row.get(0)?,
            source_path: row.get(1)?,
            file_sha: row.get(2)?,
            source_start_byte: row.get(3)?,
            source_end_byte: row.get(4)?,
            callee_start_byte: row.get(5)?,
            callee_end_byte: row.get(6)?,
            confidence: row.get(7)?,
            edge_kind: row.get(8)?,
            to_symbol_id: row.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The batch tool's persisted moniker for the logical symbol `symbol_id` belongs to — the string
/// the LIVE writer copies verbatim into its `edge_oracle.scip_symbol` (#534). Interchangeability
/// is literal string equality: the batch moniker is byte-identical to what the batch pass's own
/// verdicts carry (for rust-analyzer `stabilize_moniker_version` is the identity), so
/// clone-collapse + moniker-anchored memory relocation treat live and batch rows as one evidence
/// set, and `current_callee_monikers`' cross-tool conflict-drop never fires on a co-covered
/// span. `None` when the batch pass has no moniker for the symbol (never run, or the def maps
/// outside its definitions) — the caller then falls back to a SCIP-local sentinel. Live only
/// READS this table; only the batch pass writes it.
pub(crate) fn batch_moniker_for_symbol(
    conn: &Connection,
    symbol_id: i64,
    batch_tool: OracleTool,
) -> anyhow::Result<Option<String>> {
    use rusqlite::OptionalExtension as _;
    // The qualifier is the `m` alias this query uses, so the repo clause names the alias, not the
    // table.
    let repo_clause = oracle_repo_scope_clause(conn, "m")?;
    conn.query_row(
        &format!(
            "
            SELECT m.moniker
            FROM logical_symbol_monikers m
            JOIN logical_symbol_members mem
              ON mem.logical_symbol_id = m.logical_symbol_id
            WHERE mem.symbol_id = ?1 AND m.tool = ?2{repo_clause}
            LIMIT 1
            "
        ),
        params![symbol_id, batch_tool.as_db_str()],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

/// The persisted `(file_sha, scip_symbol)` of the `edge_oracle` row at `row`'s content key for
/// `(tool, tool_version)`, if any. The live writer reads this before its upsert (#534): an
/// existing row whose `scip_symbol` CHANGES while its `file_sha` stays constant means the
/// oracle evidence a scip-mode clone refinement consulted moved under an unchanged refinement
/// key, so the refine cache must be invalidated (mirroring the batch run's hook). A same-value
/// upsert (or a `file_sha` change, which already re-keys everything downstream) skips it.
pub(crate) fn existing_verdict_scip_symbol(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    row: &EdgeOracleRow<'_>,
) -> anyhow::Result<Option<(String, String)>> {
    use rusqlite::OptionalExtension as _;
    let repo_clause = oracle_repo_scope_clause(conn, "edge_oracle")?;
    conn.query_row(
        &format!(
            "
            SELECT file_sha, scip_symbol
            FROM edge_oracle
            WHERE tool = ?1 AND tool_version = ?2
              AND source_path = ?3
              AND source_start_byte = ?4 AND source_end_byte = ?5
              AND callee_start_byte = ?6 AND callee_end_byte = ?7
              AND edge_kind = ?8{repo_clause}
            "
        ),
        params![
            tool.as_db_str(),
            tool_version,
            row.source_path,
            row.source_start_byte,
            row.source_end_byte,
            row.callee_start_byte,
            row.callee_end_byte,
            row.edge_kind,
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .map_err(Into::into)
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

/// Delete this repo's `logical_symbol_monikers` rows for `tool`. Called at the start of a run,
/// inside the run's transaction, so the moniker set is **authoritative** for the latest run: a
/// symbol the current `.scip` no longer defines must not keep a stale moniker.
///
/// SCOPE (load-bearing): the DELETE is restricted to the ACTIVE repo's monikers via
/// `logical_symbol_monikers.repo_id` — its own column since V042 (A5), stamped on write to the
/// repo that owns the moniker. A global `DELETE ... WHERE tool = ?` would erase a SIBLING repo's
/// live moniker anchors on every oracle run, making its `scip_moniker` memory relocation go
/// gone/unverified. This repo's dangling rows (whose content-derived logical id died in a rebuild
/// but whose `repo_id` still marks them ours) are swept alongside; a sibling's dangling rows are
/// left for that repo's own run to clear (they resolve through no live join, so they never leak).
pub(crate) fn clear_logical_symbol_monikers_for_tool(
    conn: &Connection,
    tool: OracleTool,
) -> anyhow::Result<()> {
    let repo_clause = oracle_repo_scope_clause(conn, "logical_symbol_monikers")?;
    conn.execute(&format!("DELETE FROM logical_symbol_monikers WHERE tool = ?1{repo_clause}"), [
        tool.as_db_str(),
    ])?;
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
    // Post-A5 the PK is `(repo_id, logical_symbol_id, tool)`, so stamp `repo_id` AND widen the ON
    // CONFLICT target. The repo id is a per-call literal prefix so the bound params (`?1`..`?5`)
    // are unchanged; pre-A5 uses the original 5-column shape.
    let (repo_col, repo_val, conflict_prefix) = match oracle_repo_scope(conn)? {
        Some(repo_id) => (
            "repo_id, ".to_string(),
            format!("'{}', ", repo_id.replace('\'', "''")),
            "repo_id, ".to_string(),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    conn.execute(
        &format!(
            "
        INSERT INTO logical_symbol_monikers({repo_col}logical_symbol_id, tool, tool_version, \
             moniker, computed_at)
        VALUES ({repo_val}?1, ?2, ?3, ?4, ?5)
        ON CONFLICT({conflict_prefix}logical_symbol_id, tool) DO UPDATE SET
            tool_version = excluded.tool_version,
            moniker = excluded.moniker,
            computed_at = excluded.computed_at
        "
        ),
        params![logical_symbol_id, tool.as_db_str(), tool_version, moniker, now_ms()],
    )?;
    Ok(())
}

/// Delete the `external_symbols` contracts for a `(tool, checkout)` scope. Called at the start of a
/// run so the pass is AUTHORITATIVE for its tool in this checkout: a moniker the prior `.scip`
/// described but the current one no longer emits must not keep a stale contract (upsert alone only
/// overwrites monikers the current run revisits). Run inside the same transaction that writes the
/// current rows so the table is never observed mid-clear.
///
/// SCOPE (load-bearing): restricted to the ACTIVE `(commit_sha, worktree_id)` checkout AND repo
/// (`external_symbols.repo_id`, its own column from birth, V056) — the SAME per-checkout, per-repo
/// scope `oracle_runs` uses, and the reason external_symbols carries those columns. Without the
/// checkout predicate a run in one linked worktree would erase a SIBLING checkout's contracts (they
/// share `(repo_id, tool)` but resolve different dependency versions); without the repo predicate a
/// sibling repo's contracts would go. The clear is per-tool (not per-tool_version) because the
/// moniker's own version component already separates dependency versions within a checkout.
pub(crate) fn clear_external_symbols_for_tool(
    conn: &Connection,
    tool: OracleTool,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<()> {
    let repo_clause = oracle_repo_scope_clause(conn, "external_symbols")?;
    conn.execute(
        &format!(
            "DELETE FROM external_symbols WHERE tool = ?1 AND commit_sha = ?2 AND worktree_id = \
             ?3{repo_clause}"
        ),
        params![tool.as_db_str(), commit_sha, worktree_id],
    )?;
    Ok(())
}

/// One external dependency contract to persist: the `SymbolInformation` parsed from
/// `index.external_symbols`, keyed by its RAW moniker (== `edge_oracle.scip_symbol`).
pub(crate) struct ExternalSymbolRow<'a> {
    pub(crate) moniker: &'a str,
    pub(crate) kind: &'a str,
    pub(crate) display_name: &'a str,
    pub(crate) signature_text: &'a str,
    pub(crate) signature_language: &'a str,
    pub(crate) documentation: &'a str,
    pub(crate) deprecated: bool,
}

/// Upsert one `external_symbols` row. Keyed by `(repo_id, tool, commit_sha, worktree_id, moniker)`
/// — re-running the same tool in the SAME checkout overwrites the prior contract, while a sibling
/// checkout's rows are untouched. `external_symbols` is born post-A5 (V056), so `repo_id` is ALWAYS
/// present and leads the PK; it is stamped unconditionally here (unlike the moniker / edge writers,
/// which straddle the pre-/post-A5 table shapes).
pub(crate) fn write_external_symbol(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    row: &ExternalSymbolRow<'_>,
) -> anyhow::Result<()> {
    let repo_id = oracle_repo_scope(conn)?
        .ok_or_else(|| anyhow::anyhow!("external_symbols requires post-A5 repo scoping"))?;
    conn.execute(
        "
        INSERT INTO external_symbols(
            repo_id, tool, tool_version, commit_sha, worktree_id, moniker, kind, display_name,
            signature_text, signature_language, documentation, deprecated, computed_at_ms
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        ON CONFLICT(repo_id, tool, commit_sha, worktree_id, moniker) DO UPDATE SET
            tool_version = excluded.tool_version,
            kind = excluded.kind,
            display_name = excluded.display_name,
            signature_text = excluded.signature_text,
            signature_language = excluded.signature_language,
            documentation = excluded.documentation,
            deprecated = excluded.deprecated,
            computed_at_ms = excluded.computed_at_ms
        ",
        params![
            repo_id,
            tool.as_db_str(),
            tool_version,
            commit_sha,
            worktree_id,
            row.moniker,
            row.kind,
            row.display_name,
            row.signature_text,
            row.signature_language,
            row.documentation,
            row.deprecated,
            now_ms(),
        ],
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
    // CONTENT-JOIN scope (#248): with no `edge_id` FK there is no rowid to match — restrict the
    // DELETE to verdicts whose CONTENT key has a live edge in the active checkout, the same key the
    // read join uses. `edges_data` is queried directly (not the 7-LEFT-JOIN `edges` view) with ONE
    // `name_strings` join for the `edge_kind` text match. The callsite-file `files.sha256` is NOT
    // gated here: the authoritative clear must drop the prior verdict for a content-matching edge
    // even when the file drifted (the run rewrites it), exactly as the old rowid clear did.
    //
    // SCOPE (load-bearing, A5): the DELETE also filters `edge_oracle.repo_id` (its own column since
    // V042). Without it, a SIBLING repo's verdict whose `source_path` + content key collide with
    // one of this repo's live edges (an identical shared file, the same-path poison tripwire)
    // would be cross-cleared by this repo's run — the same content join
    // `edge_oracle_scope_join` guards on the read side. `{repo_clause}` empty pre-A5.
    let repo_clause = oracle_repo_scope_clause(conn, "edge_oracle")?;
    conn.execute(
        &format!(
            "
        DELETE FROM edge_oracle
        WHERE tool = ?1 AND tool_version = ?2{repo_clause}
          AND EXISTS (
            SELECT 1
            FROM edges_data
            JOIN files ON files.id = edges_data.source_file_id
            JOIN name_strings ek ON ek.id = edges_data.edge_kind_id
            WHERE files.path = edge_oracle.source_path
              AND edges_data.source_start_byte = edge_oracle.source_start_byte
              AND edges_data.source_end_byte = edge_oracle.source_end_byte
              AND edges_data.callee_start_byte = edge_oracle.callee_start_byte
              AND edges_data.callee_end_byte = edge_oracle.callee_end_byte
              AND ek.value = edge_oracle.edge_kind
              -- Materialized visibility (#734): a suppressed candidate is not a live oracle
              -- edge, so its content twin must not anchor a verdict here.
              AND edges_data.hidden = 0
              AND {scope}
          )
        ",
            scope = active_checkout_file_predicate("?3", "?4"),
        ),
        params![tool.as_db_str(), tool_version, commit_sha, worktree_id],
    )?;
    Ok(())
}

/// A verdict to persist for one edge, keyed by the edge's CONTENT identity (#248) rather than the
/// volatile `edges_data.id` rowid — so the verdict survives reindex and re-anchors to the
/// reindexed edge for free. The content key is `(tool, tool_version, source_path,
/// source_start_byte, source_end_byte, callee_start_byte, callee_end_byte, edge_kind)`; `edge_kind`
/// is stored as TEXT (the candidate already carries the resolved text via the `edges` view's
/// `name_strings` join), NOT the interned id (which is not reindex-stable).
pub(crate) struct EdgeOracleRow<'a> {
    pub(crate) source_path: &'a str,
    pub(crate) source_start_byte: i64,
    pub(crate) source_end_byte: i64,
    pub(crate) callee_start_byte: i64,
    pub(crate) callee_end_byte: i64,
    pub(crate) edge_kind: &'a str,
    pub(crate) file_sha: &'a str,
    pub(crate) resolved_symbol_id: Option<i64>,
    pub(crate) scip_symbol: &'a str,
    pub(crate) kind: OracleResolutionKind,
}

/// Upsert one `edge_oracle` row. Keyed by the content key
/// `(tool, tool_version, source_path, source_start_byte, source_end_byte, callee_start_byte,
/// callee_end_byte, edge_kind)`; re-running the same tool version overwrites the prior verdict
/// (staleness still content-addressed by `file_sha`). NEVER touches `edges`.
pub(crate) fn write_edge_oracle(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    row: &EdgeOracleRow<'_>,
) -> anyhow::Result<()> {
    // Post-A5 the content-key PK gains a leading `repo_id`, so stamp it AND widen the ON CONFLICT
    // target. The repo id is a per-call literal prefix so the bound params (`?1`..`?13`) are
    // unchanged; pre-A5 uses the original shape.
    let (repo_col, repo_val, conflict_prefix) = match oracle_repo_scope(conn)? {
        Some(repo_id) => (
            "repo_id, ".to_string(),
            format!("'{}', ", repo_id.replace('\'', "''")),
            "repo_id, ".to_string(),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    conn.execute(
        &format!(
            "
        INSERT INTO edge_oracle(
            {repo_col}source_path, source_start_byte, source_end_byte,
            callee_start_byte, callee_end_byte, edge_kind,
            file_sha, tool, tool_version,
            resolved_symbol_id, scip_symbol, kind, computed_at
        )
        VALUES ({repo_val}?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        ON CONFLICT(
            {conflict_prefix}tool, tool_version, source_path,
            source_start_byte, source_end_byte,
            callee_start_byte, callee_end_byte, edge_kind
        ) DO UPDATE SET
            file_sha = excluded.file_sha,
            resolved_symbol_id = excluded.resolved_symbol_id,
            scip_symbol = excluded.scip_symbol,
            kind = excluded.kind,
            computed_at = excluded.computed_at
        "
        ),
        params![
            row.source_path,
            row.source_start_byte,
            row.source_end_byte,
            row.callee_start_byte,
            row.callee_end_byte,
            row.edge_kind,
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
    // Post-A5 `oracle_runs` carries `repo_id` (the read key is (repo_id, tool, tool_version,
    // commit_sha, worktree_id)); stamp it as a per-call literal prefix so the bound params are
    // unchanged. Pre-A5 uses the original 7-column shape.
    let (repo_col, repo_val) = match oracle_repo_scope(conn)? {
        Some(repo_id) => ("repo_id, ".to_string(), format!("'{}', ", repo_id.replace('\'', "''"))),
        None => (String::new(), String::new()),
    };
    conn.execute(
        &format!(
            "
        INSERT INTO oracle_runs(
            {repo_col}tool, tool_version, commit_sha, worktree_id, started_at, status, stats_json
        )
        VALUES ({repo_val}?1, ?2, ?3, ?4, ?5, ?6, ?7)
        "
        ),
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
    let repo_clause = oracle_repo_scope_clause(conn, "oracle_runs")?;
    let version = conn
        .query_row(
            &format!(
                "
            SELECT tool_version FROM oracle_runs
            WHERE tool = ?1 AND commit_sha = ?2 AND worktree_id = ?3{repo_clause}
            ORDER BY id DESC
            LIMIT 1
            "
            ),
            params![tool.as_db_str(), commit_sha, worktree_id],
            |row| row.get::<_, String>(0),
        )
        .ok();
    Ok(version)
}

/// The `started_at` (Unix-epoch ms) of the LAST-COMPLETED run for `tool` **in the active
/// checkout**, or `None` when no run exists. The staleness clock the background auto-fresh oracle
/// compares against the index's `indexed_at_ms` (see [`crate::auto_run_decision`]):
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
    let repo_clause = oracle_repo_scope_clause(conn, "oracle_runs")?;
    let started_at = conn
        .query_row(
            &format!(
                "
            SELECT started_at FROM oracle_runs
            WHERE tool = ?1 AND commit_sha = ?2 AND worktree_id = ?3{repo_clause}
            ORDER BY id DESC
            LIMIT 1
            "
            ),
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
    let repo_clause = oracle_repo_scope_clause(conn, "oracle_runs")?;
    let exists = conn
        .query_row(
            &format!(
                "SELECT 1 FROM oracle_runs WHERE commit_sha = ?1 AND worktree_id = \
                 ?2{repo_clause} LIMIT 1"
            ),
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
///
/// CONTENT JOIN (load-bearing, #248): the join to live `edges` is by the edge's CONTENT key
/// (`source_path → files.path`, source/callee byte spans, `edge_kind` text), NOT by the old
/// `edges.id = edge_oracle.edge_id` rowid — there is no `edge_id` column any more. After a reindex
/// rewrites `edges_data` with new ids, an UNCHANGED file's verdict re-anchors to the reindexed edge
/// for free (its content key + `files.sha256` are unchanged); a dangling verdict (no content match)
/// simply produces no row, so the COUNT paths exclude it WITHOUT the FK pre-delete that V018 used.
/// `files.sha256 = edge_oracle.file_sha` is part of the join so a CHANGED file's verdict (sha
/// mismatch) never matches — the metric reads see only live, current-content verdicts.
///
/// ALIASING (R7): bare table names (`edges`/`files`) are kept — no `e`/`f` aliases — so the
/// appended [`edge_oracle_current_predicate`] (bare `files.sha256` + its inner `JOIN files`) and
/// the consumer SELECTs (`edges.confidence`/`edge_kind`/`to_symbol_id`/`to_name`/`id`,
/// `files.path`) still resolve. The `edges` VIEW (not `edges_data`) is used here because consumers
/// need its `name_strings`-joined text columns (`edge_kind`, `confidence`, `to_name`); the
/// surfacing reads are bound by an `edge_id IN (...)` list or the staleness/anchor indexes, so the
/// view's joins are not on an unbounded hot scan.
///
/// SCOPE (load-bearing, A5): the join to `files` is by the edge's CONTENT key (`source_path`,
/// `file_sha`) — so in a consolidated DB a SIBLING repo's `edge_oracle` verdict whose `source_path`
/// and `file_sha` collide with one of THIS repo's files (an identical vendored/shared file, the
/// same-path poison tripwire) would join onto the active repo's `files` and surface/count as ours.
/// The `edge_oracle.repo_id` predicate (its own column since V042) pins the verdict to the active
/// repo directly, so every read consumer that routes through this helper inherits the isolation.
/// `{repo_clause}` is empty pre-A5. Appended last so a caller's trailing ` AND <extra>` still
/// composes.
pub(crate) fn edge_oracle_scope_join(conn: &Connection) -> anyhow::Result<String> {
    let repo_clause = oracle_repo_scope_clause(conn, "edge_oracle")?;
    Ok(format!(
        "
    FROM edge_oracle
    JOIN files ON files.path = edge_oracle.source_path
              AND files.sha256 = edge_oracle.file_sha
    JOIN edges ON edges.source_file_id = files.id
              AND edges.source_start_byte = edge_oracle.source_start_byte
              AND edges.source_end_byte = edge_oracle.source_end_byte
              AND edges.callee_start_byte = edge_oracle.callee_start_byte
              AND edges.callee_end_byte = edge_oracle.callee_end_byte
              AND edges.edge_kind = edge_oracle.edge_kind
    WHERE edge_oracle.tool = ?1 AND edge_oracle.tool_version = ?2
      AND {scope}{repo_clause}",
        scope = active_checkout_file_predicate("?3", "?4"),
    ))
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
    let scope_join = edge_oracle_scope_join(conn)?;
    // COUNT(DISTINCT edge_oracle.rowid), not COUNT(*): the scope join joins each `edge_oracle` row
    // to its LIVE edge by the content key. That key is measured 1:1 with a live edge (0
    // collisions / 1.03M rows — the call-site source span makes it unique), so today this
    // equals COUNT(*). The DISTINCT is defense-in-depth (#248): if a future extract/resolve
    // span change ever made one content key fan onto MULTIPLE live edges, COUNT(*) would
    // inflate the verdict total by counting the row once per matched edge — DISTINCT on the
    // physical verdict's rowid pins the count to the number of persisted verdicts regardless.
    // (`edge_oracle` is STRICT, not WITHOUT ROWID, so it has an implicit rowid.) Pairs with
    // `edge_oracle_same_full_content_key_upserts_one_row`.
    let count: i64 = match kind {
        Some(kind) => conn.query_row(
            &format!(
                "SELECT COUNT(DISTINCT edge_oracle.rowid){scope_join} AND edge_oracle.kind = ?5"
            ),
            params![tool.as_db_str(), tool_version, commit_sha, worktree_id, kind.as_db_str()],
            |row| row.get(0),
        )?,
        None => conn.query_row(
            &format!("SELECT COUNT(DISTINCT edge_oracle.rowid){scope_join}"),
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
        " AND edge_oracle.file_sha = files.sha256{def_current}",
        def_current = edge_oracle_def_current_predicate("?3", "?4"),
    )
}

/// Gate (2) of [`edge_oracle_current_predicate`] on its own — the def-drift EXISTS, parameterized
/// on the caller's bind-slot names for `(commit_sha, worktree_id)` so reads that don't use the
/// `?1..?4` scope-join slot convention ([`callee_moniker_current_clause`]) still apply the ONE
/// spelling of the gate rather than re-deriving it. The inner `symbols`/`files` names shadow any
/// outer join of the same tables (SQLite resolves the subquery scope first), exactly as when
/// appended after [`edge_oracle_scope_join`].
pub(crate) fn edge_oracle_def_current_predicate(commit_slot: &str, worktree_slot: &str) -> String {
    format!(
        " AND (edge_oracle.resolved_symbol_id IS NULL OR EXISTS (SELECT 1 FROM symbols JOIN files \
         ON files.id = symbols.file_id WHERE symbols.id = edge_oracle.resolved_symbol_id AND \
         {scope}))",
        scope = active_checkout_file_predicate(commit_slot, worktree_slot),
    )
}

/// The EXISTS-form mirror of [`edge_oracle_scope_join`]'s content-key INNER JOIN: whether the
/// correlated `edge_oracle` row still decorates a LIVE `edges` row in the active
/// `(commit_sha, worktree_id)` checkout — the SAME six content-key columns (`source_path` →
/// `files.path`, `file_sha` → `files.sha256`, the source + callee byte spans, and `edge_kind`)
/// plus the active-checkout file predicate. The surfacing reads express this as an INNER JOIN
/// because they DECORATE edges (they need a live edge to attach the `Compiler` tier to); the
/// clone-refine moniker reads are `FROM edge_oracle` with no join and span multiple tools (to
/// detect cross-tool conflicts), so they can't take the single-tool JOIN form and append this
/// ` AND EXISTS(...)` instead. Same isolation model as `edge_oracle_scope_join`: repo isolation
/// comes from the outer `edge_oracle.repo_id` predicate, so this EXISTS is intentionally not
/// repo-scoped (a cross-repo path+sha+commit collision is identical content, benign for an
/// existence check). Uses the `edges` VIEW (like the JOIN) so `edge_kind` resolves as text. The
/// inner `files`/`edges` names are a fresh scope, shadowing any outer join of the same tables.
pub(crate) fn edge_oracle_live_edge_predicate(commit_slot: &str, worktree_slot: &str) -> String {
    format!(
        " AND EXISTS (SELECT 1 FROM files JOIN edges ON edges.source_file_id = files.id AND \
         edges.source_start_byte = edge_oracle.source_start_byte AND edges.source_end_byte = \
         edge_oracle.source_end_byte AND edges.callee_start_byte = edge_oracle.callee_start_byte \
         AND edges.callee_end_byte = edge_oracle.callee_end_byte AND edges.edge_kind = \
         edge_oracle.edge_kind WHERE files.path = edge_oracle.source_path AND files.sha256 = \
         edge_oracle.file_sha AND {scope})",
        scope = active_checkout_file_predicate(commit_slot, worktree_slot),
    )
}

/// One current, in-scope oracle verdict for an edge, surfaced in graph/impact query output as the
/// `Compiler` tier. `package` is the external dependency name for `resolved-external` verdicts
/// (`scip_symbol`'s package component), `None` for in-corpus resolutions.
#[derive(Debug, Clone)]
pub struct EdgeOracleVerdict {
    pub kind: OracleResolutionKind,
    /// The qualified name of the verdict's in-corpus `resolved_symbol_id` (joined at read time),
    /// `None` when the verdict is external (`resolved_symbol_id IS NULL`) or the resolved symbol
    /// no longer exists. An `Upgrade`'s target is hydrated from THIS name — never the
    /// heuristic edge's stale/heuristic target — and an `Upgrade` whose resolved symbol can't
    /// be surfaced (`None`) is NOT promoted to `compiler`: we won't attach the tier to a
    /// target we can't name (#82 finding 2). The def-drift gate in
    /// [`edge_oracle_current_predicate`] already filters a deleted/reinserted resolved symbol,
    /// so in practice this is `None` only for externals.
    pub resolved_qualified_name: Option<String>,
    pub(crate) scip_symbol: String,
    pub(crate) tool: OracleTool,
    pub(crate) tool_version: String,
}

impl EdgeOracleVerdict {
    /// The provenance string surfaced as `resolution_reason` when this verdict upgrades an edge to
    /// the `Compiler` tier: `scip:<tool>@<version>` (the #61 design's reason format).
    pub fn resolution_reason(&self) -> String {
        format!("scip:{}@{}", self.tool.as_db_str(), self.tool_version)
    }

    /// `resolved-external(<package>)` when the oracle placed this callee in a dependency outside
    /// the corpus, deriving `<package>` from the SCIP symbol's package component; `None` for
    /// in-corpus verdicts. The display string surfaced in query output for
    /// unresolved-but-externally-resolved edges.
    pub fn resolved_external_label(&self) -> Option<String> {
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
    let scope_join = edge_oracle_scope_join(conn)?;
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
        // Re-project the LIVE edge id (#248): `edge_oracle.edge_id` is gone — the verdict joins to
        // the reindexed `edges` row by content key, so its CURRENT rowid is `edges.id`. Callers key
        // this map by the heuristic-traversal edge id (graph.rs `hop.edge_id`), which is that same
        // live id, and the `edge_ids` filter is against live edges.
        let sql = format!(
            "SELECT edges.id, edge_oracle.kind, edge_oracle.resolved_symbol_id, (SELECT value \
             FROM name_strings WHERE name_strings.id = (SELECT qualified_name_id FROM symbols \
             WHERE symbols.id = edge_oracle.resolved_symbol_id)), \
             edge_oracle.scip_symbol{scope_join}{current} AND edges.id IN ({placeholders})",
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
/// ranking (`pagerank`) walks the WHOLE edge graph, so it needs the verdict set in
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
    // Re-project the LIVE edge id (#248): keyed by `edges.id` (the reindexed rowid the content join
    // resolves to), which is the id the importance ranker's heuristic traversal carries.
    let sql = format!(
        "SELECT edges.id, edge_oracle.kind, edge_oracle.resolved_symbol_id{scope_join}{current}",
        scope_join = edge_oracle_scope_join(conn)?,
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
pub struct EdgeOracleComparison {
    pub edge_id: i64,
    pub kind: OracleResolutionKind,
    pub edge_kind: String,
    pub heuristic_confidence: String,
    pub heuristic_target: Option<String>,
    pub callee_name: Option<String>,
    /// Our `symbols.id` when the compiler resolved this callee to an IN-CORPUS symbol (an
    /// in-corpus `Contradict`: the compiler picked a different in-corpus target), `None` when
    /// it placed the callee in a dependency. `compare_graph_to_scip` labels
    /// `resolved_external` ONLY when this is `None` — a Rust SCIP symbol carries a
    /// crate/package component even for the LOCAL crate, so deriving `resolved-external` from
    /// `scip_symbol` alone would mislabel an in-corpus contradiction as
    /// `resolved-external(<local-crate>)` (#82 finding 1).
    pub resolved_symbol_id: Option<i64>,
    pub scip_symbol: String,
    pub callsite_path: String,
    pub callsite_line: i64,
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
    // Re-project the LIVE edge id (#248): `edge_oracle.edge_id` is gone; the compare surface keys
    // on `edges.id` (the reindexed rowid the content join resolves to).
    let sql = format!(
        "SELECT edges.id, edge_oracle.kind, edges.edge_kind, edges.confidence, (SELECT value FROM \
         name_strings WHERE name_strings.id = (SELECT qualified_name_id FROM symbols WHERE \
         symbols.id = edges.to_symbol_id)), edges.to_name, edge_oracle.resolved_symbol_id, \
         edge_oracle.scip_symbol, files.path, COALESCE(NULLIF(edges.source_start_line, 0), 1) \
         {scope_join}{current} ORDER BY files.path, edges.source_start_line",
        scope_join = edge_oracle_scope_join(conn)?,
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
    // Per-repo (A5): scope the prune to the ACTIVE repo's runs — `live_commits`/`live_worktrees`
    // are the active repo's live set, so without this a gc of repo A would delete every sibling
    // repo's runs (their commits are not in A's live set). `{repo_clause}` empty pre-A5. This
    // is the "oracle prune parity across repos" contract.
    let repo_clause = oracle_repo_scope_clause(conn, "oracle_runs")?;
    let deleted = conn.execute(
        &format!(
            "DELETE FROM oracle_runs
             WHERE commit_sha NOT IN ({commit_list})
               AND worktree_id NOT IN ({worktree_list}){repo_clause}"
        ),
        [],
    )?;
    Ok(u64::try_from(deleted).unwrap_or(0))
}

/// Prune `external_symbols` contracts for dead `(commit_sha, worktree_id)` checkouts (#114) — the
/// gc companion for the checkout-keyed contract table. Like `oracle_runs`, nothing cascades it, so
/// a retired branch / linked worktree would otherwise leave its dependency signature+doc payloads
/// keyed by a dead checkout forever (unbounded growth). Uses the SAME live sets and the SAME
/// per-repo, both-columns-dead predicate as [`prune_oracle_runs_outside_scope`], so a dropped
/// checkout's runs and its contracts are pruned together and a sibling repo's rows are never
/// touched.
pub(crate) fn prune_external_symbols_outside_scope(
    conn: &Connection,
    live_commits: &[String],
    live_worktrees: &[String],
) -> anyhow::Result<u64> {
    if live_commits.is_empty() && live_worktrees.is_empty() {
        return Ok(0);
    }
    let commit_list = sql_quoted_list(live_commits);
    let worktree_list = sql_quoted_list(live_worktrees);
    let repo_clause = oracle_repo_scope_clause(conn, "external_symbols")?;
    let deleted = conn.execute(
        &format!(
            "DELETE FROM external_symbols
             WHERE commit_sha NOT IN ({commit_list})
               AND worktree_id NOT IN ({worktree_list}){repo_clause}"
        ),
        [],
    )?;
    Ok(u64::try_from(deleted).unwrap_or(0))
}

/// Prune `edge_oracle` verdicts whose CONTENT key matches ZERO live edges anywhere in the index —
/// the gc replacement for the `edges_data` FK cascade dropped in #248. With no FK, a deleted edge
/// no longer cascades its verdict away, so an incremental reindex (`remove_file_in_scope` DELETEs a
/// changed file's edges every pass) leaves dangling verdict rows behind; this sweep is what stops
/// them accumulating without bound.
///
/// GLOBAL by design (R6, #248): the match is against live edges across ALL scopes (no
/// `active_checkout_file_predicate`), so a sweep run in one worktree never deletes a SIBLING
/// worktree's still-live verdict. A verdict is swept iff NO live edge anywhere shares its content
/// key (`source_path`, source/callee byte spans, `edge_kind` text) — matched by joining live
/// `edges_data` to `files.path` + one `name_strings` for the `edge_kind` text, the same content key
/// the read join uses. Returns the number of rows deleted.
///
/// CORRECTNESS DOES NOT DEPEND ON THIS SWEEP. The read path joins live `edges` by content key +
/// `files.sha256 = file_sha`, so a dangling verdict simply produces no row and is never counted or
/// surfaced — exactly the moniker model. This is anti-unbounded-growth hygiene, nothing more, so it
/// is deliberately decoupled from sweep timing (the next `oracle run`'s authoritative clear also
/// removes content-matching stale rows; this catches the rows that have no live edge at all).
pub(crate) fn prune_edge_oracle_without_live_edge(conn: &Connection) -> anyhow::Result<u64> {
    // Per-repo (A5): only sweep the ACTIVE repo's dangling verdicts (`{repo_clause}` on
    // `edge_oracle`, empty pre-A5). The inner live-edge match stays GLOBAL-across-worktrees within
    // the repo (no `active_checkout_file_predicate`), so a sibling worktree's still-live verdict is
    // never swept — the #248 R6 posture, now bounded to one repo.
    let repo_clause = oracle_repo_scope_clause(conn, "edge_oracle")?;
    let deleted = conn.execute(
        &format!(
            "
        DELETE FROM edge_oracle
        WHERE NOT EXISTS (
            SELECT 1
            FROM edges_data
            JOIN files ON files.id = edges_data.source_file_id
            JOIN name_strings ek ON ek.id = edges_data.edge_kind_id
            WHERE files.path = edge_oracle.source_path
              AND edges_data.source_start_byte = edge_oracle.source_start_byte
              AND edges_data.source_end_byte = edge_oracle.source_end_byte
              AND edges_data.callee_start_byte = edge_oracle.callee_start_byte
              AND edges_data.callee_end_byte = edge_oracle.callee_end_byte
              AND ek.value = edge_oracle.edge_kind
              -- Materialized visibility (#734): a suppressed candidate does not keep a
              -- dangling verdict alive.
              AND edges_data.hidden = 0
        ){repo_clause}
        "
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
