use super::*;

pub(crate) fn call_tool_with_db(
    db: &IndexDatabase,
    name: &str,
    arguments: Value,
    graded_history: bool,
) -> anyhow::Result<Value> {
    let result = match name {
        "semantic_search" => {
            let args: SearchArgs = serde_json::from_value(arguments)?;
            json!(db.search_with_graph_meta(rag_rat_core::index::SearchRequest {
                query: &args.query,
                limit: args.limit,
                include_generated: included(&args.include, SearchInclude::Generated, false),
                explain: args.explain,
                graph_mode: GraphMetaMode::parse(args.include_graph.as_str())?,
                graph_limit: args.graph_limit,
                options: SearchOptions {
                    include_git: included(&args.include, SearchInclude::Git, true),
                    include_papertrail: included(&args.include, SearchInclude::Papertrail, true),
                    // Sourced from `[search] graded_git_rerank` (default false) by the caller;
                    // every other tool ignores it. OFF → the semantic_search
                    // fuse is byte-identical.
                    graded_history,
                },
            })?)
        },
        "symbol_lookup" => {
            let args: SymbolArgs = serde_json::from_value(arguments)?;
            symbol_lookup_tool(db, args)?
        },
        "find_callers" => {
            let args: SymbolGraphArgs = serde_json::from_value(arguments)?;
            let resolution_mode = resolution_mode(args.resolution);
            graph_tool(db, args, resolution_mode, true)?
        },
        "trace_callees" => {
            let args: SymbolGraphArgs = serde_json::from_value(arguments)?;
            let resolution_mode = resolution_mode(args.resolution);
            graph_tool(db, args, resolution_mode, false)?
        },
        "compare_graph_to_text" => {
            let args: CompareGraphTextArgs = serde_json::from_value(arguments)?;
            let resolution_mode = resolution_mode(args.resolution);
            compare_graph_to_text_tool(db, args, resolution_mode)?
        },
        "compare_graph_to_scip" => json!(db.compare_graph_to_scip()?),
        "impact_surface" => {
            let args: ImpactArgs = serde_json::from_value(arguments)?;
            let resolution_mode = resolution_mode(args.resolution);
            impact_tool(db, args, resolution_mode)?
        },
        "repo_brief" => {
            let args: RepoBriefArgs = serde_json::from_value(arguments)?;
            json!(db.repo_brief(RepoBriefOptions {
                mode: args.mode.core(),
                limit: args.limit,
                include_generated: included(&args.include, OrientationInclude::Generated, false),
                include_memories: included(&args.include, OrientationInclude::Memories, true),
            })?)
        },
        "repo_clusters" => {
            let args: RepoClustersArgs = serde_json::from_value(arguments)?;
            json!(db.repo_clusters(RepoClustersOptions {
                limit: args.limit,
                include_generated: included(&args.include, OrientationInclude::Generated, false),
                include_memories: included(&args.include, OrientationInclude::Memories, true),
                min_cluster_size: args.min_cluster_size,
            })?)
        },
        "important_symbols" => {
            let args: ImportantSymbolsArgs = serde_json::from_value(arguments)?;
            json!(important_symbols_tool(db, args)?)
        },
        "ffi_surface" => {
            let args: LimitArgs = serde_json::from_value(arguments)?;
            json!(db.ffi_surface(args.limit)?)
        },
        "docs_for_symbol" => {
            let args: SymbolGraphArgs = serde_json::from_value(arguments)?;
            docs_for_symbol_tool(db, args)?
        },
        "read_chunk" => {
            let args: ReadChunkArgs = serde_json::from_value(arguments)?;
            json!(db.read_chunk_with_graph_and_memories(
                args.chunk_id,
                GraphMetaMode::parse(args.include_graph.as_str())?,
                args.graph_limit,
                included(&args.include, MemoriesInclude::Memories, true)
            )?)
        },
        "commit_search" => {
            let args: SearchArgs = serde_json::from_value(arguments)?;
            json!(db.commit_search(&args.query, args.limit)?)
        },
        "git_history_for_path" => {
            let args: PathHistoryArgs = serde_json::from_value(arguments)?;
            json!(db.git_history_for_path(&args.path, args.limit)?)
        },
        "git_history_for_symbol" => {
            let args: SymbolRefArgs = serde_json::from_value(arguments)?;
            git_history_for_symbol_tool(db, args)?
        },
        "commits_touching_query" => {
            let args: SearchArgs = serde_json::from_value(arguments)?;
            json!(db.commits_touching_query(&args.query, args.limit)?)
        },
        "git_blame_chunk" => {
            let args: BlameChunkArgs = serde_json::from_value(arguments)?;
            json!(db.git_blame_chunk(args.chunk_id)?)
        },
        "papertrail_for_chunk" => {
            let args: PapertrailChunkArgs = serde_json::from_value(arguments)?;
            json!(db.papertrail_for_chunk(args.chunk_id, args.limit)?)
        },
        "papertrail_for_symbol" => {
            let args: SymbolRefArgs = serde_json::from_value(arguments)?;
            papertrail_for_symbol_tool(db, args)?
        },
        "papertrail_for_commit" => {
            let args: PapertrailCommitArgs = serde_json::from_value(arguments)?;
            let mut value = json!(db.papertrail_for_commit(&args.commit_hash, args.limit)?);
            if !included(&args.include, PapertrailCommitInclude::Fallback, false) {
                strip_fallback_github_evidence(&mut value);
            }
            value
        },
        "github_issue_search" => {
            let args: SearchArgs = serde_json::from_value(arguments)?;
            json!(db.github_issue_search(&args.query, args.limit)?)
        },
        "github_refs_for_path" => {
            let args: PathHistoryArgs = serde_json::from_value(arguments)?;
            json!(db.github_refs_for_path(&args.path, args.limit)?)
        },
        "rationale_search" => {
            let args: SearchArgs = serde_json::from_value(arguments)?;
            let mut value = json!(db.rationale_search(&args.query, args.limit)?);
            if !included(&args.include, SearchInclude::Fallback, false) {
                keep_literal_github_refs_if_present(&mut value);
            }
            value
        },
        "local_ai_status" => {
            let mut value = json!(db.local_ai_status()?);
            // `fastembed` re-states the same counts as `embedding` plus backend diagnostics; keep
            // the canonical `embedding` capability/state block and the `artifacts` coverage block.
            remove_object_key(&mut value, "fastembed");
            value
        },
        "heal_index" => {
            let args: HealIndexArgs = serde_json::from_value(arguments)?;
            json!(db.heal_index(args.limit)?)
        },
        "github_sync_status" => json!(db.github_sync_status()?),
        "index_status" => {
            let mut value = json!(db.status(db.database_path())?);
            // The full migration ledger is static detail (use the CLI `doctor`/`migrate` for it),
            // and the embedded llm block duplicates the `local_ai_status` tool.
            if let Some(schema) = value.get_mut("schema") {
                remove_object_key(schema, "migrations");
            }
            remove_object_key(&mut value, "llm");
            value
        },
        "memory_create" => {
            let args: MemoryCreateArgs = serde_json::from_value(arguments)?;
            json!(db.memory_create(args.core())?)
        },
        "memory_rebind" => {
            let args: MemoryRebindArgs = serde_json::from_value(arguments)?;
            json!(db.memory_rebind(&args.memory_id, args.bind.into())?)
        },
        "memory_update" => {
            let args: MemoryUpdateArgs = serde_json::from_value(arguments)?;
            json!(db.memory_update(args.core())?)
        },
        "memory_search" => {
            let args: MemorySearchArgs = serde_json::from_value(arguments)?;
            json!(db.memory_search(&args.query, args.limit)?)
        },
        "memory_for_symbol" => {
            let args: MemoryForSymbolArgs = serde_json::from_value(arguments)?;
            memory_for_symbol_tool(db, args)?
        },
        "memory_for_path" => {
            let args: MemoryForPathArgs = serde_json::from_value(arguments)?;
            json!(db.memory_for_path(&args.path, args.limit)?)
        },
        "memory_for_call_path" => {
            let args: MemoryForCallPathArgs = serde_json::from_value(arguments)?;
            json!(db.memory_for_call_path_hash(&args.edge_sequence_hash, args.limit)?)
        },
        "memory_validate" => json!(db.memory_validate()?),
        "memory_doctor" => json!(db.memory_doctor()?),
        "memory_mark_obsolete" => {
            let args: MemoryIdArgs = serde_json::from_value(arguments)?;
            json!(db.memory_mark_obsolete(&args.memory_id)?)
        },
        "find_clones" => {
            let args: FindClonesArgs = serde_json::from_value(arguments)?;
            find_clones_tool(db, args)?
        },
        "clones_for_symbol" => {
            let args: ClonesForSymbolArgs = serde_json::from_value(arguments)?;
            clones_for_symbol_tool(db, args)?
        },
        other => anyhow::bail!("unknown tool `{other}`"),
    };
    Ok(result)
}

