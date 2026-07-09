# rag-rat plugin (prototype)

A coding-agent plugin — for **Claude Code and Codex** (and any other harness with the same plugin
shape) — that bundles rag-rat's MCP server, skills, and hooks, and **installs a version-matched
rag-rat binary on first run**. Installing the plugin is one step: the user does not need to
`cargo install` / `brew install` / put `rag-rat` on `PATH` first.

## Layout (single source, shared across harnesses)

```
plugin/                          plugin root (CLAUDE_PLUGIN_ROOT / CODEX_PLUGIN_ROOT)
  scripts/launch.js                the all-OS launcher (shared)
  skills/                          shared skills: using-rag-rat, dream-review
  hooks/hooks.json                 Claude hooks (auto-discovered)
  .claude-plugin/plugin.json       Claude: mcpServers → node scripts/launch.js mcp
  .claude-plugin/marketplace.json
  .codex-plugin/plugin.json        Codex: mcpServers + skills + hooks references
  .codex-plugin/.mcp.json          → node scripts/launch.js mcp
  .codex-plugin/hooks/hooks.json   Codex hooks
```

Both harnesses treat `plugin/` as the plugin root, so `scripts/`, `skills/`, and the launcher are
shared — no per-harness duplication.

## Launcher — one-step, all-OS install (`scripts/launch.js`)

Node (not `.sh`) so it works on native Windows too. Resolution order, first hit wins:

1. `$RAG_RAT_BIN` — explicit override (a local dev build).
2. Managed cache — `<XDG_CACHE_HOME|~/.cache>/rag-rat/bin/<version>/rag-rat[.exe]` (version-exact).
3. Plugin-local `bin/<binary>`.
4. `PATH` rag-rat — only if `--version` matches the plugin's declared version.
5. Download — `rag-rat-<triple>.<ext>` + `.sha256` from the GitHub release, checksum-verified,
   extracted with `tar` (bsdtar reads `.tar.xz` and `.zip` on Win10+/macOS/Linux), atomically
   cached. Concurrent launches serialize on a lockfile.

Version comes from `plugin.json`. Intel Mac has no prebuilt → the launcher prints the source path
(`cargo install rag-rat --no-default-features --features model2vec`). A leading `--no-install`
(passed by hooks) resolves from an existing binary only and never blocks on a download — the MCP
server's launch is what installs the binary; until then a hook is a harmless no-op.

Not `npx @rag-rat/bin mcp` as the runtime: npx's shared `_npx/<hash>` staging races across concurrent
agent sessions (ENOENT) and re-resolves per launch. `@rag-rat/bin` stays a fine *install* channel.

## Skills

`using-rag-rat` and `dream-review`, copied from the repo's `.agents/skills/` source of truth. One
`skills/` dir serves every harness (a package step should regenerate it from `.agents/skills/` so the
copies never drift).

## Hooks

The hook command routes through the launcher: `node <launcher> --no-install claude-hook`, so it
resolves the same version-matched binary and never blocks on install.

**Claude — full parity** (`hooks/hooks.json`, replicates `rag-rat hooks install --claude`):
- `SessionStart` (`startup|clear|compact`, 5s) → repo orientation digest.
- `PreToolUse` on `Grep`/`Bash` (10s) → grep-augmentation.
- `PreToolUse` on `Write`/`Edit`/`MultiEdit` (10s) → write-time clone check.

**Codex — orientation digest** (`.codex-plugin/hooks/hooks.json`):
- `SessionStart` → the same orientation digest, via the harness-agnostic `claude-hook` (Codex uses
  the `{"hooks":{...}}` wrapper + `async` schema, not Claude's `timeout`).

### Non-Claude hook parity — what's still owed

The existing `rag-rat claude-hook` handler is mostly harness-agnostic (it reads a tolerant
`HookInput` and branches on `hook_event_name`), but two pieces are Claude-shaped:

- **grep-augmentation on Codex** depends on Codex firing a `PreToolUse` matcher on its shell tool
  with `tool_input.command` — unverified here; wire it once confirmed on-device.
- **write-time clone check on Codex/Cursor** matches `Write`/`Edit`/`MultiEdit`; those harnesses edit
  via `apply_patch`. Reaching parity needs a **rag-rat binary change**: a harness-neutral hook
  handler that recognizes `apply_patch` and parses its `tool_input`. Tracked as a follow-up (also a
  good moment to rename `claude-hook` → a neutral `agent-hook`).

## Installation for non-Claude agents

Two layers:
- **Universal (any MCP agent):** register `node <launcher> mcp` in the agent's MCP config — the
  launcher is harness-agnostic and self-installs the binary. This covers every MCP-capable agent.
- **Native plugin UX:** Claude + Codex manifests are bundled here. Cursor / Windsurf / others follow
  the same two files (a `plugin.json` + a `hooks.json`) pointed at the shared launcher.

## Testing

```bash
# Fast path against a local build (no release needed):
RAG_RAT_BIN="$(command -v rag-rat)" node scripts/launch.js --version

# MCP handshake through the launcher:
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
  | RAG_RAT_BIN="$(command -v rag-rat)" node scripts/launch.js mcp

# SessionStart hook through the launcher:
printf '%s\n' '{"hook_event_name":"SessionStart","source":"startup"}' \
  | RAG_RAT_BIN="$(command -v rag-rat)" node scripts/launch.js --no-install claude-hook
```

Verified: launcher syntax; `$RAG_RAT_BIN` override; `PATH` version-match; `--no-install` cold-cache
no-op; exit-code propagation; MCP `initialize` handshake (clean stdout/stderr); SessionStart digest.

## Status / open items

- **Download path (#5) untested** until a green cargo-dist release publishes prebuilt assets
  (v0.15.0's build failed). URL/asset naming matches cargo-dist output.
- **Windows extraction** relies on the bundled `tar` (bsdtar, Win10 1803+); needs a real Windows run.
- **Codex path resolution + hook schema** are inferred (basemind-style layout); need on-device
  verification of `mcpServers`/`skills`/`hooks` path bases and the hook-JSON field names.
- **apply_patch clone-check** for Codex/Cursor needs the rag-rat binary follow-up above.
- **Final placement**: a real Claude marketplace needs `.claude-plugin/marketplace.json` at the repo
  root; this prototype keeps everything under `plugin/` for isolated review.
- **First-run timing**: if the binary is not yet cached when a hook fires, the hook no-ops (by
  design) until the MCP server installs it.
