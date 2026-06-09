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
- **`symbol_lookup`** — exact/fuzzy symbol resolution (Rust/TS/Kotlin/C/C++), with any bound
  memories attached.
- **`impact_surface`** — the coding preflight before editing a symbol: graph callers/callees,
  tests, git history, papertrail, and **repo memories** crossing the call path. Run it before
  changing anything non-trivial.
- **`find_callers` / `trace_callees`** — reverse/forward graph traversal instead of grepping for
  call sites.
- **`read_chunk`** — current text for a chunk with anchor validation + graph + memories.
- **`repo_brief` / `repo_clusters`** — orientation (spine, churn, god-modules, ownership clusters).

Why this beats grep here:
- Results carry **provenance**: confidence labels, coverage warnings, and raw evidence, so you can
  judge them rather than trust them blindly.
- **Drive-by memories**: a function may carry an `Invariant`/`Decision`/`Risk` memory that explains
  a non-obvious constraint. Grep can't show you that; the MCP tools attach it automatically. When
  you discover a durable invariant or rationale, record it with `memory_create` so the next agent
  gets it for free.
- The index is **kept fresh by git hooks** (see below), so what the MCP returns matches HEAD.

Fall back to direct file reads/edits for the actual *writing* of code, and to confirm exact text
before an `Edit`. Use the MCP to *find and understand*; use the file tools to *change*. (The MCP
server is read-only on source — it never edits files; it writes only its own SQLite index.)

If the MCP returns empty results, the self-index may be stale or pointed at the wrong root —
`rag-rat index --discover` then `rag-rat reconcile` refreshes it.

The grep-augmentation PreToolUse hook augments Claude Code's `Grep` and Bash grep/rg/ag calls with
symbol and repo-memory context automatically; install it with `rag-rat hooks install --claude` (or
`--global`).

## Repo orientation

- Rust workspace, three crates: `rag-rat-core` (engine: indexing, tree-sitter graph, embeddings,
  git/GitHub, repo memories), `rag-rat-mcp` (the STDIO MCP server), `rag-rat-cli` (the `rag-rat`
  binary). Rust 2024 edition.
- `rag-rat.toml` (repo root) configures what gets indexed and the SQLite database path.

## Style

Follow the `rust-modern-style` conventions: closed/persisted enums with `as_db_str`/`from_db_str`,
`{self, ..}` imports for mixed lists, read/write-obvious DB method names, injected time (`now_ms()`),
parameter structs over long arg trains, `mod.rs` as a curated index. Keep SQL in helpers named for
the domain question, with invariant comments and tests (migrations included).

## Build / test

```bash
cargo build
cargo test -p rag-rat-core
cargo clippy --all-targets
cargo fmt
```
