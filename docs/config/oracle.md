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
resolves the callees of **just-changed Rust files** through a resident `rust-analyzer` language
server and writes the same `edge_oracle` verdicts the batch pass writes, under a distinct `ra-lsp`
tool id. The batch pass (`auto_run` / `oracle run`) stays the canonical whole-checkout writer —
live rows are a freshness patch for files being edited, and where both tools cover the same edge
the batch verdict wins.

```toml
[oracle.live]
enabled = false               # off by default — opt in explicitly
idle_shutdown_secs = 900      # shut the language server down after 15 min idle
max_requests_per_pass = 200   # positive cap per maintenance pass; zero is rejected
```

**Standalone:** `[oracle.live]` does NOT imply or require `[oracle] auto_run`. Without a batch
baseline the live pass is *moniker-blind* — it upgrades edge confidence tiers under `local
ra-lsp-<n>` sentinel monikers, but clone-collapse (`find_clones` scip refine mode) and
moniker-anchored memory relocation get nothing until a batch `oracle run` completes (`oracle
status` says so when that's the case). Each completed batch run auto-upgrades subsequent live
passes to the real monikers. Live runs only from the resident watcher (never one-shot hook/CLI
maintenance passes, which would pay a language-server warm-up per invocation), and needs
`rust-analyzer` on `PATH`; probe and launch run from the checkout root so rustup directory overrides
apply. A missing tool degrades quietly like a missing embedding model.
