# Index freshness: the watcher and git hooks (`[watch]`)

Part of the [config reference](../config.md).

`[watch]` controls the background file watcher that keeps the index fresh as files change (new
files, uncommitted edits) so graph/symbol queries reflect the working tree without a commit:

```toml
[watch]
enabled = true        # on by default; false (or RAG_RAT_NO_WATCH=1) disables it
debounce_ms = 400     # quiet window before a reindex pass
max_latency_ms = 2500 # force a pass after this much continuous activity (starvation cap)
periodic_sweep_secs = 300 # backstop pass at least this often (0 disables) — set for NFS/WSL
pass_cooldown_secs = 60   # minimum gap between event-driven passes (0 disables)
overlay_quiet_secs = 300  # linked-worktree quiet window for uncommitted edits (0 disables)
```

`pass_cooldown_secs` paces the watcher under sustained editing: the next event-driven pass starts
no sooner than this long after the previous pass completed, so long agent sessions can't run
minutes-long passes back-to-back. The trade-off is up to that much added indexing latency after a
pass completes; `periodic_sweep_secs` remains the staleness bound, and the periodic sweep is never
held back by the cooldown.

`overlay_quiet_secs` paces the per-worktree overlay refresh the same way, per linked worktree:
when a pass's events for a checkout are dirty-only (neither its HEAD nor the base HEAD has moved
since the last complete refresh), the refresh — a base↔branch tree diff plus a working-tree status
walk — is skipped until the window since that refresh elapses. A commit or checkout in any
worktree refreshes immediately; only uncommitted-edit visibility in worktree-scoped queries is
deferred. A deferred edit surfaces on the first event-driven pass after the window elapses (under
sustained editing), or at latest on the next periodic sweep (a one-off edit with no follow-up
event) — sweep-style passes (startup catch-up, the periodic sweep, gc, the hook-driven
`maintenance` command) ignore the window, so `periodic_sweep_secs` stays a staleness bound here
too. When the sweep is disabled (`periodic_sweep_secs = 0`) the window is ignored entirely: a
deferred one-off edit would otherwise have no later pass to surface it.

The watcher runs inside `rag-rat mcp` automatically, and on demand via `rag-rat index --watch`. It
watches the configured target directories recursively and runs the discover → reconcile → gc →
memory_validate pipeline on debounced bursts. One watcher per worktree and one writer at a time per
index are enforced with file locks under the index directory; the index DB is shared across a repo's
worktrees (a relative `database` resolves against the main worktree). File locks are unreliable on
NFS and WSL2 `drvfs`/`9p` (`/mnt/...`) mounts — keep the repo on a native filesystem.

## Git hooks (`rag-rat hooks install`)

`rag-rat hooks install` writes generated `post-checkout`, `post-merge`, `post-rewrite`, and
`post-commit` hooks to the current worktree's Git hooks directory. Those hooks call `rag-rat
maintenance --max-seconds 30` in the background so branch switches, merges, rebases, and commits
refresh the current worktree index and advance changed-first embedding reconciliation without
blocking normal Git operations. Each maintenance pass also runs a worktree-safe `gc` that prunes
index rows for commits no longer held by any live worktree (run `rag-rat gc` to prune on demand).
