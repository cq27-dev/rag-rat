# Claude Code grep augmentation

`rag-rat` augments Claude Code's `Grep` tool calls and `grep`/`rg`/`ag` Bash commands with symbol
and repo-memory context via the PreToolUse hook mechanism. When a grep pattern matches a known
symbol or a bound repo memory, the hook injects an `additionalContext` digest before the tool runs.
The hook never blocks a grep — it always exits 0.

## What gets injected

The payload has three lanes (in order, up to 1500 characters total):

1. **Repo memories** — `Invariant`, `Decision`, `Risk`, and other memories bound to the matched
   symbol, the search path, or the pattern text. These are the unique signal: grep and search tools
   can't surface them.
2. **Matching symbols** — location (`file:line`), kind, caller/callee edge counts, and signature for
   symbols whose name matches the pattern. Only triggered when the pattern looks like a code
   identifier (single token, no spaces, at least 3 characters).
3. **Lexical hits** — a few indexed source chunks (fallback lane, only active when no symbol match
   was found).

The pattern is normalized before matching: regex metacharacters become spaces, but dots and
double-colons inside identifier tokens are preserved so `Watcher::spawn` or `foo.bar` resolve
through the symbol lane.

## Install

The grep-augmentation hook ships with the **rag-rat plugin** — installing the plugin registers it in
one step (along with the MCP server, the agent skills, and the write-time clone-check and
session-digest hooks):

```bash
# Claude Code
claude plugin marketplace add cq27-dev/rag-rat
claude plugin install rag-rat@rag-rat

# Codex
codex plugin marketplace add cq27-dev/rag-rat
codex plugin add rag-rat@rag-rat
```

The plugin registers `PreToolUse` hooks on `Grep`/`Bash` (Claude Code) / `^Bash$` (Codex), each
invoking `rag-rat agent-hook` with a 10-second timeout. When invoked, `agent-hook` walks up from the
reported `cwd` looking for a `rag-rat.toml`. If none is found, the hook exits immediately without
printing anything — a silent no-op for repositories that are not indexed by rag-rat.

## How it serves

When `rag-rat mcp` is running (the normal configuration for Claude Code), its process also holds the
socket election for the current worktree. One listener wins per worktree and binds a Unix domain
socket. The hook client connects to that socket, sends the pattern and session ID in a
newline-delimited JSON request, and gets a reply within its 250 ms budget.

If no listener is running (MCP not active, or a race during startup), the hook client falls back to
a direct read-only SQLite query. The fallback is stateless: it produces the same payload lanes but
without per-session dedupe.

## Dedupe

The listener tracks per-session state: once a memory or symbol has been injected for a given Claude
Code session ID, it will not be injected again for the same session. The session map is in-memory;
dedupe resets when the listener restarts. The fallback path is stateless — the same content can be
injected on every grep when no listener is running.

## Troubleshooting

Set `RAG_RAT_HOOK_DEBUG=1` in the environment where `rag-rat mcp` runs to enable diagnostic output
from the listener. Errors encountered while serving individual hook requests are printed to stderr.
Without this variable, the listener is silent on errors; the hook client is always silent.
