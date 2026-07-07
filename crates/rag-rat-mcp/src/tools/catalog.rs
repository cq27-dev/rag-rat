use super::*;

pub const TOOL_NAMES: &[&str] = &[
    "semantic_search",
    "symbol_lookup",
    "find_callers",
    "trace_callees",
    "compare_graph_to_text",
    "compare_graph_to_scip",
    "impact_surface",
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
    "github_issue_search",
    "github_refs_for_path",
    "rationale_search",
    "llm_status",
    "heal_index",
    "github_sync_status",
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
             (every hit 'lexical') when no embedding model is present.",
        "symbol_lookup" =>
            "Resolve a symbol name (or ref/id) to its definition(s) in Rust, TypeScript, Kotlin, \
             C, C++, or Python — exact or fuzzy. Returns candidates with signatures, locations, \
             logical-symbol grouping (cfg variants), and any bound repo memories. Use to \
             disambiguate before a graph or read call. Generated bindings (codegen, ubrn FFI \
             output) are excluded by default; pass include: [\"generated\"] to see them.",
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
             history, GitHub papertrail, and the repo memories crossing it, with a completeness / \
             risk summary. Run this before changing anything non-trivial.",
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
             read exact text after a search returns a chunk_id.",
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
            "The 'why' behind a chunk: its current text plus the cached GitHub issues/PRs/reviews \
             that reference it.",
        "papertrail_for_symbol" =>
            "Resolve a symbol, then return its current context plus the cached GitHub rationale \
             (issues/PRs/reviews) referencing it.",
        "papertrail_for_commit" =>
            "Cached GitHub issues/PRs/reviews related to a historical commit.",
        "github_issue_search" =>
            "Full-text search across cached GitHub issue and PR titles and bodies.",
        "github_refs_for_path" =>
            "List cached GitHub issues/PRs discovered to reference a current path.",
        "rationale_search" =>
            "Search cached GitHub rationale snippets (review comments, PR/issue discussion) by \
             keyword.",
        "llm_status" =>
            "Embedding status (local or remote/Ollama-served): model, install state, and how many \
             chunks are embedded / missing / skipped.",
        "heal_index" =>
            "Re-index stale already-indexed files and refresh FTS — repair when reads report \
             drift. Writes only to the index, never to source.",
        "github_sync_status" =>
            "GitHub papertrail cache status: counts of issues, PRs, comments, and refs, plus last \
             sync time.",
        "index_status" =>
            "Index freshness vs HEAD: git/indexed head, per-language file counts, parser failures, \
             FTS sync state, and schema version.",
        "memory_create" =>
            "Record a durable, source-anchored repo memory (Invariant / Decision / Risk / \
             BugPattern / …) bound to a symbol, chunk, path, edge/call-path, commit, or GitHub ref \
             — so the rationale resurfaces for the next agent editing that code. Capture \
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
             / stale / gone. Runs automatically after indexing.",
        "memory_doctor" =>
            "List repo memories whose anchor is stale or gone, each with suggested re-anchor \
             targets (qualified names) — the actionable companion to memory_validate. Read-only; \
             reports the last-validated status, so run memory_validate first for a fresh check. \
             Rebind the listed memories with memory_rebind.",
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
        _ => "Unknown tool.",
    }
}

pub fn schema(name: &str) -> Value {
    match name {
        "semantic_search"
        | "commit_search"
        | "commits_touching_query"
        | "github_issue_search"
        | "rationale_search" => schema_for::<SearchArgs>(),
        "symbol_lookup" => schema_for::<SymbolArgs>(),
        // Pure selector args, no `include` — these resolve via select_symbol (source-only) and
        // don't honor a generated opt-in, so they must not advertise one (#202 review).
        "git_history_for_symbol" | "papertrail_for_symbol" => schema_for::<SymbolRefArgs>(),
        "find_callers" | "trace_callees" | "docs_for_symbol" => schema_for::<SymbolGraphArgs>(),
        "compare_graph_to_text" => schema_for::<CompareGraphTextArgs>(),
        "compare_graph_to_scip" => schema_for::<EmptyArgs>(),
        "impact_surface" => schema_for::<ImpactArgs>(),
        "repo_brief" => schema_for::<RepoBriefArgs>(),
        "repo_clusters" => schema_for::<RepoClustersArgs>(),
        "important_symbols" => schema_for::<ImportantSymbolsArgs>(),
        "find_clones" => schema_for::<FindClonesArgs>(),
        "clones_for_symbol" => schema_for::<ClonesForSymbolArgs>(),
        "ffi_surface" => schema_for::<LimitArgs>(),
        "read_chunk" => schema_for::<ReadChunkArgs>(),
        "git_history_for_path" | "github_refs_for_path" => schema_for::<PathHistoryArgs>(),
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
        "llm_status" | "github_sync_status" | "index_status" | "memory_validate"
        | "memory_doctor" => schema_for::<EmptyArgs>(),
        "memory_edge_add" => schema_for::<MemoryEdgeAddArgs>(),
        "memory_edge_remove" => schema_for::<MemoryEdgeRemoveArgs>(),
        "memory_edges" => schema_for::<MemoryEdgesArgs>(),
        _ => json!({"type": "object"}),
    }
}
