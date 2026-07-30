# AGENTS.md

Guidance for coding agents working in the `rag-rat` repository.
(`CLAUDE.md` is a symlink to this file.)

## Prefer the rag-rat MCP for code browsing

This repo ships `rag-rat` — a local repo-intelligence index and MCP server — and it is indexed by
its own server (dogfooded). **Heavily prefer the `rag-rat` MCP tools over raw `grep`/`cat`/file
reads when browsing or understanding code.** One MCP call returns more context, faster, than a
shell sweep, and it surfaces *drive-by repo memories* (source-anchored invariants, decisions,
risks) attached to the code you're touching — context you would otherwise never see.

Reach for these first:

- **`semantic_search`** — "where is this concept implemented?" Returns current source chunks with
  inline graph (callers/callees), git, and GitHub papertrail, all validated against current source.
- **`symbol_lookup`** — exact/fuzzy symbol resolution (Rust/TS/Kotlin/C/C++/Python/Swift/Go), with
  any bound memories attached.
- **`impact_surface`** — the coding preflight before editing a symbol: graph callers/callees,
  tests, git history, papertrail, and **repo memories** crossing the call path. Run it before
  changing anything non-trivial.
- **`find_callers` / `trace_callees`** — reverse/forward graph traversal instead of grepping for
  call sites.
- **`read_chunk`** — current text for a chunk with anchor validation + graph + memories.
- **`repo_brief` / `repo_clusters`** — orientation (spine, churn, god-modules, ownership clusters).
- **`important_symbols`** — load-bearing symbols by (SCIP-aware) PageRank; pass `personalize` to
  bias toward what you're editing. Compiler-grade once `rag-rat oracle run` has run.

**Symbol handle:** symbol-returning tools emit `id`, an opaque `sym_<hex>` token — the stable handle
to cache and pass back into graph/impact/memory tools as the `id` param (copy verbatim; never parse
it as a number). There is no numeric `symbol_id` on the wire (it's an internal rowid reassigned on
every reindex). Use `ref` (the `path::name` qualified name) for the human-readable identity. The
symbol-tool params are `ref` / `id` / `lang` (formerly `symbol_path` / `logical_symbol_id` /
`language`).

Why this beats grep here:
- Results carry **provenance**: confidence labels, coverage warnings, and raw evidence, so you can
  judge them rather than trust them blindly.
- **Drive-by memories**: a function may carry an `Invariant`/`Decision`/`Risk` memory that explains
  a non-obvious constraint. Grep can't show you that; the MCP tools attach it automatically. See
  *Record durable learnings as rag-rat memories* below — capturing these is part of the job.
- The index is **kept fresh by git hooks** (see below), so what the MCP returns matches HEAD.

Fall back to direct file reads/edits for the actual *writing* of code, and to confirm exact text
before an `Edit`. Use the MCP to *find and understand*; use the file tools to *change*. (The MCP
server is read-only on source — it never edits files; it writes only its own SQLite index.)

## Record durable learnings as rag-rat memories

**This is required, not optional.** When you discover something durable and non-obvious — a
load-bearing invariant, a decision + its rationale, a risk/footgun that cost you time, a perf
characteristic, a "do not do X because Y" — record it with `memory_create` **before you finish the
task**. If you had to read three files and reason for ten minutes to learn it, the next agent should
get it in one MCP call. Don't let hard-won context evaporate.

**Why rag-rat and not your own notes:** rag-rat memories live in this repo's shared index, so they
surface for **every** agent that uses the rag-rat MCP — Claude Code, Codex, and any future tool —
not just the one that wrote them. An agent's private/session memory (e.g. Claude Code's file memory,
Codex's own store) is invisible to the others. rag-rat is the **cross-agent memory layer**; put
anything another agent would benefit from here.

How to do it well:
- **Anchor to the tightest stable target.** Prefer an `id` binding (the `sym_<hex>` logical-symbol
  handle — self-heals across cross-file moves via the relocation engine); fall back to a `path`
  binding for file/area-level notes, or a commit/GitHub ref for historical rationale.
- **Pick the right `kind`:** `Invariant` (must stay true), `Decision`/`RejectedAlternative` (why it's
  this way / why not the other), `Risk` / `BugPattern` (footguns), `PerformanceNote`, `PlatformQuirk`,
  `FFIBoundary`. Write a concrete title, and a body with the *why* and *how to apply* — not just *what*.
- **Write the present tense, not a changelog.** A memory says what is true NOW and what to do about
  it. "Fixed in #123", "used to fail open", "stage 2 landed the split" are unactionable: they cost
  the reader attention, go stale on the next change, and teach them to distrust the rest of the
  entry. When updating a memory whose warning no longer applies, rewrite the body to state the rule
  that now holds — do not append a status section — and `memory_mark_obsolete` it if nothing
  actionable survives. An issue or test *name* is a fine pointer; the story is not.
