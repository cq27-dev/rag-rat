//! `check_library_usage` (#114): join `resolved-external` call sites to the `external_symbols`
//! dependency contracts the oracle parsed out of `index.external_symbols`, surface each
//! dependency's CURRENT signature + docs as inline context, and assert deprecated-but-compiling
//! usage.
//!
//! POSTURE (load-bearing, honest scope). The ONLY asserted verdict is `deprecated` — a
//! deterministic docs marker. `signature_text` / `documentation` are surfaced as CONTEXT for the
//! agent to reason about (arity, order, misuse); NO arity or removed/renamed verdict is asserted
//! here, because call-site arg-counts are not instrumented and a "removed" verdict needs a
//! cross-version baseline (the re-index-on-lockfile-change diff — a documented #114 follow-up).
//! Surfacing-not-asserting for non-deterministic drift is the deliberate design, mirroring the
//! issue's guidance.

use rusqlite::Connection;
use serde::Serialize;

use super::{OracleTool, latest_runs_in_scope, package_of, store};

/// The note surfaced on an `ok` report so a consuming agent knows what is asserted vs. contextual.
const POSTURE_NOTE: &str = "Asserted verdict: `deprecated` (a deterministic 'deprecated' marker \
                            in the dependency's docs/signature). Context only — reason yourself, \
                            no verdict is asserted: `signature_text` / `documentation` are the \
                            dependency's CURRENT contract at each call site, for judging arity / \
                            misuse. Removed-or-renamed and arity drift are NOT asserted here.";

/// Note for the `no_oracle_run` status.
const NO_ORACLE_RUN_NOTE: &str = "No oracle run for this checkout. Run `rag-rat oracle run` (e.g. \
                                  `--tool scip-python`), then retry.";

/// Note for the `no_external_symbols` status. Deliberately does NOT assert the indexer emits none —
/// an oracle run from BEFORE this feature (pre-V057) also leaves the table empty, so a rerun is the
/// first thing to try (the report's counts still show how many external calls are uncovered).
const NO_EXTERNAL_SYMBOLS_NOTE: &str =
    "No external-symbol contracts for this checkout. If you upgraded rag-rat since the last \
     oracle run, run `rag-rat oracle run` to (re)populate — older runs predate this data. If a \
     fresh run is still empty, the indexer emits no external symbol info (scip-python does; \
     rust-analyzer / scip-typescript do not).";

/// The status-appropriate note.
fn note_for(status: &LibraryUsageStatus) -> &'static str {
    match status {
        LibraryUsageStatus::Ok => POSTURE_NOTE,
        LibraryUsageStatus::NoOracleRun => NO_ORACLE_RUN_NOTE,
        LibraryUsageStatus::NoExternalSymbols => NO_EXTERNAL_SYMBOLS_NOTE,
    }
}

/// The default `limit` when a caller does not set one — mirrors the MCP `default_graph_limit`.
pub const DEFAULT_LIMIT: usize = 50;

/// Max call sites surfaced per moniker entry. `call_count` still reports the TRUE total; this only
/// caps the enumerated `call_sites` so a hot symbol (thousands of calls) can't balloon the
/// response.
const MAX_CALL_SITES_PER_ENTRY: usize = 25;

/// Filters for a [`check_library_usage`] read.
#[derive(Debug)]
pub struct LibraryUsageOptions {
    /// Restrict to external call sites in this exact file or under this directory prefix.
    pub path: Option<String>,
    /// Restrict to external calls whose dependency package (the moniker's package component)
    /// equals this, e.g. `ky` / `tokio`.
    pub package: Option<String>,
    /// Only surface contracts flagged `deprecated`.
    pub deprecated_only: bool,
    /// Max moniker entries returned — a hard cap honored exactly (`0` returns no entries, only the
    /// summary counts, which always cover the full pre-limit set). Pass a large value for "all".
    pub limit: usize,
}

impl Default for LibraryUsageOptions {
    fn default() -> Self {
        Self { path: None, package: None, deprecated_only: false, limit: DEFAULT_LIMIT }
    }
}

/// Why a report carries no `entries` (or how to read an empty one).
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LibraryUsageStatus {
    /// External call sites were joined to dependency contracts (entries may still be empty if a
    /// filter excluded them).
    Ok,
    /// No oracle run exists for this checkout — run `rag-rat oracle run` first.
    NoOracleRun,
    /// The oracle ran but emitted no external `SymbolInformation` (the indexer did not populate
    /// `index.external_symbols`), so there are no dependency contracts to surface.
    NoExternalSymbols,
}

