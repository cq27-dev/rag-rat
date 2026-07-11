mod historical;
mod items;
mod neighbors;
mod select;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) use historical::*;
pub(crate) use items::*;
pub(crate) use neighbors::*;
use rusqlite::{Connection, OptionalExtension, params};
pub(crate) use select::*;
use serde::Serialize;

use crate::query::graph::{self, GraphHop, GraphResolutionMode, GraphTraversalOptions};
use crate::query::memory::{self, CompactRepoMemoryEvidence, RepoMemoryEvidence};
use crate::query::symbol::SymbolHit;

/// An FTS5 phrase query for `needle` (a symbol name / qualified name / path), or `None` when it has
/// no alphanumeric token. Used to find chunks that MENTION the needle through the `chunk_fts` index
/// instead of a raw `chunks.text LIKE '%needle%'` full-table scan — same intent, but tokenized +
/// indexed, and it never reads raw chunk text (so it stays fast and survives #77 text compression).
/// Wrapped as a quoted FTS5 phrase (embedded `"` doubled) so `::`, `(`, `<`, etc. in a symbol name
/// tokenize as separators rather than parse as FTS query syntax. Semantics shift substring→token —
/// more precise for "mentions this symbol" than a substring match.
pub(crate) fn fts_phrase_query(needle: &str) -> Option<String> {
    if !needle.chars().any(char::is_alphanumeric) {
        return None;
    }
    Some(format!("\"{}\"", needle.replace('"', "\"\"")))
}

#[derive(Debug, Serialize)]
pub struct ImpactItem {
    pub path: String,
    #[serde(rename = "lang")]
    pub language: String,
    pub kind: String,
    pub symbol: Option<String>,
    pub category: String,
    pub reason: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ImpactSurfaceOptions {
    pub resolution_mode: GraphResolutionMode,
    pub include_tests: bool,
    pub include_docs: bool,
    pub include_git: bool,
    pub include_papertrail: bool,
    pub include_text_fallback: bool,
    pub include_memories: bool,
    /// Emit `repo_memories` as the scannable compact view (default) rather than full bodies +
    /// bindings + call paths (#37). The agent-facing MCP default is compact; full detail stays one
    /// lookup away (`memory_for_symbol` / `memory_for_path` / `memory_for_call_path`).
    pub compact_memories: bool,
    /// How the compact `repo_memories` headers render (`[memory] surface`). `Summary` (the
    /// default) hydrates each header with the dream-compacted summary and verdict marker for
    /// the memory's current body (dream v2 pass 2); `Full` keeps the mechanical header. Only
    /// consulted when `compact_memories` is set; the full-body view is unaffected.
    pub surface: crate::config::MemorySurface,
}

/// `impact_surface`'s `repo_memories` payload — compact by default (#37), full on request. The two
/// variants serialize with identical field names (`direct` / `path_crossed` / `call_path_crossed` /
/// `stale`), so only the per-memory detail differs on the wire, not the lane shape.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RepoMemoryEvidenceView {
    Compact(CompactRepoMemoryEvidence),
    Full(RepoMemoryEvidence),
}

impl RepoMemoryEvidenceView {
    /// Borrow the full evidence; `None` when this view is compact.
    pub fn full(&self) -> Option<&RepoMemoryEvidence> {
        match self {
            Self::Full(evidence) => Some(evidence),
            Self::Compact(_) => None,
        }
    }

