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
  .mcp.json                        Codex MCP config (root, per Codex plugin spec)
  .claude-plugin/plugin.json       Claude: mcpServers → node scripts/launch.js mcp
  .claude-plugin/marketplace.json
  .codex-plugin/plugin.json        Codex manifest: mcpServers → ./.mcp.json, skills → ./skills/
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

The hook command routes through the launcher: `node <launcher> --no-install agent-hook`, so it
resolves the same version-matched binary and never blocks on install. The handler
(`rag-rat agent-hook`) is harness-neutral: Claude Code, Codex, and Cursor share the same
`hook_event_name` / `tool_name` / `tool_input` input and the same `hookSpecificOutput.additionalContext`
output.

**Claude** (`hooks/hooks.json`, replicates `rag-rat hooks install --claude`):
- `SessionStart` (`startup|clear|compact`, 5s) → repo orientation digest.
- `PreToolUse` on `Grep`/`Bash` (10s) → grep-augmentation.
- `PreToolUse` on `Write`/`Edit`/`MultiEdit` (10s) → write-time clone check.

**Codex** (`.codex-plugin/hooks/hooks.json`, `{"hooks":{…}}` wrapper):
- `SessionStart` → repo orientation digest.
- `PreToolUse` on `^Bash$` → grep-augmentation.
- `PreToolUse` on `^apply_patch$` → write-time clone check. The handler parses the V4A diff in
  `tool_input.command` for added lines (Codex/Cursor edit via `apply_patch`, not `Write`/`Edit`).

### Still to verify on-device

The Codex wiring is built from OpenAI's published hook docs (input field names, PreToolUse tool-name
regex matching, `hookSpecificOutput.additionalContext` output) but hasn't been exercised against a
live Codex. Confirm: the plugin `hooks.json` path/schema is read as expected, `^Bash$`/`^apply_patch$`
matchers actually fire, and the digest/clone-check output is injected. Cursor mirrors the same shape.

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
  | RAG_RAT_BIN="$(command -v rag-rat)" node scripts/launch.js --no-install agent-hook
```

Verified: launcher syntax; `$RAG_RAT_BIN` override; `PATH` version-match; `--no-install` cold-cache
no-op; exit-code propagation; MCP `initialize` handshake (clean stdout/stderr); SessionStart digest.

## Status / open items

- **Download path (#5) untested** until a green cargo-dist release publishes prebuilt assets
  (v0.15.0's build failed). URL/asset naming matches cargo-dist output.
- **Windows extraction** relies on the bundled `tar` (bsdtar, Win10 1803+); needs a real Windows run.
- **Codex path resolution + hook firing** are built from OpenAI's hook docs but unverified on a live
  Codex — confirm `.mcp.json` at the root resolves, and that Codex auto-discovers
  `.codex-plugin/hooks/hooks.json` (the `hooks` manifest field was dropped because Codex validation
  rejects it) and fires the `^Bash$` / `^apply_patch$` matchers. (The `apply_patch` V4A clone-check
  is implemented and unit-tested in the binary; only the end-to-end Codex wiring is unverified.)
- **Legacy hook-settings migration (deferred):** the `claude-hook` → `agent-hook` rename means a user
  who previously ran `rag-rat hooks install --claude` keeps a stale `rag-rat claude-hook` entry, and a
  fresh install adds a duplicate `agent-hook` one (`is_ours` only matches the new command). A clean
  fix updates the settings migration to recognize + replace the legacy command; deferred here since it
  touches `claude_settings.rs` migration semantics and the repo's pre-launch posture tolerates a
  hooks reinstall.
- **Final placement**: a real Claude marketplace needs `.claude-plugin/marketplace.json` at the repo
  root; this prototype keeps everything under `plugin/` for isolated review.
- **First-run timing**: if the binary is not yet cached when a hook fires, the hook no-ops (by
  design) until the MCP server installs it.