/// One call site of an external dependency symbol.
#[derive(Debug, Serialize)]
pub struct LibraryUsageCallSite {
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
}

/// One external dependency symbol the corpus calls, with its contract and every call site.
#[derive(Debug, Serialize)]
pub struct LibraryUsageEntry {
    /// The raw SCIP moniker (carries the dependency's version component).
    pub moniker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    /// CONTEXT (not a verdict): the dependency's current printed signature.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub signature_text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub signature_language: String,
    /// CONTEXT (not a verdict): the dependency's current documentation.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub documentation: String,
    /// ASSERTED verdict: this contract is marked deprecated.
    pub deprecated: bool,
    pub call_count: usize,
    pub call_sites: Vec<LibraryUsageCallSite>,
}

/// The `check_library_usage` result.
#[derive(Debug, Serialize)]
pub struct LibraryUsageReport {
    pub status: LibraryUsageStatus,
    pub note: &'static str,
    pub total_external_call_sites: usize,
    pub distinct_monikers: usize,
    pub deprecated_call_sites: usize,
    /// External call sites whose moniker has NO contract in `external_symbols` (the indexer
    /// emitted no info for it) — coverage transparency, NOT a finding.
    pub call_sites_without_signature_info: usize,
    pub entries: Vec<LibraryUsageEntry>,
}

impl LibraryUsageReport {
    fn empty(status: LibraryUsageStatus) -> Self {
        Self {
            note: note_for(&status),
            status,
            total_external_call_sites: 0,
            distinct_monikers: 0,
            deprecated_call_sites: 0,
            call_sites_without_signature_info: 0,
            entries: Vec::new(),
        }
    }
}

/// A dependency contract loaded ONCE per moniker from `external_symbols`. The doc/signature text is
/// fetched a single time here (keyed by moniker), never repeated per call site.
struct Contract {
    kind: String,
    display_name: String,
    signature_text: String,
    signature_language: String,
    documentation: String,
    deprecated: bool,
}

/// One external call site — moniker + location only, NO contract text (that is looked up from the
/// contract map by moniker, so a hot symbol's large docs are not materialized per call).
struct CallSiteRow {
    moniker: String,
    source_path: String,
    start_line: i64,
    end_line: i64,
}

/// Per-moniker accumulator built while scanning call sites — counts + sites only, so the expensive
/// contract payload is cloned once per SURVIVING entry (after ranking + truncation), not per call.
struct MonikerAgg {
    call_count: usize,
    call_sites: Vec<LibraryUsageCallSite>,
    deprecated: bool,
}

/// Run `f` inside a deferred READ transaction (one WAL snapshot) when the connection is idle, so a
/// multi-statement read sees a CONSISTENT view — a concurrent writer committing between statements
/// can't change what a later statement sees. A no-op when a transaction is already open (the
/// caller's snapshot governs). Read-only, so the snapshot is only ever released. Mirrors
/// `query_api::clones::of_text::with_clone_read_snapshot`.
fn with_read_snapshot<T>(
    conn: &Connection,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if !conn.is_autocommit() {
        return f();
    }
    conn.execute_batch("BEGIN DEFERRED")?;
    let result = f();
    let _ = conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" });
    result
}

/// `check_library_usage` (#114). Runs the whole read inside ONE WAL snapshot
/// ([`with_read_snapshot`]) so the multi-statement read — flag index → call-site scan → survivor
/// contract bodies — is consistent: a concurrent `oracle run` committing mid-read cannot make a
/// ranked moniker's contract vanish between statements. See [`check_library_usage_inner`] for the
/// read itself.
pub fn check_library_usage(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
    opts: &LibraryUsageOptions,
) -> anyhow::Result<LibraryUsageReport> {
    with_read_snapshot(conn, || check_library_usage_inner(conn, commit_sha, worktree_id, opts))
}