- **`memory_search` first** to avoid duplicates; **`memory_update` / `memory_mark_obsolete`** when a
  memory is wrong or superseded (don't leave stale guidance).
- **After large refactors**, run `rag-rat memory doctor` and re-anchor anything it flags `gone`
  (`rag-rat memory rebind <id> --symbol <name>`); content-confirmed moves re-anchor automatically.
- This works the same from any agent: Claude Code calls the MCP `memory_create` tool; the `rag-rat`
  CLI exposes the equivalents — use whichever your harness has.

If the MCP returns empty results, the self-index may be stale or pointed at the wrong root —
`rag-rat index --discover` then `rag-rat reconcile` refreshes it.

The grep-augmentation PreToolUse hook augments Claude Code's `Grep` and Bash grep/rg/ag calls with
symbol and repo-memory context automatically; it ships with the rag-rat plugin (install the plugin —
see the README's "Connect it to your agent (MCP)" section).

## Public artifacts describe the change, not the process that produced it

PR descriptions, commit messages, and issue text are read by humans reviewing the **change**. Keep
them about the code — the problem, the fix, the rationale, how it was verified. Do **not** narrate
the process that produced them:

- No agent/subagent counts or fan-out language ("a 12-agent sweep found…", "each verifier reported…").
- No multi-agent / workflow / orchestration / delegation framing, and no internal phase or round
  codes (`C3.1`, "round 6"). Give the change a descriptive title and reference the issue number.
- No naming of AI assistants or AI review tooling, and no review play-by-play ("caught in review and
  fixed before merge"). State the final behavior; if a concern shaped the design, explain the concern
  on its own terms.

A reviewer should not be able to tell from a PR whether one agent or twenty produced it — only what
changed and why. Durable, checkable references are encouraged: issue/PR numbers, commit SHAs, test
names, file paths. (This is about *public* artifacts; rag-rat memories are the internal cross-agent
layer and may record provenance freely.)

## Repo orientation

- Rust workspace, 13 crates in a layered DAG (Rust 2024 edition):
  - `rag-rat-base` — foundation: config, repo identity/discovery, language + embedding-model
    registries, path classification, canonical JSON, locks, logging.
  - `rag-rat-db` — the SQLite substrate: schema/migrations (+ the `MigrationHooks` seam),
    storage, meta, chunk-text store/compression.
  - domain layers on base+db: `rag-rat-llm` (embedder providers, cookbook provisioning),
    `rag-rat-papertrail` (provider-neutral GitHub/GitLab tracker mirrors), `rag-rat-clones`
    (fingerprints, postings, refine/antiunify), and `rag-rat-oracle` (SCIP/LSP evidence,
    manifests, verdict store).
  - `rag-rat-query` — the read layer: graph/impact/symbol/tree queries, repo-memory reads +
    evidence resolution, pagerank, orientation primitives.
  - `rag-rat-dream` — deterministic memory-maintenance findings plus model-driven verify/compact
    passes, built on query+llm.
  - `rag-rat-oplog` — signed, hash-chained memory op-log: operation model, projection fold,
    account/authority substrate, content-candidate DAG, durable store.
  - `rag-rat-core` — the engine that remains: indexing + tree-sitter graph, the
    `IndexDatabase` query surface, memory-write orchestration, watcher, eval.
  - `rag-rat-sync` — iroh QUIC peer transport for exchanging signed op-log entries.
  - entrypoints: `rag-rat-mcp` (the STDIO MCP server) and `rag-rat-cli` (package name `rag-rat`,
    the CLI binary).
  All crates version in lockstep; see docs/releasing.md.
- `rag-rat.toml` (repo root) configures what gets indexed and the SQLite database path.

## Worktree correctness

All changes that affect indexing or worktree-overlay behavior MUST support both the main checkout and
linked worktrees sharing the same database. Every such change MUST include regression tests that
exercise worktree behavior, including active-checkout scope and sibling-checkout preservation or
isolation as applicable. Single-checkout coverage alone is not sufficient for index/overlay changes.

## Style

Follow the `rust-modern-style` conventions: closed/persisted enums use strum-backed stable tokens
behind `as_db_str`/`from_db_str`, `{self, ..}` imports for mixed lists, read/write-obvious DB method
names, injected time (`now_ms()`), parameter structs over long arg trains, `mod.rs` as a curated
index. Keep SQL in helpers named for the domain question, with invariant comments and tests
(migrations included).

## Build / test

```bash
cargo build
cargo nextest run -p rag-rat-core   # CI runs nextest (`cargo nextest run --workspace`); see .config/nextest.toml
cargo clippy --all-targets
cargo +nightly fmt   # CI uses nightly rustfmt; stable fmt silently diverges and reddens CI
```
