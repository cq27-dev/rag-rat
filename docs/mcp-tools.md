# MCP Tools

`rag-rat mcp` starts a local `rmcp` STDIO server. The server is read-only for configured source
roots; it may update the configured SQLite index when automatic stale-index healing is needed. It
opens the index through the configured root, so search and graph tools resolve the active git commit
plus dirty worktree overlay in the same way as CLI queries.

## Install

The MCP server is launched by the MCP client over STDIO; it does not listen on a TCP port. The
recommended setup is `rag-rat init` from the target repo, which indexes it and registers a
**project-scoped** MCP server. The per-repo setup, by hand:

```bash
cd /path/to/your/repo
cargo install rag-rat                 # or: cargo install --path crates/rag-rat-cli --bin rag-rat
rag-rat index --discover
rag-rat models install fastembed-all-minilm-l6-v2
rag-rat reconcile --changed-first --limit 500 --batch-size 64
rag-rat doctor
```

Use `embedding-hash` (built with `--no-default-features`) instead of FastEmbed when a small
dependency footprint matters more than real semantic embeddings:

```bash
rag-rat models install embedding-hash
```

Register a **project-scoped** server that runs in the repo so it resolves that repo's `rag-rat.toml`:

```bash
claude mcp add --scope project rag-rat -- rag-rat mcp
```

or a project `.mcp.json` / equivalent:

```json
{
  "mcpServers": {
    "rag-rat": { "command": "rag-rat", "args": ["mcp"] }
  }
}
```

Do **not** register a single global/user-scoped server with a hardcoded `--config /some/repo/...`:
it would serve that one repo's index and memories in every project. Keep the server project-scoped
and let it find `rag-rat.toml` from the repo it runs in. `--config <path>` remains available for a
specific profile, and `rag-rat mcp --json` switches tool results from TOON to JSON.

To run from a checkout without installing, use a project-scoped server whose command is
`cargo run --manifest-path /path/to/rag-rat/Cargo.toml --bin rag-rat -- mcp`.

## Tools

- `semantic_search`: `{ "query": string, "limit"?: number, "include_generated"?: boolean, "include_graph"?: "none" | "compact" | "full", "graph_limit"?: number, "include_git"?: boolean, "include_papertrail"?: boolean, "explain"?: boolean }`
- `symbol_lookup`: `{ "symbol"?: string, "ref"?: string, "id"?: string, "lang"?: string, "allow_ambiguous"?: boolean, "limit"?: number }`
- `find_callers`: `{ "symbol"?: string, "ref"?: string, "id"?: string, "resolution"?: "exact" | "syntactic" | "fuzzy", "allow_ambiguous"?: boolean, "limit"?: number, "include_unresolved"?: boolean, "include_macros"?: boolean, "include_common_methods"?: boolean, "include_references"?: boolean, "edge_kinds"?: string[] }`
- `trace_callees`: `{ "symbol"?: string, "ref"?: string, "id"?: string, "resolution"?: "exact" | "syntactic" | "fuzzy", "allow_ambiguous"?: boolean, "limit"?: number, "include_unresolved"?: boolean, "include_macros"?: boolean, "include_common_methods"?: boolean, "include_references"?: boolean, "edge_kinds"?: string[] }`
- `compare_graph_to_text`: `{ "symbol"?: string, "ref"?: string, "id"?: string, "pattern": string, "resolution"?: "exact" | "syntactic" | "fuzzy", "allow_ambiguous"?: boolean, "limit"?: number, "include_tests"?: boolean, "include_unresolved"?: boolean, "include_macros"?: boolean, "include_common_methods"?: boolean, "include_references"?: boolean, "edge_kinds"?: string[] }`
- `impact_surface`: `{ "query"?: string, "symbol"?: string, "ref"?: string, "id"?: string, "resolution"?: "exact" | "syntactic" | "fuzzy", "allow_ambiguous"?: boolean, "limit"?: number, "include_tests"?: boolean, "include_docs"?: boolean, "include_git"?: boolean, "include_papertrail"?: boolean, "include_text_fallback"?: boolean }`
- `ffi_surface`: `{ "limit"?: number }`
- `docs_for_symbol`: `{ "symbol"?: string, "ref"?: string, "id"?: string, "allow_ambiguous"?: boolean, "limit"?: number }`
- `read_chunk`: `{ "chunk_id": number, "include_graph"?: "none" | "compact" | "full", "graph_limit"?: number }`
- `commit_search`: `{ "query": string, "limit"?: number }`
- `git_history_for_path`: `{ "path": string, "limit"?: number }`
- `git_history_for_symbol`: `{ "symbol"?: string, "ref"?: string, "id"?: string, "lang"?: string, "allow_ambiguous"?: boolean, "limit"?: number }`
- `commits_touching_query`: `{ "query": string, "limit"?: number }`
- `git_blame_chunk`: `{ "chunk_id": number }`
- `papertrail_for_chunk`: `{ "chunk_id": number, "limit"?: number }`
- `papertrail_for_symbol`: `{ "symbol"?: string, "ref"?: string, "id"?: string, "lang"?: string, "allow_ambiguous"?: boolean, "limit"?: number }`
- `papertrail_for_commit`: `{ "commit_hash": string, "limit"?: number }`
- `github_issue_search`: `{ "query": string, "limit"?: number }`
- `github_refs_for_path`: `{ "path": string, "limit"?: number }`
- `rationale_search`: `{ "query": string, "limit"?: number }`
- `local_ai_status`: `{}`
- `heal_index`: `{ "limit"?: number }`
- `github_sync_status`: `{}`
- `index_status`: `{}`