pub(crate) fn symbol_lookup_tool(db: &IndexDatabase, args: SymbolArgs) -> anyhow::Result<Value> {
    let include_memories = included(&args.include, SymbolInclude::Memories, true);
    let include_generated = included(&args.include, SymbolInclude::Generated, false);
    let lookup = db.symbol_candidates(&symbol_selector(args)?, include_generated)?;
    let mut value = json!(lookup);
    if !include_memories {
        return Ok(value);
    }
    let Some(candidates) = value.get_mut("candidates").and_then(Value::as_array_mut) else {
        return Ok(value);
    };
    // Resolve memories from the in-memory hits, which still carry the internal `symbol_id`. The
    // serialized candidates no longer expose it (#149), so the old read of `candidate["symbol_id"]`
    // found nothing and dropped every memory — zip by position against the source hits instead.
    for (candidate, hit) in candidates.iter_mut().zip(&lookup.candidates) {
        let memories = db.memory_for_symbol(hit, 10)?;
        if !memories.is_empty() {
            candidate["memories"] = json!(memories);
        }
    }
    Ok(value)
}

pub(crate) fn graph_tool(
    db: &IndexDatabase,
    args: SymbolGraphArgs,
    resolution_mode: GraphResolutionMode,
    reverse: bool,
) -> anyhow::Result<Value> {
    let limit = args.limit;
    let include_references = included(&args.include, GraphInclude::References, false);
    let include_unresolved = included(&args.include, GraphInclude::Unresolved, false);
    let include_macros = included(&args.include, GraphInclude::Macros, false);
    let include_common_methods = included(&args.include, GraphInclude::CommonMethods, false);
    let include_coverage = included(&args.include, GraphInclude::Coverage, false);
    let include_memories = included(&args.include, GraphInclude::Memories, true);
    let edge_kinds = graph_edge_kinds(args.edge_kinds.as_deref());
    let allow_ambiguous = args.allow_ambiguous;
    let selector = graph_symbol_selector(&args)?;
    let selected = db.select_symbol(&selector)?;
    match selected {
        Ok(Some(symbol)) => {
            let options = GraphTraversalOptions {
                include_references,
                include_unresolved,
                include_macros,
                include_common_methods,
                edge_kinds,
                resolution_mode,
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: args.logical_symbol_id,
            };
            let mut value = json!(db.graph_traversal_report(
                if reverse { "find_callers" } else { "trace_callees" },
                &symbol,
                reverse,
                limit,
                &options
            )?);
            compact_graph_coverage(&mut value, include_coverage);
            if include_memories {
                let edge_ids = value["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|hop| hop.get("edge_id").and_then(Value::as_i64))
                    .collect::<Vec<_>>();
                // find_callers crosses caller edges (X -> symbol); trace_callees crosses callee
                // edges (symbol -> X). Pass the correct side so call-path hashes line up (#38).
                let (caller_edge_ids, callee_edge_ids): (&[i64], &[i64]) =
                    if reverse { (&edge_ids, &[]) } else { (&[], &edge_ids) };
                value["repo_memories"] = json!(db.memory_evidence_for_symbol_and_edges(
                    &symbol,
                    caller_edge_ids,
                    callee_edge_ids,
                    10
                )?);
            }
            Ok(value)
        },
        Ok(None) if allow_ambiguous => {
            let Some(symbol) = args.symbol.as_deref() else {
                return Ok(Value::Null);
            };
            let options = GraphTraversalOptions {
                include_references,
                include_unresolved,
                include_macros,
                include_common_methods,
                edge_kinds,
                resolution_mode,
                symbol_id: None,
                logical_symbol_id: args.logical_symbol_id,
            };
            let hops = if reverse {
                db.find_callers_with_options(symbol, limit, &options)?
            } else {
                db.trace_callees_with_options(symbol, limit, &options)?
            };
            Ok(json!(hops))
        },
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

pub(crate) fn docs_for_symbol_tool(
    db: &IndexDatabase,
    args: SymbolGraphArgs,
) -> anyhow::Result<Value> {
    let selector = graph_symbol_selector(&args)?;
    match db.select_symbol(&selector)? {
        Ok(Some(symbol)) => Ok(json!(db.docs_for_selected_symbol(&symbol, args.limit)?)),
        Ok(None) if args.allow_ambiguous => {
            let Some(symbol) = args.symbol.as_deref() else {
                return Ok(Value::Null);
            };
            Ok(json!(db.docs_for_symbol(symbol, args.limit)?))
        },
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

pub(crate) fn compare_graph_to_text_tool(
    db: &IndexDatabase,
    args: CompareGraphTextArgs,
    resolution_mode: GraphResolutionMode,
) -> anyhow::Result<Value> {
    let selector = SymbolSelector {
        logical_symbol_id: args.logical_symbol_id,
        symbol_id: None,
        symbol_path: args.symbol_path,
        symbol: args.symbol,
        language: None,
        allow_ambiguous: args.allow_ambiguous,
        limit: args.limit,
    };
    match db.select_symbol(&selector)? {
        Ok(Some(symbol)) => {
            let options = GraphTraversalOptions {
                include_references: included(&args.include, CompareInclude::References, false),
                include_unresolved: included(&args.include, CompareInclude::Unresolved, false),
                include_macros: included(&args.include, CompareInclude::Macros, false),
                include_common_methods: included(
                    &args.include,
                    CompareInclude::CommonMethods,
                    false,
                ),
                edge_kinds: graph_edge_kinds(args.edge_kinds.as_deref()),
                resolution_mode,
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: args.logical_symbol_id,
            };
            Ok(json!(db.compare_graph_to_text(
                &symbol,
                &args.pattern,
                args.limit,
                &options,
                included(&args.include, CompareInclude::Tests, true)
            )?))
        },
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

pub(crate) fn strip_fallback_github_evidence(value: &mut Value) {
    if let Value::Object(map) = value {
        map.remove("fallback_github_evidence");
    }
}

/// Drop a top-level object key from a JSON value, if present. Used to trim verbose/redundant
/// sub-blocks from status tool output without touching the shared core status structs.
pub(crate) fn remove_object_key(value: &mut Value, key: &str) {
    if let Value::Object(map) = value {
        map.remove(key);
    }
}

pub(crate) fn keep_literal_github_refs_if_present(value: &mut Value) {
    let Value::Array(items) = value else {
        return;
    };
    let literal_items = items
        .iter()
        .filter(|item| {
            item.get("evidence_kind").and_then(Value::as_str) == Some("literal_github_ref")
        })
        .cloned()
        .collect::<Vec<_>>();
    if !literal_items.is_empty() {
        *items = literal_items;
    }
}

pub(crate) fn compact_graph_coverage(value: &mut Value, include_coverage: bool) {
    let Some(report) = value.as_object_mut() else {
        return;
    };
    let (parser_failures, stale_files, known_gaps) = report
        .get("coverage")
        .and_then(Value::as_object)
        .map(|coverage| {
            (
                coverage.get("parser_failures").and_then(Value::as_u64).unwrap_or_default(),
                coverage.get("stale_files").and_then(Value::as_u64).unwrap_or_default(),
                coverage.get("known_index_gaps").and_then(Value::as_array).map_or(0, Vec::len),
            )
        })
        .unwrap_or_default();

    // A stale or partially-parsed index can hide caller/callee edges entirely, so a 0-result
    // must not read as confident. Never report `low` completeness when coverage is degraded
    // (issue #47: a stale index produced "0 callers, completeness_risk: low").
    if (stale_files > 0 || parser_failures > 0 || known_gaps > 0)
        && let Some(risk) = report
            .get_mut("summary")
            .and_then(Value::as_object_mut)
            .and_then(|summary| summary.get_mut("completeness_risk"))
        && risk.as_str() == Some("low")
    {
        *risk = Value::String("medium".to_string());
    }

    if include_coverage {
        return;
    }
    if report.remove("coverage").is_none() {
        return;
    }
    let mut warnings = Vec::new();
    if parser_failures > 0 {
        warnings.push(Value::String(format!(
            "{parser_failures} parser failures may affect graph coverage"
        )));
    }
    if stale_files > 0 {
        warnings
            .push(Value::String(format!("{stale_files} stale files may affect graph coverage")));
    }
    if known_gaps > 0 {
        warnings.push(Value::String(format!("{known_gaps} known graph index gaps")));
    }
    if !warnings.is_empty() {
        report.insert("coverage_warnings".to_string(), Value::Array(warnings));
    }
}

pub(crate) fn git_history_for_symbol_tool(
    db: &IndexDatabase,
    args: SymbolRefArgs,
) -> anyhow::Result<Value> {
    let selector = symbol_ref_selector(args)?;
    match db.select_symbol(&selector)? {
        Ok(Some(symbol)) => Ok(json!(db.git_history_for_symbol(
            &symbol.qualified_name,
            optional_language(Some(symbol.language.clone()))?,
            selector.limit
        )?)),
        Ok(None) if selector.allow_ambiguous => {
            let Some(symbol) = selector.symbol.as_deref() else {
                return Ok(Value::Null);
            };
            Ok(json!(db.git_history_for_symbol(symbol, selector.language, selector.limit)?))
        },
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

pub(crate) fn papertrail_for_symbol_tool(
    db: &IndexDatabase,
    args: SymbolRefArgs,
) -> anyhow::Result<Value> {
    let selector = symbol_ref_selector(args)?;
    match db.select_symbol(&selector)? {
        Ok(Some(symbol)) => Ok(json!(db.papertrail_for_selected_symbol(&symbol, selector.limit)?)),
        Ok(None) if selector.allow_ambiguous => {
            let Some(symbol) = selector.symbol.as_deref() else {
                return Ok(Value::Null);
            };
            Ok(json!(db.papertrail_for_symbol(symbol, selector.language, selector.limit)?))
        },
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

pub(crate) fn impact_tool(
    db: &IndexDatabase,
    args: ImpactArgs,
    resolution_mode: GraphResolutionMode,
) -> anyhow::Result<Value> {
    let options = ImpactSurfaceOptions {
        resolution_mode,
        include_tests: included(&args.include, ImpactInclude::Tests, true),
        include_docs: included(&args.include, ImpactInclude::Docs, true),
        include_git: included(&args.include, ImpactInclude::Git, true),
        include_papertrail: included(&args.include, ImpactInclude::Papertrail, true),
        include_text_fallback: included(&args.include, ImpactInclude::TextFallback, true),
        include_memories: included(&args.include, ImpactInclude::Memories, true),
        compact_memories: !args.full_memories,
    };
    if args.logical_symbol_id.is_some() || args.symbol_path.is_some() || args.symbol.is_some() {
        let selector = SymbolSelector {
            logical_symbol_id: args.logical_symbol_id,
            symbol_id: None,
            symbol_path: args.symbol_path,
            symbol: args.symbol,
            language: None,
            allow_ambiguous: args.allow_ambiguous,
            limit: args.limit,
        };
        return match db.select_symbol(&selector)? {
            Ok(Some(symbol)) => Ok(json!(
                db.impact_surface_report_for_selected_symbol(&symbol, args.limit, &options)?
            )),
            Ok(None) if selector.allow_ambiguous => {
                let Some(symbol) = selector.symbol.as_deref() else {
                    return Ok(Value::Null);
                };
                Ok(json!(db.impact_surface_with_options(symbol, args.limit, resolution_mode)?))
            },
            Ok(None) => Ok(Value::Null),
            Err(disambiguation) => Ok(json!(disambiguation)),
        };
    }
    let Some(query) = args.query.as_deref() else {
        anyhow::bail!("impact_surface requires query, symbol_id, symbol_path, or symbol");
    };
    Ok(json!(db.impact_surface_with_options(query, args.limit, resolution_mode)?))
}

pub(crate) fn memory_for_symbol_tool(
    db: &IndexDatabase,
    args: MemoryForSymbolArgs,
) -> anyhow::Result<Value> {
    let selector = SymbolSelector {
        logical_symbol_id: args.logical_symbol_id,
        symbol_id: None,
        symbol_path: args.symbol_path,
        symbol: args.symbol,
        language: None,
        allow_ambiguous: args.allow_ambiguous,
        limit: args.limit,
    };
    match db.select_symbol(&selector)? {
        Ok(Some(symbol)) => Ok(json!(db.memory_for_symbol(&symbol, args.limit)?)),
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

/// `important_symbols` over MCP: auto-seeds from the current git diff when no `personalize` is
/// given (the headline default). A single `"global"` selector (or an all-empty list) is the
/// explicit override that forces whole-repo PageRank — distinct from "no arg", which auto-seeds.
/// Returns the labeled [`ImportantSymbolsResult`] (mode + seed provenance), not a bare array.
fn important_symbols_tool(
    db: &IndexDatabase,
    args: ImportantSymbolsArgs,
) -> anyhow::Result<rag_rat_core::query::pagerank::ImportantSymbolsResult> {
    let meaningful: Vec<String> =
        args.personalize.into_iter().filter(|entry| !entry.trim().is_empty()).collect();
    let force_global = meaningful.len() == 1 && meaningful[0].eq_ignore_ascii_case("global");
    let (personalize, auto_seed_from_diff) = if force_global {
        // Explicit global override: drop the selector, do NOT auto-seed.
        (Vec::new(), false)
    } else {
        // No explicit seed → auto-seed from the diff (MCP default). With seeds → personalize.
        let auto_seed = meaningful.is_empty();
        (meaningful, auto_seed)
    };
    db.important_symbols(rag_rat_core::index::ImportantSymbolsRequest {
        limit: args.limit as usize,
        personalize,
        auto_seed_from_diff,
    })
}

pub(crate) fn symbol_selector(args: SymbolArgs) -> anyhow::Result<SymbolSelector> {
    Ok(SymbolSelector {
        logical_symbol_id: args.logical_symbol_id,
        symbol_id: None,
        symbol_path: args.symbol_path,
        symbol: args.symbol,
        language: optional_language(args.language)?,
        allow_ambiguous: args.allow_ambiguous,
        limit: args.limit,
    })
}

pub(crate) fn symbol_ref_selector(args: SymbolRefArgs) -> anyhow::Result<SymbolSelector> {
    Ok(SymbolSelector {
        logical_symbol_id: args.logical_symbol_id,
        symbol_id: None,
        symbol_path: args.symbol_path,
        symbol: args.symbol,
        language: optional_language(args.language)?,
        allow_ambiguous: args.allow_ambiguous,
        limit: args.limit,
    })
}

pub(crate) fn resolution_mode(value: Option<McpGraphResolutionMode>) -> GraphResolutionMode {
    value.map(McpGraphResolutionMode::core).unwrap_or_default()
}

pub(crate) fn graph_edge_kinds(edge_kinds: Option<&[McpGraphEdgeKind]>) -> Option<Vec<String>> {
    edge_kinds.map(|edge_kinds| {
        edge_kinds.iter().map(|edge_kind| edge_kind.as_str().to_string()).collect()
    })
}

pub(crate) fn graph_symbol_selector(args: &SymbolGraphArgs) -> anyhow::Result<SymbolSelector> {
    Ok(SymbolSelector {
        logical_symbol_id: args.logical_symbol_id,
        symbol_id: None,
        symbol_path: args.symbol_path.clone(),
        symbol: args.symbol.clone(),
        language: None,
        allow_ambiguous: args.allow_ambiguous,
        limit: args.limit,
    })
}

pub(crate) fn find_clones_tool(db: &IndexDatabase, args: FindClonesArgs) -> anyhow::Result<Value> {
    use rag_rat_core::index::FindClonesOptions;
    let result = db.find_clones(FindClonesOptions {
        min_similarity: args.min_similarity,
        min_copies: args.min_copies,
        limit: args.limit,
    })?;
    Ok(json!(result))
}

pub(crate) fn clones_for_symbol_tool(
    db: &IndexDatabase,
    args: ClonesForSymbolArgs,
) -> anyhow::Result<Value> {
    let selector = args.into_selector()?;
    Ok(json!(db.clones_for_symbol(selector)?))
}
