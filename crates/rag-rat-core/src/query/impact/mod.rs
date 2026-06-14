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
use crate::query::memory::{self, RepoMemoryEvidence};
use crate::query::symbol::SymbolHit;

#[derive(Debug, Serialize)]
pub struct ImpactItem {
    pub path: String,
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
    pub github_rationale_issues_prs: Vec<ImpactItem>,
    pub repo_memories: RepoMemoryEvidence,
    pub completeness_and_caveats: ImpactCompleteness,
}

#[derive(Debug, Serialize)]
pub struct ImpactSurfaceQuery {
    // Internal rowid — never serialized (reindex-churned, #149); `symbol_path` identifies the
    // query.
    #[serde(skip_serializing)]
    pub symbol_id: Option<i64>,
    pub symbol_path: Option<String>,
    pub query: Option<String>,
    pub resolution: String,
    pub include_tests: bool,
    pub include_docs: bool,
    pub include_git: bool,
    pub include_papertrail: bool,
    pub include_text_fallback: bool,
    pub include_memories: bool,
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
    Ok(ImpactSurfaceReport {
        query: ImpactSurfaceQuery {
            symbol_id: Some(symbol.symbol_id),
            symbol_path: Some(symbol.qualified_name.clone()),
            query: None,
            resolution: options.resolution_mode.as_str().to_string(),
            include_tests: options.include_tests,
            include_docs: options.include_docs,
            include_git: options.include_git,
            include_papertrail: options.include_papertrail,
            include_text_fallback: options.include_text_fallback,
            include_memories: options.include_memories,
        },
        completeness_and_caveats: ImpactCompleteness {
            exact_graph_callers: u64::try_from(direct_semantic_callers.len()).unwrap_or(u64::MAX),
            graph_callees: u64::try_from(direct_semantic_callees.len()).unwrap_or(u64::MAX),
            text_fallback_hits: u64::try_from(text_fallback_hits.len()).unwrap_or(u64::MAX),
            parser_failures: parser_failure_count(conn)?,
            stale_files: 0,
            memory_status: ImpactMemoryStatus {
                active: u64::try_from(
                    repo_memories.direct.len()
                        + repo_memories.path_crossed.len()
                        + repo_memories.call_path_crossed.len(),
                )
                .unwrap_or(u64::MAX),
                stale: u64::try_from(repo_memories.stale.len()).unwrap_or(u64::MAX),
            },
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

    if surface.len() < max_items {
        let remaining = max_items.saturating_sub(surface.len());
        textual_fallback(conn, query, &mut surface, remaining)?;
    }

    let current_paths = surface.current_paths();
    historical_evidence(conn, &current_paths, query, &mut surface, max_items)?;

    Ok(surface.into_items(max_items))
}

pub fn ffi_surface(conn: &Connection, limit: u32) -> anyhow::Result<Vec<ImpactItem>> {
    let mut stmt = conn.prepare(
        "
        WITH rust_exports AS (
            SELECT DISTINCT
                   files.path AS path,
                   files.language AS language,
                   files.kind AS kind,
                   symbols.qualified_name AS symbol,
                   CASE
                       WHEN symbols.kind = 'impl' THEN 'rust_uniffi_exported_impl'
                       ELSE 'rust_uniffi_export'
                   END AS reason
            FROM symbols
            JOIN files ON files.id = symbols.file_id
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
                   members.qualified_name AS symbol,
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