`tools/list` is served by `rmcp` and exposes typed JSON schemas derived from the same request structs
used by the handlers. Existing tool names and response fields are kept stable for current MCP clients.

### Response encoding (TOON)

Tool **results** are returned as **TOON** (Token-Oriented Object Notation), not JSON. TOON is a
token-efficient text encoding that renders uniform result rows (caller lists, symbol candidates,
clusters) as a dense `[N]{cols}:` table — measured ~30% smaller than compact JSON on those payloads,
and never larger in practice (it ties compact JSON on nested shapes). Because MCP results are text
content consumed by an LLM, the denser encoding is both valid and cheaper. There is no per-call flag;
MCP output is always TOON. The JSON snippets below describe the **logical shape** of each response —
the same fields, just shown in JSON for readability. (The `rag-rat` CLI mirrors this: it defaults to
TOON and takes a global `--json` flag for JSON output.)

`symbol_lookup` is the candidate-selection step for symbol-shaped tools. It returns:

```json
{
  "candidates": [
    {
      "id": "sym_a3f29c1b",
      "logical_variant_count": 2,
      "logical_group_reason": "cfg_variant",
      "ref": "crates/app/src/runtime/task_spawn.rs::spawn_blocking",
      "qualified_name": "crates/app/src/runtime/task_spawn.rs::spawn_blocking",
      "kind": "function",
      "signature": "pub(crate) async fn spawn_blocking<F, T>(...)"
    }
  ],
  "disambiguation_required": true
}
```

