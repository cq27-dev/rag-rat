---
name: using-rag-rat
description: >
  Use when working in a repository indexed by rag-rat (a `rag-rat.toml` at the root and a rag-rat MCP
  server available). Establishes the working rule: reach for the rag-rat MCP tools
  (semantic_search, symbol_lookup, impact_surface, find_callers/trace_callees, repo_brief,
  important_symbols) to FIND and UNDERSTAND code before falling back to grep/cat, and record durable,
  non-obvious learnings as rag-rat memories before finishing. Triggers: "use rag-rat", "how do I
  navigate this repo", any code-understanding task in a rag-rat repo.
---

# using-rag-rat — navigate with the MCP, remember what you learn

This repo is indexed by **rag-rat**, a local repo-intelligence index + MCP server. One MCP call
returns graph (callers/callees), git, GitHub papertrail, and **drive-by repo memories**
(source-anchored invariants, decisions, risks) — all validated against current source. A `grep`
can't surface any of that. Two rules follow.

## Rule 1 — Find and understand through the MCP, not a shell sweep

Prefer these over `grep`/`cat`/file sweeps when browsing or understanding code:

- **`semantic_search`** — "where is this concept implemented?" Current source chunks with inline
  graph, git, and papertrail.
- **`symbol_lookup`** — exact/fuzzy symbol resolution (Rust/TS/Kotlin/C/C++/Python), with any bound
  memories attached.
- **`impact_surface`** — the **coding preflight before editing any non-trivial symbol**: callers,
  callees, tests, git history, papertrail, and the repo memories crossing that call path. Run it
  before you change something load-bearing — it's how you avoid missing an invariant.
- **`find_callers` / `trace_callees`** — reverse/forward graph traversal instead of grepping for
  call sites.
- **`read_chunk`** — current text for a chunk with anchor validation + graph + memories.
- **`repo_brief` / `repo_clusters`** — orientation (spine, churn, god-modules, ownership clusters).
- **`important_symbols`** — load-bearing symbols by (SCIP-aware) PageRank; pass `personalize` to bias
  toward what you're editing.

**Symbol handle:** symbol-returning tools emit `id`, an opaque `sym_<hex>` token — the stable handle
to cache and pass back into graph/impact/memory tools as `id` (copy verbatim; never parse it as a
number). Use `ref` (the `path::name` qualified name) for human-readable identity.

Why this beats grep: results carry **provenance** (confidence, coverage warnings, raw evidence) so
you can judge them; a function may carry an `Invariant`/`Decision`/`Risk` **memory** that explains a
non-obvious constraint grep can't show; and the index is **kept fresh by git hooks**, so what the
MCP returns matches HEAD.

Use the MCP to **find and understand**; use your file tools to **change** (and to confirm exact text
before an edit). The MCP is read-only on source — it never edits files.

If the MCP returns empty/thin results, the index is stale or mis-rooted: `rag-rat index --discover`
then `rag-rat reconcile`. (Optional but recommended: `rag-rat hooks install --claude` adds a
PreToolUse hook that auto-augments your `grep`/`rg` calls with symbol + memory context.)

## Rule 2 — Record durable learnings as rag-rat memories before you finish

When you discover something **durable and non-obvious** — a load-bearing invariant, a decision + its
rationale, a risk/footgun that cost you time, a perf or platform quirk — record it with
`memory_create` **before finishing the task**. If you had to read several files and reason to learn
it, the next agent should get it in one MCP call.

**Why rag-rat and not your harness's own notes:** rag-rat memories live in the repo's shared index,
so they surface for **every** agent that queries it — Claude Code, Codex, any future tool — not just
the one that wrote them. Your harness's private memory is invisible to the others. rag-rat is the
**cross-agent memory layer**.

Do it well:
- **`memory_search` first** to avoid duplicates.
- **Anchor to the tightest stable target:** prefer an `id` binding (the `sym_<hex>` handle —
  self-heals across cross-file moves); fall back to a `path` binding for file/area notes, or a
  commit/GitHub ref for historical rationale.
- **Pick the right `kind`:** `Invariant` (must stay true), `Decision`/`RejectedAlternative` (why
  this / why not that), `Risk`/`BugPattern` (footguns), `PerformanceNote`, `PlatformQuirk`,
  `FFIBoundary`. Write a concrete title and a body with the **why** + **how to apply** — not just the
  what.
- **`memory_update` / `memory_mark_obsolete`** when a memory is wrong or superseded — don't leave
  stale guidance. After a large refactor, `rag-rat memory doctor` flags `gone` anchors to re-bind.

The equivalents exist on the CLI too (`rag-rat memory …`) — use whichever your harness exposes.
