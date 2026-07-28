# SCIP oracle auto-fresh (`[oracle]`)

Part of the [config reference](../config.md).

`[oracle]` controls the background auto-fresh SCIP oracle — compiler-grade ranking that keeps itself
current without a manual `rag-rat oracle run`. **Opt-in; off by default.** When enabled, the
long-lived `rag-rat mcp` server runs the oracle for the active checkout when its index is *stale*
(changed since the last run) and *quiet* (no recent edits), heavily throttled by two gates:

```toml
[oracle]
auto_run = false                 # off by default — opt in explicitly
auto_run_quiet_period_secs = 900   # run only after ~15 min with no index change (debounce)
auto_run_min_interval_secs = 21600 # and at most once every 6 h (floor)
```

Both gates are required, not redundant: producing a `.scip` takes minutes while edits arrive in
seconds, so debouncing a single burst is not enough — the quiet-period keeps a pass from firing
mid-session, and the min-interval floor caps how often it can run regardless of churn. The pass runs
on a **detached thread of the MCP server only** (never short-lived CLI/hook commands), uses the same
lock-free production path as `oracle run` (the slow subprocess runs OUTSIDE the index write lock; only
the brief join/write serializes), and is **fail-open** — any error, or a missing indexer tool, is a
silent no-op, and the thread dies with the server process. While auto-fresh is on, `important-symbols`
reports `heuristic ranking — compiler ranking refreshes in the background` instead of nudging you to
run the oracle by hand.

## Live LSP oracle (`[oracle.live]`)

The **live** oracle is the per-pass freshness path (#534): the resident watcher's maintenance pass
resolves the callees of **just-changed files** through a resident language server and writes the
same `edge_oracle` verdicts the batch pass writes, under a distinct live tool id. The batch pass
(`auto_run` / `oracle run`) stays the canonical whole-checkout writer — live rows are a freshness
patch for files being edited, and where both tools cover the same edge the batch verdict wins.

| Live tool | Language | Server | Extra requirement |
|---|---|---|---|
| `ra-lsp` | Rust | `rust-analyzer` | — |
| `ts-lsp` | TypeScript / TSX | `typescript-language-server` | a `tsconfig.json` project (see below) |
| `clangd-lsp` | C / C++ | `clangd` | a `compile_commands.json` in the checkout (see below) |

```toml
[oracle.live]
enabled = false               # off by default — opt in explicitly
idle_shutdown_secs = 900      # shut each language server down after 15 min idle
max_requests_per_pass = 200   # positive cap per maintenance pass; zero is rejected
```

One setting configures every live backend. A checkout indexed in several of these languages runs
one resident server per language, each with its own backlog and respawn backoff, so a wedged
server for one language never stalls another. `max_requests_per_pass` is **shared** across them —
it bounds the pass, which holds the repository write lock — and the backends take turns claiming
it so none is starved.

**TypeScript needs a `tsconfig.json`.** `typescript-language-server` reports that it has finished
loading a project only for a real tsconfig project, and the oracle waits for that signal before it
trusts an answer (asked mid-load, the server resolves an imported callee to the *import statement*
rather than the definition). The config does not have to be at the checkout root — a monorepo with
`packages/*/tsconfig.json` is fine — but a checkout with none is reported `Blocked` rather than run
blind.

**C/C++ needs a `compile_commands.json`, and clangd writes into the checkout.** clangd resolves a
call across translation units only through its background index, and it builds that index from the
compilation database — without one it answers with the callee's header declaration instead, and
reports no progress the oracle can wait on, so the backend is `Blocked`. A single usable database
may sit anywhere in the checkout: clangd itself only looks in an opened file's ancestor directories
and their `build/` subdirectory, so rag-rat finds it and points the server at it with
`--compile-commands-dir`. An out-of-tree CMake build (`build/`, `out/`, `cmake-build-debug/`, …)
therefore works unchanged. That index is persisted to `.cache/clangd/` *inside the checkout*: no
clangd flag or environment variable relocates it, and disabling it is what costs the
cross-translation-unit resolution. So enabling the live oracle in a C/C++ checkout means accepting
that write. rag-rat floors `.cache/clangd/` specifically — not `.cache/` itself, which a repo may
genuinely track for sources, and the floor is unconditional and cannot be whitelisted back. That
keeps a large, entirely machine-written tree out of the discovery walk and stops anything
source-shaped appearing there from being indexed as first-party code — the same category as
`.rag-rat`, a tool's own index living inside the checkout.

**Several databases in one checkout narrow what gets resolved.** `--compile-commands-dir` is
global, so when a checkout holds more than one `compile_commands.json` rag-rat passes it for none:
forcing one project's database onto another project's files would give them the wrong include and
define flags, and so a wrong definition. clangd's own per-file lookup decides instead, and it
searches only an opened file's ancestor directories and their `build/` subdirectory. A file whose
database is not there is **skipped** rather than resolved with fallback flags — a fallback-flags
answer resolves a cross-translation-unit call to the callee's header declaration, which is a wrong
verdict rather than a missing one. If that costs coverage, either move each database to where
clangd looks for it or keep one database per checkout.

**Standalone:** `[oracle.live]` does NOT imply or require `[oracle] auto_run`. Without a batch
baseline the live pass is *moniker-blind* — it upgrades edge confidence tiers under `local
<tool>-<n>` sentinel monikers, but clone-collapse (`find_clones` scip refine mode) and
moniker-anchored memory relocation get nothing until a batch `oracle run` completes (`oracle
status` says so when that's the case). Each completed batch run auto-upgrades subsequent live
passes to the real monikers. Live runs only from the resident watcher (never one-shot hook/CLI
maintenance passes, which would pay a language-server warm-up per invocation), and needs the
backend's server on `PATH`; probe and launch run from the checkout root so directory-scoped
toolchain overrides apply. A missing tool degrades quietly like a missing embedding model.