Graph, docs, git-history, papertrail, and symbol-specific impact tools accept `id`,
`ref`, or `symbol` and resolve them in that priority order. `id` is the
**opaque, stable handle** a `symbol_lookup` (or any symbol-returning tool) emits — a `sym_<hex>`
token; copy it verbatim and pass it back, never parse it as a number. There is no numeric
`symbol_id` on the wire: it's an internal rowid reassigned on every reindex, so it could not be
cached safely across an edit (#149). If a bare `symbol` maps to multiple candidates, the tool
returns the same candidate object instead of guessing. Pass `allow_ambiguous: true` only for
navigation/debugging when name fallback is acceptable.

Logical symbols group multiple concrete symbol rows that represent one source-level API, such as
Rust `#[cfg]` variants of the same function in the same file. Use `id` with
`resolution: "exact"` when callers target the logical API rather than one cfg body. Exact logical
mode only returns edges whose verified `target_symbol_id` is a member of the group. Graph envelopes
include the selected `logical_symbol` and its `variants`; `cfg_expr` may be `null` until cfg
attribute extraction is recorded by the parser.

Search tools return chunk IDs, paths, line spans, current-text snippets in the `summary` field, and
scores. Search and `read_chunk` validate stored chunk anchors against current source before
returning context; stale files are reindexed once per tool call and their SQLite FTS5 rows are
updated before retrying. Use `read_chunk` only after a search or lookup has narrowed the context.

Auto-heal is capped at four files per call. If more stale files are detected, tools return a
`needs_reindex` error instead of doing unbounded work. Deleted source for a requested chunk returns
`Gone`; a chunk that disappears after file reindex returns `StaleChunk`.

`index_status` reports `content_revision`, `fts_source_revision`, `fts_dirty`, and `fts_fresh`.
Search tools synchronize dirty or stale SQLite FTS state before querying so results are not served
from an outdated FTS table.

`semantic_search` graph and evidence controls:

- `include_graph`: `none`, `compact`, or `full`. Default is `compact`.
- `graph_limit`: maximum caller/callee/import/type evidence entries to attach. Default is `3` for
  search and `20` for `read_chunk`.
- `include_git`: include git-history ranking boosts when available. Default is `true`.
- `include_papertrail`: include cached GitHub papertrail ranking boosts when available. Default is
  `true`.
- `explain`: include score components (`bm25`, `vector`, `symbol`, `graph`, `git`, `github`).
  Default is `false`.

Every hit also carries `retrieval_mode` (`lexical`, `vector`, or `hybrid`) — always present, so a
caller knows whether embeddings contributed without setting `explain`. It is `lexical` for every
hit when no embedding model is active.

Graph tools are backed by tree-sitter-derived syntax edges. Edge kinds are `imports`, `exports`,
`calls_name`, `constructs`, `uses_macro`, `references_type`, `implements`, and `contains`; confidence is reported as
`edge_confidence` (`confidence` is retained as the compatibility alias) with values `Exact`,
`Syntactic`, `NameOnly`, or `Ambiguous`. Graph evidence is syntactic, confidence-labeled evidence,
not compiler-grade name resolution or hard truth. Search results default to compact graph evidence
with bounded caller/callee lists; `read_chunk` defaults to full graph evidence. Caller and callee
entries include exact tree-sitter callsite spans: `callsite.path`, `callsite.line`, and
`callsite.span` (`[start_line, end_line]`).

Graph tools and `impact_surface` accept `resolution`:

- `exact`: only verified target-symbol rows are returned. A row is allowed only when the edge's
  verified target resolves to the selected symbol (its `id`/`ref`), or the
  resolved fully-qualified symbol identity matches `symbol`. Every returned row has
  `verified_target_symbol: true`.
- `syntactic`: default. Exact matches plus qualified syntactic evidence are returned. Unresolved
  qualified call targets may be shown, but broad bare-name ambiguous fallback is excluded.
- `fuzzy`: compatibility/navigation mode. Suffix and bare-name fallback are allowed, including
  ambiguous candidates. Treat these rows as possible evidence, not proof.

Use `id` (the `sym_<hex>` handle from a previous `symbol_lookup`) with
`resolution: "exact"` to target a resolved symbol — including the cfg/duplicate-variant case, where
the handle addresses the whole logical group (all its members). Bare names in `exact` mode
intentionally return little or nothing unless `id` is provided.

`find_callers` and `trace_callees` return a graph envelope, not a bare row array:

```json
{
  "query": {
    "tool": "find_callers",
    "id": "sym_a3f29c1b",
    "ref": "crates/app/src/runtime/task_spawn.rs::spawn_blocking",
    "resolution": "exact"
  },
  "summary": {
    "returned_count": 38,
    "total_matching_edges": 38,
    "truncated": false,
    "exact_verified": 38,
    "syntactic": 0,
    "name_only": 0,
    "ambiguous": 0,
    "unresolved": 0,
    "false_positive_risk": "low",
    "completeness_risk": "low"
  },
  "coverage": {
    "indexed_files": 1055,
    "parser_failures": 0,
    "stale_files": 0,
    "known_index_gaps": [],
    "parser_coverage_for_paths": []
  },
  "results": []
}
```

The trust signal is in `summary` and `coverage`: exact mode should have
`exact_verified == total_matching_edges`, `truncated: false`, no parser failures for covered paths,
`stale_files: 0`, and `completeness_risk: low`. If the source changed after indexing, `stale_files` is non-zero
for the covered paths and the result should be treated as needing reindex before audit use.
When strict graph rows are verified but same-name unresolved callsites exist, `known_index_gaps`
contains a plain-language note such as
`31 unresolved qualified callsites match the requested final segment but are not verified to this symbol`.

`trace_callees` is repo-relevant by default. It returns verified or syntactic `calls_name` edges and
verified/repo-local `constructs` edges. It hides unresolved calls, unresolved macros, type
references, imports/exports, common method/combinator calls, and std/common constructors unless a
caller asks for them explicitly:

- `include_unresolved`: include unresolved qualified/name-only calls.
- `include_macros`: include `uses_macro` edges such as `format!`, `json!`, and `vec!`.
- `include_common_methods`: include common low-signal calls such as `clone`, `map`, `map_err`,
  `and_then`, `unwrap_or`, `unwrap_or_else`, `to_string`, `to_owned`, `as_ref`, `as_mut`, `get`,
  `insert`, and unresolved/common `new`.
- `include_references`: include type references, imports, exports, `contains`, and `implements`.

`find_callers` uses exact target resolution first, then qualified-name and target-name fallbacks;
fallback hits are labeled with `resolution`, `verified_target_symbol`, raw `evidence`, and optional
`receiver_hint` so clients can treat name-only or ambiguous edges as possible evidence instead of
compiler truth. Rust macro invocations are stored as `uses_macro` and are not resolved to same-named
normal modules or functions.

`compare_graph_to_text` is the graph-vs-rg audit bridge. It resolves a selected symbol, runs the
same reverse caller graph traversal used by `find_callers`, runs a line-oriented regex search over
currently indexed source files, then compares `(path, line)` sets:

```json
{
  "query": {
    "id": "sym_a3f29c1b",
    "ref": "crates/app/src/runtime/task_spawn.rs::spawn_blocking",
    "pattern": "crate::runtime::task_spawn::spawn_blocking\\(",
    "resolution": "syntactic",
    "include_tests": true
  },
  "summary": {
    "text_hits": 38,
    "graph_hits": 31,
    "matched": 29,
    "graph_only": 2,
    "text_only": 9,
    "likely_parser_gaps": 7,
    "likely_false_positives": 1,
    "complete": false,
    "recommended_fallback": "both"
  },
  "matched_hits": [],
  "text_only_hits": [],
  "graph_only_edges": [],
  "likely_parser_gaps": [],
  "likely_false_positives": []
}
```

Matching is by `path + callsite line`. Text hits with no graph edge at that location are reported as
`text_only_hits` and copied into `likely_parser_gaps`; graph edges with no text match are reported as
`graph_only_edges`. Exact or logical graph-only rows may be legitimate imported, aliased, or
unqualified calls whose source line does not match the supplied regex. Fuzzy/name-only graph-only
rows are also listed in `likely_false_positives`. The tool includes the same `coverage` object as
graph traversal envelopes so audit output can be checked for stale source and parser failures before
treating the counts as authoritative.

`impact_surface` is the graph-backed rg replacement path for a selected symbol. For
`id`, `ref`, or `symbol` requests it returns sections instead of a flat list:

1. `direct_semantic_callers`
2. `direct_semantic_callees`
3. `import_export_dependents`
4. `tests_touching_symbol_path`
5. `docs_mentioning_symbol_path`
6. `text_fallback_hits`
7. `recent_commits_touching_symbol_path`
8. `github_rationale_issues_prs`
9. `completeness_and_caveats`

Use the same disambiguation controls as graph tools. `resolution: "exact"` means direct graph
callers/callees are verified against the selected symbol; `syntactic` allows qualified
tree-sitter evidence; `fuzzy` is for navigation only. Optional section controls are `include_tests`,
`include_docs`, `include_git`, `include_papertrail`, and `include_text_fallback`, all enabled by
default. If exact graph callers are empty but text fallback finds symbol/path hits, the caveats
section explicitly says that graph extraction or resolution gaps are likely. Free-text `query`
requests retain the older flat impact item shape for compatibility.

Git history tools return historical evidence. `commit_search` searches commit subjects and bodies;
`git_history_for_path` returns commits touching a current path; `git_history_for_symbol` resolves
the current symbol path/range first, then returns path history; `commits_touching_query` combines
commit-message evidence with current file-change evidence; `git_blame_chunk` computes blame lazily
and caches it by `source_text_hash`.

`index_status.git_history` reports whether git history is available, current git HEAD, indexed HEAD,
commit count, and file-change count. Non-git roots report unavailable history without failing source
or docs indexing.

GitHub papertrail tools read the local cache only. `rag-rat github sync --from-refs` discovers refs
and fetches through `gh api --paginate`; `rag-rat github sync --issue owner/repo#123` fetches one
issue/PR thread; `--offline` updates discovered refs and reports cache status without network use.

Papertrail outputs keep `current_source` separate from `github_evidence`. GitHub snippets are labeled
as historical GitHub evidence and classified as `decision`, `rejected_alternative`, `constraint`,
`risk`, `obsolete`, or `context`.

`index_status.github` reports cached refs, issues, comments, pulls, reviews, review comments,
last sync time, and whether the `gh` CLI capability is available.
`github_sync_status` returns that GitHub cache section directly. `heal_index` repairs or removes
already-indexed files whose current source no longer matches the stored SQLite index, then refreshes
SQLite FTS. It does not discover brand-new files; run `rag-rat index` for discovery.

Local AI artifacts are explicit and current-only. `local_ai_status` reports embedding model state,
artifact counts, FastEmbed build/cache/model details, and the last reconcile throughput summary when
available. The CLI-only `models install embedding-hash` command selects the deterministic baseline
embedder. Building with `--features fastembed` enables the real local
`fastembed-all-minilm-l6-v2` backend; `models install fastembed-all-minilm-l6-v2` is the intended
FastEmbed cache-population step. `doctor` reports FastEmbed build support, cache path, model,
dimension, current/stale/missing/failed embedding counts, last reconcile throughput, and the next
command needed to make local AI current. `reconcile` writes model-id, dimension, text-hash-bound, and
embedding-input-hash-bound chunk embeddings in configurable batches for eligible current chunks
only.
`semantic_search` combines BM25 candidates, vector similarity, symbol/name/path boosts,
graph-neighborhood boosts, and optional git/GitHub papertrail boosts. Embeddings are used only when
the active model is installed, the embedding dimension matches active model metadata, the artifact
status is `Current`, the artifact text hash matches the current chunk text hash, and the stored
embedding input hash matches the current bounded embedding input; stale embeddings are treated as
absent. There is no summarizer or LLM runtime in this milestone.

Reconcile intentionally does not embed every chunk. It skips generated/coarse chunks, tiny chunks,
oversized low-value chunks, unsupported languages, low-signal import/export barrels, and test
fixtures with explicit policy reasons such as `SkipGenerated`, `SkipTooSmall`, `SkipTooLarge`,
`SkipLowSignal`, `SkipLanguageUnsupported`, and `SkipTestFixture`. Source symbols and docs are
prioritized ahead of tests and low-signal chunks. `rag-rat reconcile --plan` reports skipped counts
by policy and missing work by priority; reconcile JSON reports batch size, input character counts,
truncated input counts, `chunks_per_sec`, `chars_per_sec`, and average input size. Use
`rag-rat reconcile --changed-first --max-seconds 60 --batch-size 64` for bounded daily catch-up and
`rag-rat reconcile --until-clean --batch-size 64` for a full backlog pass.

Indexing uses a Rayon worker pool for CPU-heavy preparation: file reads, hashing, tree-sitter
chunk/symbol preparation, and git-log parsing run across available cores. Reconcile builds bounded
embedding inputs, runs model inference outside SQLite transactions, then writes vectors through one
short serialized transaction per batch.

Parser failures are visible through `index_status.parser_failure_paths`, with path, language, and
message for each failed source parse. Markdown files are chunked by headings instead of parsed with
tree-sitter.
