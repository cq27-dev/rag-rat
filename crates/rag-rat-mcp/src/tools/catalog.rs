use super::*;

pub const TOOL_NAMES: &[&str] = &[
    "semantic_search",
    "symbol_lookup",
    "find_callers",
    "trace_callees",
    "compare_graph_to_text",
    "compare_graph_to_scip",
    "impact_surface",
    "check_library_usage",
    "repo_brief",
    "repo_clusters",
    "important_symbols",
    "find_clones",
    "clones_for_symbol",
    "ffi_surface",
    "docs_for_symbol",
    "read_chunk",
    "commit_search",
    "git_history_for_path",
    "git_history_for_symbol",
    "commits_touching_query",
    "git_blame_chunk",
    "papertrail_for_chunk",
    "papertrail_for_symbol",
    "papertrail_for_commit",
    "papertrail_issue_search",
    "papertrail_refs_for_path",
    "rationale_search",
    "llm_status",
    "heal_index",
    "papertrail_sync_status",
    "index_status",
    "memory_create",
    "memory_rebind",
    "memory_update",
    "memory_search",
    "memory_for_symbol",
    "memory_for_path",
    "memory_for_call_path",
    "memory_show",
    "memory_validate",
    "memory_doctor",
    "memory_mark_obsolete",
    "memory_edge_add",
    "memory_edge_remove",
    "memory_edges",
    "dream",
    "dream_review",
];

pub fn list_tools() -> Value {
    json!(
        TOOL_NAMES
            .iter()
            .map(|name| json!({
                "name": name,
                "description": description(name),
                "inputSchema": schema(name)
            }))
            .collect::<Vec<_>>()
    )
}