/// Join external call sites to `external_symbols` contracts for the active checkout, across every
/// oracle tool that has a run. Groups per moniker, asserts deprecation, surfaces the current
/// signature/docs as context.
///
/// COST SHAPE (load-bearing): the flag index (moniker → deprecated) is loaded first, call sites are
/// scanned WITHOUT contract text, monikers are ranked on the cheap `(deprecated, call_count)`
/// fields, and only the surviving top-`limit` monikers materialize their doc/signature. So a symbol
/// called thousands of times with a large docstring never repeats that docstring per call, and the
/// `limit` bounds the doc/signature payload (not the whole dependency surface).
fn check_library_usage_inner(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
    opts: &LibraryUsageOptions,
) -> anyhow::Result<LibraryUsageReport> {
    let runs = latest_runs_in_scope(conn, commit_sha, worktree_id)?;
    if runs.is_empty() {
        return Ok(LibraryUsageReport::empty(LibraryUsageStatus::NoOracleRun));
    }

    let package_filter = opts.package.as_deref().map(str::trim).filter(|pkg| !pkg.is_empty());

    // LIGHTWEIGHT rank/coverage index: `moniker -> deprecated` flag, NO doc/signature text.
    // Presence means a contract exists; the value is the asserted verdict. Loading only the
    // flag keeps the scan + ranking cheap — the heavy doc/signature bodies are fetched below
    // for SURVIVING entries only, so the payload is bounded by `limit`, not by the dependency
    // surface. Monikers are tool-unique (the SCIP scheme names the tool), so merging tools is
    // collision-free. An EMPTY index means the indexer emitted no `index.external_symbols` (the
    // `no_external_symbols` diagnostic) — but we still scan the call sites so the coverage
    // counts reflect the real gap.
    let mut flags: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for (tool, _) in &runs {
        for (moniker, deprecated) in load_contract_flags(conn, *tool, commit_sha, worktree_id)? {
            flags.insert(moniker, deprecated);
        }
    }
    let no_contracts = flags.is_empty();

    // Group external call sites by moniker — counts + (deterministically capped) locations only.
    let mut agg: std::collections::HashMap<String, MonikerAgg> = std::collections::HashMap::new();
    let mut total = 0usize;
    let mut without_info = 0usize;
    let mut deprecated_sites = 0usize;
    for (tool, tool_version) in &runs {
        for site in external_call_sites(conn, *tool, tool_version, commit_sha, worktree_id, opts)? {
            // The package filter is applied here (not in SQL) because the package component is
            // extracted by `scip::symbol::parse_symbol`, not a stored column.
            if let Some(pkg) = package_filter
                && package_of(&site.moniker).as_deref() != Some(pkg)
            {
                continue;
            }
            let deprecated = flags.get(&site.moniker).copied();
            // `deprecated_only` keeps only call sites whose contract is deprecated (a call site
            // with no contract can never be deprecated, so it drops too).
            if opts.deprecated_only && deprecated != Some(true) {
                continue;
            }
            total += 1;
            let Some(deprecated) = deprecated else {
                without_info += 1;
                continue;
            };
            if deprecated {
                deprecated_sites += 1;
            }
            let entry = agg.entry(site.moniker).or_insert_with(|| MonikerAgg {
                call_count: 0,
                call_sites: Vec::new(),
                deprecated,
            });
            // `call_count` is the TRUE total; the surfaced `call_sites` are capped at
            // [`MAX_CALL_SITES_PER_ENTRY`] so a symbol called thousands of times can't balloon the
            // response. `external_call_sites` orders deterministically, so the kept subset is
            // stable across reindex / query-plan changes (not an arbitrary SQL row
            // order).
            if entry.call_sites.len() < MAX_CALL_SITES_PER_ENTRY {
                entry.call_sites.push(LibraryUsageCallSite {
                    path: site.source_path,
                    start_line: site.start_line,
                    end_line: site.end_line,
                });
            }
            entry.call_count += 1;
        }
    }

    let distinct_monikers = agg.len();
    // Rank on the cheap fields (deprecated first — the actionable ones — then most-called, then
    // moniker for stability) and truncate to `limit` BEFORE any doc/signature is loaded (`0` yields
    // no entries; the summary counts above still cover the full pre-limit set).
    let mut ranked: Vec<(String, MonikerAgg)> = agg.into_iter().collect();
    ranked.sort_by(|(a_mon, a), (b_mon, b)| {
        b.deprecated.cmp(&a.deprecated).then(b.call_count.cmp(&a.call_count)).then(a_mon.cmp(b_mon))
    });
    ranked.truncate(opts.limit);

    // NOW fetch the heavy contract bodies — for the SURVIVING monikers only — so the doc/signature
    // payload materialized is bounded by `limit`, not by the whole dependency surface.
    let survivors: Vec<&str> = ranked.iter().map(|(moniker, _)| moniker.as_str()).collect();
    let mut contracts: std::collections::HashMap<String, Contract> =
        std::collections::HashMap::new();
    if !survivors.is_empty() {
        for (tool, _) in &runs {
            for (moniker, contract) in
                load_contracts_for(conn, *tool, commit_sha, worktree_id, &survivors)?
            {
                contracts.insert(moniker, contract);
            }
        }
    }

    let entries = ranked
        .into_iter()
        .map(|(moniker, agg)| {
            let contract = contracts.get(&moniker).expect("survivor monikers all carry a contract");
            LibraryUsageEntry {
                package: package_of(&moniker),
                moniker,
                kind: contract.kind.clone(),
                display_name: contract.display_name.clone(),
                signature_text: contract.signature_text.clone(),
                signature_language: contract.signature_language.clone(),
                documentation: contract.documentation.clone(),
                deprecated: contract.deprecated,
                call_count: agg.call_count,
                call_sites: agg.call_sites,
            }
        })
        .collect();

    // `no_external_symbols` when the indexer emitted no contracts (the useful diagnostic), else
    // `ok`. Either way the coverage counts above are populated from the real call-site scan — so a
    // repo whose indexer omits `index.external_symbols` still sees HOW MANY external calls have no
    // contract (`total_external_call_sites` / `call_sites_without_signature_info`), not a bare
    // zero.
    let status =
        if no_contracts { LibraryUsageStatus::NoExternalSymbols } else { LibraryUsageStatus::Ok };

    Ok(LibraryUsageReport {
        note: note_for(&status),
        status,
        total_external_call_sites: total,
        distinct_monikers,
        deprecated_call_sites: deprecated_sites,
        call_sites_without_signature_info: without_info,
        entries,
    })
}

