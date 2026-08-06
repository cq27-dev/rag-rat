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
- **`symbol_lookup`** — exact/fuzzy symbol resolution (Rust/TS/Kotlin/C/C++/Python/Swift/Go), with
  any bound memories attached.
- **`impact_surface`** — the **coding preflight before editing any non-trivial symbol**: callers,
  callees, tests, git history, papertrail, and the repo memories crossing that call path. Run it
  before you change something load-bearing — it's how you avoid missing an invariant.
- **`find_callers` / `trace_callees`** — reverse/forward graph traversal instead of grepping for
  call sites.
- **`read_chunk`** — current text for a chunk with anchor validation + graph + memories.
- **`repo_brief` / `repo_clusters`** — orientation (spine, churn, god-modules, ownership clusters).
- **`important_symbols`** — load-bearing symbols by (SCIP-aware) PageRank; pass `personalize` to bias
  toward what you're editing.

That's the daily loop. The MCP exposes **many more tools** — reach past the core ones by the question
you're actually asking (full schemas: `docs/mcp-tools.md`):

| When you want to… | Reach for |
|---|---|
| Find where a concept/behavior lives | `semantic_search` |
| Resolve a symbol by name (exact/fuzzy) | `symbol_lookup` |
| See what calls X / what X calls | `find_callers` / `trace_callees` |
| **Know the blast radius before editing** | **`impact_surface`** (callers, callees, tests, history, memories — the preflight) |
| Read a chunk's exact current text | `read_chunk` |
| Orient in an unfamiliar repo | `repo_brief` (spine / churn / god_modules / refactor_candidates), `repo_clusters` |
| Find the load-bearing symbols | `important_symbols` |
| Check if code duplicates what's already here | `find_clones`; the clone class of one symbol → `clones_for_symbol` |
| Understand **why** code exists (rationale) | `papertrail_for_symbol` / `papertrail_for_chunk`, `rationale_search` |
| Trace **when/why** something changed | `git_history_for_symbol` / `git_history_for_path`, `commit_search`, `commits_touching_query`, `git_blame_chunk` |
| Pull a tracker issue/PR or refs for a path | `papertrail_issue_search`, `papertrail_refs_for_path`, `papertrail_for_commit` |
| Read docs / doc-comments for a symbol | `docs_for_symbol` |
| Map the FFI / binding surface | `ffi_surface` |
| Audit whether the graph is trustworthy here | `compare_graph_to_scip` (vs compiler), `compare_graph_to_text` (vs regex) |
| Recall prior notes and their links | `memory_search`, `memory_for_symbol` / `memory_for_path` / `memory_for_call_path`, `memory_edges` |
| Triage the memory-maintenance worklist | `dream` → `dream_review` (see the **dream-review** skill) |
| Check index / embedding / papertrail-cache health | `index_status`, `llm_status`, `papertrail_sync_status`; repair drift with `heal_index` |

Reaching for the right tool is cheap and eager: prefer the specific one (`papertrail_for_symbol` for
*why*, `find_clones` before writing a helper) over defaulting to `semantic_search` for everything.

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
then `rag-rat reconcile`. (Optional but recommended: install the rag-rat plugin — it registers a
PreToolUse hook that auto-augments your `grep`/`rg` calls with symbol + memory context.)

## Rule 2 — Record durable learnings as rag-rat memories before you finish

**The gate, before anything else: could the next agent recover this by reading the repo?** If yes,
don't record it. A memory that restates what the code, the types, the tests, or the history already
say leaves the reader *worse off*, not merely no better — it costs attention and returns nothing. On
most tasks the honest answer is **record nothing**; that is the common outcome, not a failure to try
hard enough. What passes the gate is what you had to read several files and reason to learn, and
that the repo states nowhere.

**Rejected alternatives lead.** The approach that was *not* taken leaves no artifact anywhere — not
in the diff, not in the types, not in the history — which makes it at once the most-asked question
about unfamiliar code and the least-supplied answer. If your task settled a choice, record the
alternative and why it lost (`RejectedAlternative`) before you record anything about what the code
now does.

**Why rag-rat and not your harness's own notes:** rag-rat memories live in the repo's shared index,
so they surface for **every** agent that queries it — Claude Code, Codex, any future tool — not just
the one that wrote them. Your harness's private memory is invisible to the others. rag-rat is the
**cross-agent memory layer**.

**Translate; never store the trajectory.** What you did, in what order, is worth less than nothing
to the next reader — a replayed trace scores worse than having no memory at all. Distill it to a
situation and an action — *when X, do Y, because Z* — and quote the evidence you generalised from
(the failing line, the error, the constraint). Situated questions are not answered by general
advice, and the quote is what spares the reader from trusting a summary of a summary.

**Revise before you create.** A memory that has drifted actively misleads, while a missing one
merely fails to help — so correcting or retiring a wrong record is the highest-value write
available. `memory_search` first: if something already covers the ground, `memory_update` it; reach
for `memory_create` only when nothing does.

**Terseness is a staleness strategy, not a style preference.** Every extra detail is one more thing a
later change can falsify, and a record that contradicts the source on one line gets distrusted on all
of them. State the rule and the reason, then stop.

**Write the present tense, not a changelog.** A memory is read by someone about to change the code,
so it must say **what is true now and what to do about it**. "This was fixed in #123", "the predicate
used to fail open", "stage 2 landed the split" are unactionable, and history goes stale the moment
the next change lands. The same applies when you update one: if the thing a memory warned about has
been fixed, rewrite the body to state the rule that now holds rather than appending a status section,
and `memory_mark_obsolete` it if nothing actionable survives.

Keep: invariants, the reasoning behind a decision, traps and their failure modes, what to reach for,
what is still unresolved. Drop: PR/stage narration, "used to be", anything whose only value is that
it happened. Referencing an issue or test *name* is fine when it is a pointer the reader can follow —
it is the story that does not belong.

Do it well:
- **Anchor to the tightest stable target:** prefer an `id` binding (the `sym_<hex>` handle —
  self-heals across cross-file moves); fall back to a `path` binding for file/area notes, or a
  commit/GitHub ref for historical rationale.
- **Pick the right `kind`:** `Invariant` (must stay true), `Decision`/`RejectedAlternative` (why
  this / why not that), `Risk`/`BugPattern` (footguns), `PerformanceNote`, `PlatformQuirk`,
  `FFIBoundary`. Write a concrete title and a body with the **why** + **how to apply** — not just the
  what, and not what changed.
- **After a large refactor**, `memory_doctor` flags `gone` anchors and `memory_rebind` re-anchors
  them.

The memory layer is kept honest by **`dream`** — a maintenance worklist of load-bearing code with no
memory (coverage gaps) and memories that have drifted from the source. The **dream-review** skill is
the loop for triaging it.

The equivalents exist on the CLI too (`rag-rat memory …`) — use whichever your harness exposes.