pub fn description(name: &str) -> &'static str {
    match name {
        "semantic_search" =>
            "Search indexed source and docs. `score` is a blended relevance score combining BM25 \
             lexical rank and (when an embedding model is installed) vector cosine similarity; \
             pass explain=true for the per-component breakdown. Each hit carries `retrieval_mode` \
             ('lexical', 'vector', or 'hybrid') so you can tell whether embeddings contributed \
             without explain. Hits are validated against current source. Falls back to BM25-only \
             (every hit 'lexical') when no embedding model is present. When a hit's symbol has a \
             distilled decision record (the model's resolved root-cause / decision / outcome over \
             the tracker thread that shaped it), it rides along as `distilled_records` — labeled \
             unreviewed, capped at 2; empty for almost every hit.",
        "symbol_lookup" =>
            "Resolve a symbol name (or ref/id) to its definition(s) in Rust, TypeScript, Kotlin, \
             C, C++, Python, or Swift — exact or fuzzy. Returns candidates with signatures, \
             locations, logical-symbol grouping (cfg variants), and any bound repo memories. Use \
             to disambiguate before a graph or read call. Generated bindings (codegen, ubrn FFI \
             output) are excluded by default; pass include: [\"generated\"] to see them. A \
             candidate whose symbol has distilled decision records carries them as \
             `distilled_records` (labeled unreviewed, capped at 2; empty for almost every symbol).",
        "find_callers" =>
            "Find what calls a symbol (reverse call graph), instead of grepping for call sites. \
             Returns call sites with confidence + target verification, a completeness / \
             false-positive risk summary, and repo memories crossing the call path. Includes \
             synthesized `dispatches` edges for message/enum (actor-channel) dispatch — the sender \
             that constructs the variant a handler's match arm handles. Resolve the symbol with \
             symbol_lookup first when a name is ambiguous.",
        "trace_callees" =>
            "Find what a symbol calls (forward call graph). Same evidence shape as find_callers; \
             unresolved std/common-method noise is filtered out by default (add `common_methods` / \
             `unresolved` to the `include` array to keep it).",
        "compare_graph_to_text" =>
            "Cross-check a symbol's graph caller edges against a regex text search of indexed \
             source — surfaces call sites the tree-sitter graph missed and flags likely false \
             edges. Use when you suspect graph coverage gaps.",
        "compare_graph_to_scip" =>
            "Cross-check the tree-sitter graph against the SCIP compiler oracle — report the edges \
             where they DISAGREE on a callee's resolution (the compiler contradicts tree-sitter). \
             A resolver-debugging diagnostic; requires `rag-rat oracle run` first to populate \
             compiler verdicts. Reports nothing when no oracle data exists for this checkout.",
        "impact_surface" =>
            "Pre-edit blast radius for a symbol or path: graph callers/callees, tests, docs, git \
             history, tracker papertrail, and the repo memories crossing it, with a completeness / \
             risk summary. Run this before changing anything non-trivial. Distilled decision \
             records for the symbol ride along as `distilled_records` (labeled unreviewed, capped \
             at 2; the cap is signalled in `completeness_and_caveats.truncated_sections` when more \
             exist).",
        "check_library_usage" =>
            "Dependency-contract check for the code's EXTERNAL library calls, from the SCIP \
             oracle's external symbol info. For each `resolved-external` call site it surfaces the \
             dependency's CURRENT signature + docs as inline context (judge arity / misuse \
             yourself) and ASSERTS a `deprecated` verdict when the docs mark it so. Filter by \
             `path`, `package`, or `deprecated_only`. Requires an `oracle run`; returns a \
             `NoOracleRun` / `NoExternalSymbols` status otherwise. Does NOT assert arity or \
             removed/renamed drift (not instrumented / needs a cross-version baseline) — those \
             stay context.",
        "repo_brief" =>
            "Orientation for an unfamiliar repo: ranked files by mode — spine (central coupling), \
             churn, god_modules, or refactor_candidates — with size/coupling/churn/memory signals \
             and suggested next tools. Start here when you don't know the codebase.",
        "repo_clusters" =>
            "Map the repo into ownership clusters from path proximity, graph edges, and git \
             co-touch — a cheap overview of subsystems and their representative files.",
        "important_symbols" =>
            "Rank the most load-bearing symbols by weighted PageRank over the call/type/import \
             edge graph — what the rest of the code most depends on. Run before editing to see the \
             spine you shouldn't reinvent or break. By DEFAULT (no `personalize`) it auto-seeds \
             from your current git diff, returning importance relative to your current changes; \
             pass `personalize` (names, refs, or `sym_<hex>` handles you're working on) to seed it \
             explicitly, or a single `\"global\"` to force whole-repo PageRank. The result is a \
             labeled object: `mode` (which scale), `seed_source` (seed provenance), and `symbols`.",
        "find_clones" =>
            "Ranked candidate clone classes (unrefined; exact overlap metrics). Returns classes \
             sorted by ROI (cross-module spread × member count × token length × load-bearing \
             factor × cohesion), with a completeness provenance block. `min_similarity` (if set) \
             must be in [0.5, 1.0] (default 0.7). A LIMITED query (`limit: N`) is capped at the \
             refine budget (currently 50) — it returns at most 50 classes, all refined; pass \
             `limit: null`/omit it to retrieve all classes (only the top 50 refined). \
             `completeness.refine_budget_clamped` is true when a supplied limit hit that cap.",
        "clones_for_symbol" =>
            "The clone class containing a symbol (by id / ref / path+line). Returns the candidate \
             class if the symbol is fingerprinted and has clone siblings; null if it is unique or \
             not fingerprinted.",
        "ffi_surface" =>
            "Find the FFI surface: #[uniffi::export] items, exported impl members, and generated \
             binding artifacts (detected by path). Empty in repos without FFI.",
        "docs_for_symbol" =>
            "Find documentation related to a symbol — markdown chunks and doc comments, preferring \
             local context before broad docs.",
        "read_chunk" =>
            "Read the current source text for one chunk id, validated against HEAD (relocates or \
             flags stale/gone), with compact call-graph context and bound repo memories. Use to \
             read exact text after a search returns a chunk_id. When the chunk's symbol has \
             distilled decision records they attach as `distilled_records` (labeled unreviewed, \
             capped at 2).",
        "commit_search" =>
            "Full-text search over historical commit subjects and bodies — find when/why something \
             changed by keyword.",
        "git_history_for_path" =>
            "List commits that touched a current path, newest first, with additions/deletions and \
             subjects.",
        "git_history_for_symbol" =>
            "Resolve a symbol, then list commits touching its file — symbol-scoped history without \
             needing the path.",
        "commits_touching_query" =>
            "Combine commit-message matches with current file-change evidence for a query — \"what \
             work relates to X?\" across both messages and the files that changed.",
        "git_blame_chunk" =>
            "Hash-bound git blame for one chunk: who last touched its lines, computed lazily and \
             cached against the chunk hash.",
        "papertrail_for_chunk" =>
            "The 'why' behind a chunk: its current text plus the cached tracker items (issues / \
             change requests) and review comments that reference it.",
        "papertrail_for_symbol" =>
            "Resolve a symbol, then return its current context plus the cached tracker rationale \
             (issues / change requests / reviews) referencing it.",
        "papertrail_for_commit" =>
            "Cached tracker items (issues / change requests / reviews) related to a historical \
             commit.",
        "papertrail_issue_search" =>
            "Full-text search across cached tracker issue and change-request titles and bodies. A \
             hit whose thread has a distilled decision record carries it as `record` (the model's \
             root-cause / decision / outcome over the whole thread); coalesced issue↔PR pairs \
             answer as one result.",
        "papertrail_refs_for_path" =>
            "List cached tracker items discovered to reference a current path.",
        "rationale_search" =>
            "Search cached tracker rationale snippets (review comments, issue / change-request \
             discussion) by keyword. A hit whose thread has a distilled decision record carries it \
             as `record` (the model's root-cause / decision / outcome over the whole thread).",
        "llm_status" =>
            "Embedding status (local or remote/Ollama-served): model, install state, and how many \
             chunks are embedded / missing / skipped.",
        "heal_index" =>
            "Re-index stale already-indexed files and refresh FTS — repair when reads report \
             drift. Writes only to the index, never to source.",
        "papertrail_sync_status" =>
            "Papertrail cache status: counts of issues, change requests, comments, and refs, plus \
             last sync time.",
        "index_status" =>
            "Index freshness vs HEAD: git/indexed head, per-language file counts, parser failures, \
             FTS sync state, and schema version.",
        "memory_create" =>
            "Record a durable, source-anchored repo memory (Invariant / Decision / Risk / \
             BugPattern / …) bound to a symbol, chunk, path, edge/call-path, commit, or tracker \
             ref — so the rationale resurfaces for the next agent editing that code. Capture \
             non-obvious invariants and decisions as you discover them.",
        "memory_rebind" =>
            "Re-anchor an existing repo memory to a different symbol, chunk, path, or other source \
             location — use this after a symbol moves or is renamed rather than obsoleting and \
             recreating the memory. Replaces the binding and refreshes the source_text_hash so the \
             memory stays current.",
        "memory_update" => "Update a repo memory's text, status, confidence, kind, or tags by id.",
        "memory_search" => "Full-text search across active (or stale) repo memories by keyword.",
        "memory_for_symbol" =>
            "Return repo memories bound to a symbol (or its logical-symbol group).",
        "memory_for_path" => "Return repo memories bound to a path.",
        "memory_for_call_path" =>
            "Return repo memories bound to a specific call-path edge sequence.",
        "memory_show" =>
            "Expand ONE repo memory to its FULL body by `memory_id` — the expand path for a \
             compact summary. When `[memory] surface = \"summary\"` renders drive-by attachments \
             (e.g. impact_surface) as the dream-compacted summary, call this with the attachment's \
             `memory_id` to get the complete original. Surface-independent: always the full body.",
        "memory_validate" =>
            "Re-anchor every repo memory against current source and mark each current / relocated \
             / stale / gone / pending. Runs automatically after indexing.",
        "memory_doctor" =>
            "List repo memories whose anchor is stale, gone, or pending, each with suggested \
             re-anchor targets (qualified names) — the actionable companion to memory_validate. A \
             `pending` anchor is alive on an in-flight worktree branch: informational only — do \
             NOT rebind or mark it obsolete; it re-anchors when that branch lands. Read-only; \
             reports the last-validated status, so run memory_validate first for a fresh check. \
             Rebind stale/gone entries with memory_rebind.",
        "memory_mark_obsolete" =>
            "Mark a repo memory obsolete — kept for audit, hidden from active recall.",
        "memory_edge_add" =>
            "Add a typed graph edge from a source node to another node or a GitHub issue. \
             Relations: depends_on (task DAG), relates_to (mind-map link), supersedes, \
             derived_from, tracks (issue <- task). Give exactly ONE target: a target_node_id (with \
             an optional target_repo_id for a cross-repo edge) or a full github ref (owner + repo \
             + number).",
        "memory_edge_remove" => "Remove a graph edge by its stable edge_key.",
        "memory_edges" =>
            "List a node's typed edges: direction=from returns its outgoing edges (deps / links / \
             tracks); direction=into is the reverse traversal (e.g. tasks that track an issue, or \
             nodes that depend on this one).",
        "dream" =>
            "Return the deterministic memory-maintenance worklist: coverage gaps (load-bearing \
             symbols with no memory) + stale references (a memory citing a path that no longer \
             resolves), ranked, each with a stable `id` to review. This is the pull surface for a \
             strong agent to burn down the worklist. Recomputes the deterministic findings on each \
             call (like `rag-rat dream`); it does NOT run the opt-in model verdict/compaction \
             passes — those stay on the CLI/cron `rag-rat dream --verify|--compact`, and the \
             findings they persist (e.g. memory_divergence) still surface here. Review a finding \
             with dream_review.",
        "dream_review" =>
            "Apply a human verdict to ONE dream finding by id (a full id or unambiguous prefix): \
             accept (a real gap to act on), dismiss (noise), or reset (clear a prior verdict, back \
             to open). Dream only proposes; this is how the reviewer confirms. The verdict \
             survives future dream runs. Mirrors `rag-rat dream <id> --accept|--dismiss|--reset`.",
        _ => "Unknown tool.",
    }
}