/// The LIGHTWEIGHT rank/coverage index for `(tool, checkout, repo)` — `moniker` + the `deprecated`
/// flag ONLY, no doc/signature bodies. Loaded before the scan so ranking + coverage counting never
/// pulls the heavy contract text; the surviving entries' bodies come from [`load_contracts_for`].
fn load_contract_flags(
    conn: &Connection,
    tool: OracleTool,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<Vec<(String, bool)>> {
    let repo_clause = store::oracle_repo_scope_clause(conn, "external_symbols")?;
    let sql = format!(
        "SELECT moniker, deprecated FROM external_symbols
         WHERE tool = ?1 AND commit_sha = ?2 AND worktree_id = ?3{repo_clause}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params![tool.as_db_str(), commit_sha, worktree_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The FULL `external_symbols` contracts (kind / display_name / signature / docs) for a BOUNDED set
/// of `monikers` in `(tool, checkout, repo)` — used to materialize ONLY the surviving entries after
/// ranking, so the heavy payload is bounded by `limit`. A tool's table holds only its own monikers,
/// so passing the full survivor set to each tool is safe (non-matching monikers just don't join).
fn load_contracts_for(
    conn: &Connection,
    tool: OracleTool,
    commit_sha: &str,
    worktree_id: &str,
    monikers: &[&str],
) -> anyhow::Result<Vec<(String, Contract)>> {
    if monikers.is_empty() {
        return Ok(Vec::new());
    }
    let repo_clause = store::oracle_repo_scope_clause(conn, "external_symbols")?;
    // Chunk the moniker IN-list so a huge `limit` (many surviving monikers) can't exceed SQLite's
    // host-parameter cap (default 999): 3 fixed params (?1..?3) + one placeholder per moniker.
    const CHUNK: usize = 500;
    let mut out = Vec::with_capacity(monikers.len());
    for chunk in monikers.chunks(CHUNK) {
        // The moniker IN-list placeholders start at ?4 (?1..?3 are tool / commit_sha /
        // worktree_id).
        let placeholders =
            (0..chunk.len()).map(|i| format!("?{}", i + 4)).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT moniker, kind, display_name, signature_text, signature_language, \
             documentation, deprecated
             FROM external_symbols
             WHERE tool = ?1 AND commit_sha = ?2 AND worktree_id = ?3{repo_clause}
               AND moniker IN ({placeholders})"
        );
        let mut binds: Vec<&str> = vec![tool.as_db_str(), commit_sha, worktree_id];
        binds.extend_from_slice(chunk);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds), |row| {
                Ok((row.get::<_, String>(0)?, Contract {
                    kind: row.get(1)?,
                    display_name: row.get(2)?,
                    signature_text: row.get(3)?,
                    signature_language: row.get(4)?,
                    documentation: row.get(5)?,
                    deprecated: row.get(6)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.extend(rows);
    }
    Ok(out)
}

/// The external call sites for ONE oracle tool's run — moniker + location, NO contract text. Reuses
/// [`store::edge_oracle_scope_join`] verbatim (the canonical active-checkout + repo scope predicate
/// is never re-spelled, #248).
///
/// EXTERNAL POPULATION (load-bearing): the filter is `edge_oracle.resolved_symbol_id IS NULL`, NOT
/// `kind = 'resolved-external'`. That NULL captures the COMPLETE external set — both
/// `resolved-external` AND the external-target `contradict` (where the heuristic mis-bound a
/// dependency call to an in-corpus symbol; SCIP disagrees and resolves it external, keeping the
/// external moniker with a NULL resolved id). Every in-corpus verdict resolves to a symbol, so NULL
/// excludes them exactly.
fn external_call_sites(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    opts: &LibraryUsageOptions,
) -> anyhow::Result<Vec<CallSiteRow>> {
    let scope_join = store::edge_oracle_scope_join(conn)?;

    // Path filter: exact file OR anything under `<path>/`. `instr(x, y) = 1` (y is a prefix of x)
    // avoids LIKE metacharacter hazards on a caller-supplied path; the trailing `/` pins a
    // directory boundary so `src/foo` never matches `src/foobar`. Trailing separators are
    // trimmed first, so a caller passing `src/` builds the `src/` boundary (not a no-matching
    // `src//`).
    let path_filter = opts
        .path
        .as_deref()
        .map(|path| path.trim().trim_end_matches('/'))
        .filter(|path| !path.is_empty());
    let path_clause = if path_filter.is_some() {
        " AND (edge_oracle.source_path = ?5 OR instr(edge_oracle.source_path, ?5 || '/') = 1)"
    } else {
        ""
    };

    // Restrict to INVOCATION edges — the kinds where a dependency's callable is actually called:
    // `calls_name` (function/method calls), `constructs` (constructor calls, e.g. `new Foo()`), and
    // `dispatches` (dynamic dispatch). This is the same call-edge set find_callers/trace_callees
    // use (`query::graph::CALL_EDGE_KINDS`). An external token ALSO produces `references_type`
    // / `uses_macro` / `imports` edges (each with a NULL resolved id); without this filter a
    // type-referenced dependency would be miscounted as a call — double-counting a token that both
    // calls and type-references, and mislabeling a type ref as a call site.
    // GROUP BY the physical CALLEE-token identity `(source_path, callee_start_byte,
    // callee_end_byte)` — one row per source call site, NOT per verdict. This collapses BOTH:
    //  * the content join's fan-out (a verdict CAN match >1 live edge), and
    //  * a single call that emits MULTIPLE invocation edge kinds at the SAME callee span — e.g. a
    //    Kotlin constructor `Foo()` produces both a `calls_name` and a `constructs` edge, stored as
    //    separate `edge_oracle` rows because `edge_kind` is part of its key.
    // Without this, one source call would count 2× and burn the per-entry site cap on the
    // duplicate. A callee span uniquely identifies a token in a file, so the grouped
    // `scip_symbol` / `edges.*` columns are identical across the collapsed rows.
    let sql = format!(
        "SELECT edge_oracle.scip_symbol, edge_oracle.source_path, edges.source_start_line, \
         edges.source_end_line
         {scope_join} AND edge_oracle.resolved_symbol_id IS NULL AND edge_oracle.edge_kind IN \
         ('calls_name', 'constructs', 'dispatches'){path_clause}
         GROUP BY edge_oracle.source_path, edge_oracle.callee_start_byte, \
         edge_oracle.callee_end_byte
         ORDER BY edge_oracle.source_path, edge_oracle.callee_start_byte"
    );

    // Bind slots ?1..?4 are fixed by `edge_oracle_scope_join`; ?5 is the optional path filter.
    let mut binds: Vec<String> = vec![
        tool.as_db_str().to_string(),
        tool_version.to_string(),
        commit_sha.to_string(),
        worktree_id.to_string(),
    ];
    if let Some(path) = path_filter {
        binds.push(path.to_string());
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
            Ok(CallSiteRow {
                moniker: row.get(0)?,
                source_path: row.get(1)?,
                start_line: row.get(2)?,
                end_line: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