    /// Borrow the compact evidence; `None` when this view is full.
    pub fn compact(&self) -> Option<&CompactRepoMemoryEvidence> {
        match self {
            Self::Compact(evidence) => Some(evidence),
            Self::Full(_) => None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ImpactSurfaceReport {
    pub query: ImpactSurfaceQuery,
    pub direct_semantic_callers: Vec<GraphHop>,
    pub direct_semantic_callees: Vec<GraphHop>,
    pub import_export_dependents: Vec<ImpactItem>,
    pub tests_touching_symbol_path: Vec<ImpactItem>,
    pub docs_mentioning_symbol_path: Vec<ImpactItem>,
    pub text_fallback_hits: Vec<ImpactItem>,
    pub recent_commits_touching_symbol_path: Vec<ImpactItem>,
    pub files_co_changed_with_symbol_path: Vec<ImpactItem>,
    pub github_rationale_issues_prs: Vec<ImpactItem>,
    pub repo_memories: RepoMemoryEvidenceView,
    pub completeness_and_caveats: ImpactCompleteness,
}

#[derive(Debug, Serialize)]
pub struct ImpactSurfaceQuery {
    // Internal rowid — never serialized (reindex-churned, #149); `symbol_path` identifies the
    // query.
    #[serde(skip_serializing)]
    pub symbol_id: Option<i64>,
    #[serde(rename = "ref")]
    pub symbol_path: Option<String>,
    pub query: Option<String>,
    pub resolution: String,
}

#[derive(Debug, Default, Serialize)]
pub struct ImpactCompleteness {
    pub exact_graph_callers: u64,
    pub graph_callees: u64,
    pub text_fallback_hits: u64,
    pub parser_failures: u64,
    pub stale_files: u64,
    pub memory_status: ImpactMemoryStatus,
    /// Sections that returned exactly `limit` rows and were therefore capped — more results may
    /// exist (no silent caps, #49). Empty when nothing was truncated.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub truncated_sections: Vec<String>,
    pub caveats: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct ImpactMemoryStatus {
    pub active: u64,
    pub stale: u64,
}

impl Default for ImpactSurfaceOptions {
    fn default() -> Self {
        Self {
            resolution_mode: GraphResolutionMode::Syntactic,
            include_tests: true,
            include_docs: true,
            include_git: true,
            include_papertrail: true,
            include_text_fallback: true,
            include_memories: true,
            compact_memories: true,
            surface: crate::config::MemorySurface::default(),
        }
    }
}

pub fn impact_surface(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    impact_surface_with_options(conn, query, limit, GraphResolutionMode::Syntactic)
}

/// Traverse one direction, oracle-enrich, re-rank by effective confidence ONLY when a hop was
/// promoted, and truncate to `limit` — the impact-side mirror of
/// `IndexDatabase::traverse_with_oracle` so a compiler-upgraded neighbor survives the limit here
/// too (#82 finding 4). Overfetches via `graph::oracle_overfetch_limit`, runs `enrich` over the
/// larger candidate set, and — when `enrich` reports a promotion — stable-sorts by
/// `graph::effective_confidence_rank` (so `compiler` outranks `exact`) before truncating.
///
/// `enrich` returns whether any hop was PROMOTED to `compiler`. With no promotion (no oracle run,
/// or no in-scope verdict on these hops) the heuristic order + the caller's `limit` are already
/// correct — re-sorting would change truncation membership on EVERY query, including repos with no
/// oracle run (#82 P2). The overfetched set is in heuristic order, so its first `limit` rows are
/// the original top-`limit`, identical to pre-oracle behavior.
fn oracle_ranked_neighbors(
    conn: &Connection,
    symbol: &str,
    reverse: bool,
    limit: u32,
    graph_options: &GraphTraversalOptions,
    enrich: &impl Fn(&mut Vec<GraphHop>) -> anyhow::Result<bool>,
) -> anyhow::Result<Vec<GraphHop>> {
    let overfetch = graph::oracle_overfetch_limit(limit);
    let mut hops = graph::traverse_with_options(conn, symbol, reverse, overfetch, graph_options)?;
    if enrich(&mut hops)? {
        hops.sort_by_key(|hop| graph::effective_confidence_rank(&hop.confidence));
    }
    hops.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(hops)
}

pub fn impact_surface_report_for_symbol(
    conn: &Connection,
    symbol: &SymbolHit,
    limit: u32,
    options: &ImpactSurfaceOptions,
    enrich: impl Fn(&mut Vec<GraphHop>) -> anyhow::Result<bool>,
) -> anyhow::Result<ImpactSurfaceReport> {
    let graph_options = GraphTraversalOptions {
        resolution_mode: options.resolution_mode,
        symbol_id: Some(symbol.symbol_id),
        logical_symbol_id: symbol.logical_symbol_id,
        ..Default::default()
    };
    // Overfetch → oracle-enrich → re-rank by effective confidence → truncate, so a
    // compiler-upgraded low-confidence neighbor isn't dropped by the heuristic `LIMIT` before
    // enrichment runs (#82 finding 4). Everything downstream (memory evidence edge ids,
    // completeness counts) sees the SAME truncated, re-ranked window the report returns.
    // `enrich` is a no-op for callers without an oracle pass (e.g. tests), so the lists
    // collapse back to the plain heuristic top-`limit`.
    let direct_semantic_callers = oracle_ranked_neighbors(
        conn,
        &symbol.qualified_name,
        true,
        limit,
        &graph_options,
        &enrich,
    )?;
    let direct_semantic_callees = oracle_ranked_neighbors(
        conn,
        &symbol.qualified_name,
        false,
        limit,
        &graph_options,
        &enrich,
    )?;
    let names = vec![symbol.name.clone(), symbol.qualified_name.clone()];
    let import_export_dependents =
        import_export_items(conn, symbol.symbol_id, &symbol.qualified_name, &names, limit)?;
    let tests_touching_symbol_path =
        if options.include_tests { test_items(conn, symbol, &names, limit)? } else { Vec::new() };
    let docs_mentioning_symbol_path =
        if options.include_docs { docs_items(conn, symbol, &names, limit)? } else { Vec::new() };
    let text_fallback_hits = if options.include_text_fallback {
        text_fallback_items(conn, symbol, &names, limit)?
    } else {
        Vec::new()
    };
    let recent_commits_touching_symbol_path = if options.include_git {
        git_commit_items(conn, std::slice::from_ref(&symbol.path), limit)?
    } else {
        Vec::new()
    };
    // Change coupling is git-derived evidence, so it rides the existing `include_git` gate (no new
    // `ImpactInclude` variant). The DerivedIndex table is kept fresh by `ensure_coupling_fresh` on
    // the IndexDatabase seam before this pure reader runs.
    let files_co_changed_with_symbol_path =
        if options.include_git { coupling_items(conn, &symbol.path, limit)? } else { Vec::new() };
    let github_rationale_issues_prs = if options.include_papertrail {
        let mut items = github_ref_items(conn, std::slice::from_ref(&symbol.path), limit)?;
        items.extend(github_rationale_items(conn, &symbol.qualified_name, limit)?);
        items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        items
    } else {
        Vec::new()
    };
    let (repo_memories, memories_truncated) = if options.include_memories {
        let caller_edge_ids =
            direct_semantic_callers.iter().map(|hop| hop.edge_id).collect::<Vec<_>>();
        let callee_edge_ids =
            direct_semantic_callees.iter().map(|hop| hop.edge_id).collect::<Vec<_>>();
        memory::memory_evidence_for_symbol_and_edges(
            conn,
            symbol,
            &caller_edge_ids,
            &callee_edge_ids,
            limit,
        )?
    } else {
        (
            RepoMemoryEvidence {
                direct: Vec::new(),
                path_crossed: Vec::new(),
                call_path_crossed: Vec::new(),
                stale: Vec::new(),
            },
            false,
        )
    };
    let mut caveats = vec![
        "Graph evidence is tree-sitter/syntactic, not compiler-grade name resolution.".to_string(),
    ];
    if options.resolution_mode == GraphResolutionMode::Exact
        && direct_semantic_callers.is_empty()
        && !text_fallback_hits.is_empty()
    {
        caveats.push(format!(
            "No exact graph callers found. Text search found {} symbol/path hits. This likely \
             indicates graph extraction or resolution gaps.",
            text_fallback_hits.len()
        ));
    }
    // No silent caps (#49): a section that returns exactly `limit` rows was capped and may hide
    // more. Name every capped section so the agent can raise `limit` or narrow the query instead of
    // trusting a truncated list as complete.
    let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
    let truncated_sections: Vec<String> = [
        ("direct_semantic_callers", direct_semantic_callers.len()),
        ("direct_semantic_callees", direct_semantic_callees.len()),
        ("import_export_dependents", import_export_dependents.len()),
        ("tests_touching_symbol_path", tests_touching_symbol_path.len()),
        ("docs_mentioning_symbol_path", docs_mentioning_symbol_path.len()),
        ("text_fallback_hits", text_fallback_hits.len()),
        ("recent_commits_touching_symbol_path", recent_commits_touching_symbol_path.len()),
        ("files_co_changed_with_symbol_path", files_co_changed_with_symbol_path.len()),
        ("github_rationale_issues_prs", github_rationale_issues_prs.len()),
    ]
    .into_iter()
    .filter(|&(_, len)| limit_usize != 0 && len >= limit_usize)
    .map(|(name, _)| name.to_string())
    // `repo_memories` is capped per lane inside memory_evidence; its `memories_truncated` flag
    // accounts for rows split off to `stale` (which the active-lane lengths would miss), #146 review.
    .chain(memories_truncated.then(|| "repo_memories".to_string()))
    .collect();
    if !truncated_sections.is_empty() {
        caveats.push(format!(
            "Sections truncated at limit={limit}: {}. More results may exist — raise `limit` or \
             narrow the query.",
            truncated_sections.join(", ")
        ));
    }
    // Completeness counts read the FULL evidence lanes, so compute them before the compact
    // projection moves `repo_memories` into the view (#37).
    let memory_active = u64::try_from(
        repo_memories.direct.len()
            + repo_memories.path_crossed.len()
            + repo_memories.call_path_crossed.len(),
    )
    .unwrap_or(u64::MAX);
    let memory_stale = u64::try_from(repo_memories.stale.len()).unwrap_or(u64::MAX);
    let repo_memories = if options.compact_memories {
        // Under `surface = "summary"` each compact header is hydrated with the dream summary +
        // verdict marker for the memory's current body (a missing summary → the mechanical header);
        // `full` keeps the purely mechanical projection. The full-BODY view (compact_memories =
        // false) is unaffected — `memory show` remains the expand path there.
        let compact = match options.surface {
            crate::config::MemorySurface::Summary => repo_memories.compact_summary_first(conn)?,
            crate::config::MemorySurface::Full => repo_memories.compact(),
        };
        RepoMemoryEvidenceView::Compact(compact)
    } else {
        RepoMemoryEvidenceView::Full(repo_memories)
    };
    Ok(ImpactSurfaceReport {
        query: ImpactSurfaceQuery {
            symbol_id: Some(symbol.symbol_id),
            symbol_path: Some(symbol.qualified_name.clone()),
            query: None,
            resolution: options.resolution_mode.as_str().to_string(),
        },
        completeness_and_caveats: ImpactCompleteness {
            exact_graph_callers: u64::try_from(direct_semantic_callers.len()).unwrap_or(u64::MAX),
            graph_callees: u64::try_from(direct_semantic_callees.len()).unwrap_or(u64::MAX),
            text_fallback_hits: u64::try_from(text_fallback_hits.len()).unwrap_or(u64::MAX),
            parser_failures: parser_failure_count(conn)?,
            stale_files: 0,
            memory_status: ImpactMemoryStatus { active: memory_active, stale: memory_stale },
            truncated_sections,
            caveats,
        },
        direct_semantic_callers,
        direct_semantic_callees,
        import_export_dependents,
        tests_touching_symbol_path,
        docs_mentioning_symbol_path,
        text_fallback_hits,
        recent_commits_touching_symbol_path,
        files_co_changed_with_symbol_path,
        github_rationale_issues_prs,
        repo_memories,
    })
}

pub fn impact_surface_with_options(
    conn: &Connection,
    query: &str,
    limit: u32,
    resolution_mode: GraphResolutionMode,
) -> anyhow::Result<Vec<ImpactItem>> {
    impact_surface_from_targets(conn, query, None, limit, resolution_mode)
}

pub fn impact_surface_for_symbol(
    conn: &Connection,
    symbol: &SymbolHit,
    limit: u32,
    resolution_mode: GraphResolutionMode,
) -> anyhow::Result<Vec<ImpactItem>> {
    let target = SymbolTarget {
        id: symbol.symbol_id,
        file_id: symbol.file_id,
        path: symbol.path.clone(),
        language: symbol.language.clone(),
        file_kind: symbol.file_kind.clone(),
        name: symbol.name.clone(),
        qualified_name: symbol.qualified_name.clone(),
    };
    impact_surface_from_targets(
        conn,
        &symbol.qualified_name,
        Some(vec![target]),
        limit,
        resolution_mode,
    )
}

fn impact_surface_from_targets(
    conn: &Connection,
    query: &str,
    selected_targets: Option<Vec<SymbolTarget>>,
    limit: u32,
    resolution_mode: GraphResolutionMode,
) -> anyhow::Result<Vec<ImpactItem>> {
    let max_items = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut surface = ImpactSurface::default();
    let targets = match selected_targets {
        Some(targets) => targets,
        None => exact_symbols(conn, query)?,
    };
    let target_names = target_names(query, &targets);

    for symbol in &targets {
        surface.push(
            ImpactCategory::DirectStructural,
            FileSymbol {
                path: symbol.path.clone(),
                language: symbol.language.clone(),
                kind: symbol.file_kind.clone(),
                symbol: Some(symbol.qualified_name.clone()),
            },
            "exact_symbol_definition",
            format!("defined as {}", symbol.qualified_name),
        );
    }

    graph_neighbors(conn, &targets, &target_names, true, resolution_mode, &mut surface)?;
    graph_neighbors(conn, &targets, &target_names, false, resolution_mode, &mut surface)?;
    import_export_dependents(conn, &targets, &target_names, &mut surface)?;
    same_file_siblings(conn, &targets, &mut surface)?;

    // #150: probe ONE past the limit so a section that exactly fills the budget still reveals it
    // had more. The per-section caps (`textual_fallback`, `historical_evidence`) otherwise stop
    // at exactly `max_items`, leaving `surface.len() == max_items` — indistinguishable from a
    // result that genuinely had no more, so the sentinel below would never fire for the
    // free-text fallback / history paths (Codex review on #150). `into_items(max_items)`
    // discards the probe item.
    let probe = max_items.saturating_add(1);
    if surface.len() < probe {
        let remaining = probe.saturating_sub(surface.len());
        textual_fallback(conn, query, &mut surface, remaining)?;
    }

    let current_paths = surface.current_paths();
    historical_evidence(conn, &current_paths, query, &mut surface, probe)?;

    // The flat shape capped at `limit` silently — a capped result read as complete. When the probed
    // surface overflowed `limit`, append a visible completeness sentinel (the flat-shape analogue
    // of the structured report's `truncated_sections`). Kept as a trailing `ImpactItem` so the
    // compatibility `Vec<ImpactItem>` shape is unchanged; `category`/`kind` = `"completeness"` so a
    // reader can detect it structurally.
    let truncated = surface.len() > max_items;
    let mut items = surface.into_items(max_items);
    if truncated {
        items.push(impact_truncation_notice(max_items));
    }
    Ok(items)
}

/// Trailing sentinel flagging that a flat `impact_surface` result was capped at `limit` (#150) — so
/// a truncated flat result can't be mistaken for complete. No precise dropped count: the
/// per-section caps mean the true total isn't known cheaply (we only probe one past the limit).
/// `category`/ `kind` are `"completeness"` for structural detection.
fn impact_truncation_notice(limit: usize) -> ImpactItem {
    ImpactItem {
        path: String::new(),
        language: String::new(),
        kind: "completeness".to_string(),
        symbol: None,
        category: "completeness".to_string(),
        reason: format!(
            "result capped at {limit} impact items — more exist; raise `limit`, or use a symbol \
             selector (symbol_path/symbol) for the structured report with per-section truncation"
        ),
        evidence: vec!["more impact items exist beyond the limit".to_string()],
    }
}

pub fn ffi_surface(conn: &Connection, limit: u32) -> anyhow::Result<Vec<ImpactItem>> {
    let mut stmt = conn.prepare(
        "
        WITH rust_exports AS (
            SELECT DISTINCT
                   files.path AS path,
                   files.language AS language,
                   files.kind AS kind,
                   qn.value AS symbol,
                   CASE
                       WHEN symbols.kind = 'impl' THEN 'rust_uniffi_exported_impl'
                       ELSE 'rust_uniffi_export'
                   END AS reason
            FROM symbols
            JOIN files ON files.id = symbols.file_id
            LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
            JOIN symbol_facts
              ON symbol_facts.symbol_id = symbols.id
             AND symbol_facts.fact_kind = 'rust_attr'
             AND symbol_facts.fact_value = 'uniffi_export'
            WHERE files.language = 'rust'
              AND symbols.kind IN ('function', 'method', 'impl', 'struct', 'enum', 'trait')
        ),
        rust_exported_impl_members AS (
            SELECT DISTINCT
                   files.path AS path,
                   files.language AS language,
                   files.kind AS kind,
                   members_qn.value AS symbol,
                   'rust_uniffi_impl_member' AS reason
            FROM symbols AS impls
            JOIN files ON files.id = impls.file_id
            JOIN symbol_facts
              ON symbol_facts.symbol_id = impls.id
             AND symbol_facts.fact_kind = 'rust_attr'
             AND symbol_facts.fact_value = 'uniffi_export'
            JOIN symbols AS members
              ON members.file_id = impls.file_id
             AND members.start_byte > impls.start_byte
             AND members.end_byte < impls.end_byte
             AND members.kind IN ('function', 'method')
            LEFT JOIN name_strings members_qn ON members_qn.id = members.qualified_name_id
            WHERE files.language = 'rust'
              AND impls.kind = 'impl'
        ),
        binding_refs AS (
            -- Generated/binding artifacts detected by path. Detection is generic on purpose:
            -- matching specific native-symbol substrings in chunk text was project-specific and
            -- self-matched any source that merely mentions those names (e.g. this query). The
            -- `#[uniffi::export]` symbol facts above are the principled, language-level signal.
            SELECT DISTINCT
                   files.path AS path,
                   files.language AS language,
                   files.kind AS kind,
                   chunks.symbol_path AS symbol,
                   'generated_binding_artifact' AS reason
            FROM files
            JOIN chunks ON chunks.file_id = files.id
            WHERE files.path LIKE '%/src/generated/%'
               OR files.path LIKE '%/generated/%'
               OR files.path LIKE '%generated-manifest.json'
        )
        SELECT path, language, kind, symbol, reason FROM rust_exports
        UNION
        SELECT path, language, kind, symbol, reason FROM rust_exported_impl_members
        UNION
        SELECT path, language, kind, symbol, reason FROM binding_refs
        ORDER BY reason, kind DESC, path
        LIMIT ?1
        ",
    )?;
    rows_to_items(stmt.query_map([limit], |row| {
        let reason: String = row.get(4)?;
        Ok(ImpactItem {
            path: row.get(0)?,
            language: row.get(1)?,
            kind: row.get(2)?,
            symbol: row.get(3)?,
            category: ImpactCategory::ProbableTextual.as_str().to_string(),
            reason: reason.clone(),
            evidence: ffi_surface_evidence(&reason),
        })
    })?)
}

fn ffi_surface_evidence(reason: &str) -> Vec<String> {
    let mut evidence = vec![format!("ffi_surface evidence class: {reason}")];
    match reason {
        "rust_uniffi_impl_member" => {
            evidence.push(
                "member symbol is inside a chunk containing an exported UniFFI impl".to_string(),
            );
            evidence.push(
                "this row is not claiming the member itself has a #[uniffi::export] attribute"
                    .to_string(),
            );
        },
        "rust_uniffi_exported_impl" => {
            evidence.push(
                "exported impl/type surface; member rows are reported separately when symbols are \
                 available"
                    .to_string(),
            );
        },
        _ => {},
    }
    evidence
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ImpactCategory {
    DirectStructural,
    ProbableTextual,
    HistoricalPapertrail,
}

impl ImpactCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::DirectStructural => "Direct structural impact",
            Self::ProbableTextual => "Probable textual impact",
            Self::HistoricalPapertrail => "Historical/papertrail evidence",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FileSymbol {
    path: String,
    language: String,
    kind: String,
    symbol: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SymbolTarget {
    id: i64,
    file_id: i64,
    path: String,
    language: String,
    file_kind: String,
    name: String,
    qualified_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ImpactKey {
    category: &'static str,
    path: String,
    symbol: Option<String>,
    reason: String,
}

#[derive(Default)]
pub(crate) struct ImpactSurface {
    items: BTreeMap<ImpactKey, ImpactItem>,
}

impl ImpactSurface {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn push(
        &mut self,
        category: ImpactCategory,
        file_symbol: FileSymbol,
        reason: impl Into<String>,
        evidence: impl Into<String>,
    ) {
        let reason = reason.into();
        let key = ImpactKey {
            category: category.as_str(),
            path: file_symbol.path.clone(),
            symbol: file_symbol.symbol.clone(),
            reason: reason.clone(),
        };
        let item = self.items.entry(key).or_insert_with(|| ImpactItem {
            path: file_symbol.path,
            language: file_symbol.language,
            kind: file_symbol.kind,
            symbol: file_symbol.symbol,
            category: category.as_str().to_string(),
            reason,
            evidence: Vec::new(),
        });
        let evidence = evidence.into();
        if !item.evidence.iter().any(|value| value == &evidence) {
            item.evidence.push(evidence);
        }
    }

    fn current_paths(&self) -> Vec<String> {
        let mut paths = BTreeSet::new();
        for item in self.items.values() {
            if item.category != ImpactCategory::HistoricalPapertrail.as_str() {
                paths.insert(item.path.clone());
            }
        }
        paths.into_iter().collect()
    }

    fn into_items(self, limit: usize) -> Vec<ImpactItem> {
        let mut items = self.items.into_values().collect::<Vec<_>>();
        items.sort_by_key(|item| {
            (
                category_rank(&item.category),
                reason_rank(&item.reason),
                item.path.clone(),
                item.symbol.clone().unwrap_or_default(),
            )
        });
        items.truncate(limit);
        items
    }
}