/// The `worktree` request field every worktree-scoped read honors, declared ONCE here rather than
/// as a field on each arg struct: the dispatcher reads it as a COMMON field off the raw request
/// (`worktree_arg`), so N per-struct copies would be N chances to drift from it.
/// The silent-fallback warning is load-bearing: `resolve_worktree_scope` drops a path that isn't a
/// linked worktree of this repo to the base scope without an error, so a typo reads as a plausible
/// answer about the wrong checkout — the exact failure the parameter exists to prevent.
const WORKTREE_PARAM_DESCRIPTION: &str =
    "Absolute path of the checkout to scope reads to — pass a linked worktree to read its branch \
     overlay. Defaults to the server's working directory. A path that is not a linked worktree of \
     this repo is silently ignored: results then come from the indexed checkout, with no error.";

pub fn schema(name: &str) -> Value {
    let mut schema = arg_schema(name);
    // Only advertise the parameter on the tools that actually honor it: a write tool and
    // `compare_graph_to_text` stay base-scoped by contract, so declaring it there would promise a
    // scoping the dispatcher deliberately ignores.
    if tool_honors_worktree_scope(name) {
        declare_worktree_property(&mut schema);
    }
    schema
}

/// Add the optional `worktree` property to a generated arg schema. Optional = absent from
/// `required`, which the generated schema never lists it in. The insert OVERWRITES any property an
/// arg struct generates under the same name, so an arg struct that grows a `worktree` field of its
/// own cannot leave a divergent spec in the advertised schema.
fn declare_worktree_property(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else { return };
    // An `EmptyArgs` tool's generated schema carries no `properties` map at all.
    let properties = object.entry("properties").or_insert_with(|| json!({}));
    if let Some(properties) = properties.as_object_mut() {
        properties.insert(
            "worktree".to_string(),
            json!({"type": "string", "description": WORKTREE_PARAM_DESCRIPTION}),
        );
    }
}

