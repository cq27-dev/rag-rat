# Compiler-grade resolution & importance ranking (the SCIP oracle)

The code graph is tree-sitter-derived, so edges are heuristic and confidence-labeled
(`Exact` / `Syntactic` / `NameOnly` / `Ambiguous`). The **oracle** is an opt-in pass that consumes a
pre-built [SCIP](https://docs.sourcegraph.com/code_navigation/explanations/scip) index from a real
language tool (`rust-analyzer scip` for Rust, `scip-clang` for C/C++, `scip-python` for Python) and
uses it as a *resolution oracle* to upgrade those edges to a `Compiler` confidence tier.

It is opt-in, batch, content-addressed, and network-free — indexing stays fast and dependency-free
without it.

## What it does

- **Upgrades edges.** A heuristic edge the compiler confirms is tagged `Compiler` with
  `scip:<tool>@<version>` provenance, kept in a **side table** so the original heuristic resolution
  on the edge is never overwritten.
- **Recovers unresolved calls.** A `NameOnly` edge tree-sitter couldn't resolve, but the compiler
  can, is retargeted to the real in-corpus symbol.
- **Marks externals.** Edges that point into a dependency are labelled `resolved-external(<package>)`.
- **Reports disagreements.** `compare_graph_to_scip` shows where tree-sitter and the compiler differ.
- **Stabilizes memories.** SCIP monikers double as stable semantic anchors for repo memories,
  relocating a memory across a file move when its text/boundary anchors die.

## Importance ranking

`important_symbols` ranks the load-bearing symbols by weighted PageRank over the resolved
symbol→symbol graph. When an oracle run exists, the ranking becomes **SCIP-aware**: compiler-verified
edges are weighted at the compiler tier, recovered edges contribute, and phantom/external edges are
dropped — so the ranking surfaces the genuine god-modules instead of test-helper noise.

`important_symbols` also accepts `--personalize <names/paths/ids>` (the "Aider repo-map" trick:
biases the ranking toward the symbols you're editing). The MCP default auto-seeds from your current
git diff — "importance relative to your current changes." When the seed touches nothing connected in
the graph the result is honestly labelled global, not personalized.

## Running it

One-shot, by hand:

```bash
rag-rat oracle run                 # uses rust-analyzer by default
rag-rat oracle run --tool rust-analyzer        # Rust
rag-rat oracle run --tool scip-clang           # C/C++ (needs a compile_commands.json)
rag-rat oracle run --tool scip-python          # Python (resolves against installed deps)
rag-rat oracle run --scip path/to/index.scip   # consume a pre-built SCIP index directly
rag-rat oracle status
```

`scip-python` resolves imports against the project's **installed** dependencies, so the checkout's
deps must be importable (e.g. installed into a virtualenv) for cross-package edges to resolve; it
also derives its project version from the git revision, so it expects a git checkout.

A missing/unrunnable tool degrades to `Blocked` with an install hint and exit 0 — never an error.

## Keeping it fresh automatically

`[oracle] auto_run` (off by default) lets the long-lived `rag-rat mcp` server refresh the oracle on
its own when the active checkout's index is **stale** *and* has been **quiet** long enough, throttled
by a minimum interval. `rag-rat init` asks whether to enable it.

```toml
[oracle]
auto_run = true
auto_run_quiet_period_secs = 900     # wait ~15 min after the last index change before a run
auto_run_min_interval_secs = 21600   # and at most once every 6 h
```

It needs a SCIP tool (e.g. `rust-analyzer`) on `PATH`, runs only inside the MCP server, and produces
the `.scip` outside the index write lock so the file watcher is never starved. See
[`config.md`](config.md) for all knobs.