fn arg_schema(name: &str) -> Value {
    match name {
        "semantic_search"
        | "commit_search"
        | "commits_touching_query"
        | "papertrail_issue_search"
        | "rationale_search" => schema_for::<SearchArgs>(),
        "symbol_lookup" => schema_for::<SymbolArgs>(),
        // Pure selector args, no `include` — these resolve via select_symbol (source-only) and
        // don't honor a generated opt-in, so they must not advertise one (#202 review).
        "git_history_for_symbol" | "papertrail_for_symbol" => schema_for::<SymbolRefArgs>(),
        "find_callers" | "trace_callees" | "docs_for_symbol" => schema_for::<SymbolGraphArgs>(),
        "compare_graph_to_text" => schema_for::<CompareGraphTextArgs>(),
        "compare_graph_to_scip" => schema_for::<EmptyArgs>(),
        "impact_surface" => schema_for::<ImpactArgs>(),
        "check_library_usage" => schema_for::<CheckLibraryUsageArgs>(),
        "repo_brief" => schema_for::<RepoBriefArgs>(),
        "repo_clusters" => schema_for::<RepoClustersArgs>(),
        "important_symbols" => schema_for::<ImportantSymbolsArgs>(),
        "find_clones" => schema_for::<FindClonesArgs>(),
        "clones_for_symbol" => schema_for::<ClonesForSymbolArgs>(),
        "ffi_surface" => schema_for::<LimitArgs>(),
        "read_chunk" => schema_for::<ReadChunkArgs>(),
        "git_history_for_path" | "papertrail_refs_for_path" => schema_for::<PathHistoryArgs>(),
        "git_blame_chunk" => schema_for::<BlameChunkArgs>(),
        "papertrail_for_chunk" => schema_for::<PapertrailChunkArgs>(),
        "papertrail_for_commit" => schema_for::<PapertrailCommitArgs>(),
        "heal_index" => schema_for::<HealIndexArgs>(),
        "memory_create" => schema_for::<MemoryCreateArgs>(),
        "memory_rebind" => schema_for::<MemoryRebindArgs>(),
        "memory_update" => schema_for::<MemoryUpdateArgs>(),
        "memory_search" => schema_for::<MemorySearchArgs>(),
        "memory_for_symbol" => schema_for::<MemoryForSymbolArgs>(),
        "memory_for_path" => schema_for::<MemoryForPathArgs>(),
        "memory_for_call_path" => schema_for::<MemoryForCallPathArgs>(),
        "memory_mark_obsolete" | "memory_show" => schema_for::<MemoryIdArgs>(),
        "llm_status"
        | "papertrail_sync_status"
        | "index_status"
        | "memory_validate"
        | "memory_doctor" => schema_for::<EmptyArgs>(),
        "memory_edge_add" => schema_for::<MemoryEdgeAddArgs>(),
        "memory_edge_remove" => schema_for::<MemoryEdgeRemoveArgs>(),
        "memory_edges" => schema_for::<MemoryEdgesArgs>(),
        "dream" => schema_for::<DreamArgs>(),
        "dream_review" => schema_for::<DreamReviewArgs>(),
        _ => json!({"type": "object"}),
    }
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    /// The tools that must NOT advertise `worktree`: every write tool (writing under a linked
    /// overlay scope would reindex the overlay with the main checkout's contents) plus the one read
    /// tool that compares the graph against live main-checkout text. Spelled out rather than
    /// derived from `tool_honors_worktree_scope`, which `schema` is implemented in terms of — a
    /// derived expectation would agree with the code by construction and guard nothing.
    const BASE_SCOPED_TOOLS: &[&str] = &[
        "heal_index",
        "memory_create",
        "memory_rebind",
        "memory_update",
        "memory_edge_add",
        "memory_edge_remove",
        "memory_mark_obsolete",
        "memory_validate",
        "dream",
        "dream_review",
        "compare_graph_to_text",
    ];

    fn worktree_property(name: &str) -> Option<Value> {
        schema(name).get("properties")?.get("worktree").cloned()
    }

    #[test]
    fn every_read_tool_declares_the_worktree_param_and_the_base_scoped_ones_do_not() {
        // Per-request worktree scoping is unusable if no schema mentions it: serde tolerates the
        // field (no `deny_unknown_fields`), but a client can only pass what the catalog declares.
        for name in TOOL_NAMES {
            if BASE_SCOPED_TOOLS.contains(name) {
                assert!(
                    worktree_property(name).is_none(),
                    "{name}: base-scoped by contract — advertising `worktree` would promise a \
                     scoping the dispatcher discards",
                );
                continue;
            }
            // Covers the reads whose checkout-dependence isn't obvious from the name:
            // `index_status`'s per-language counts, `llm_status`'s chunk counts and
            // `memory_doctor`'s re-anchor candidates all read the per-connection `files` view.
            // `index_status` is also the `EmptyArgs` case, whose generated schema carries no
            // `properties` map of its own.
            let declared =
                worktree_property(name).unwrap_or_else(|| panic!("{name} declares `worktree`"));
            assert_eq!(
                declared,
                json!({"type": "string", "description": WORKTREE_PARAM_DESCRIPTION}),
                "{name}: every read tool advertises the ONE canonical spec — an arg struct that \
                 also carries the field must not leave a divergent one behind",
            );
            assert!(
                !schema(name)["required"]
                    .as_array()
                    .is_some_and(|required| required.iter().any(|field| field == "worktree")),
                "{name}: `worktree` is optional — it must never be listed as required",
            );
        }
    }
}
