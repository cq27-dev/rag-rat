# rag-rat plugin (prototype)

A coding-agent plugin — for **Claude Code, Codex, and opencode** (and any other harness with the
same plugin shape) — that bundles rag-rat's MCP server, skills, and hooks, and **installs a
version-matched rag-rat binary on first run**. Installing the plugin is one step: the user does
not need to `cargo install` / `brew install` / put `rag-rat` on `PATH` first.

## Layout (single source, shared across harnesses)

```
plugin/                          plugin root (CLAUDE_PLUGIN_ROOT / CODEX_PLUGIN_ROOT)
  scripts/launch.js                the all-OS hook launcher (resolves the binary; never installs)
  skills/                          shared skills: using-rag-rat, dream-review
  hooks/hooks.json                 Claude hooks (auto-discovered)
  .mcp.json                        Codex MCP config (root) → npx @rag-rat/bin mcp
  .claude-plugin/plugin.json       Claude: mcpServers → npx @rag-rat/bin mcp
  .claude-plugin/marketplace.json
  .codex-plugin/plugin.json        Codex manifest: mcpServers → ./.mcp.json, skills → ./skills/
  .codex-plugin/hooks/hooks.json   Codex hooks
  opencode/rag-rat.ts              opencode plugin (TS module; MCP self-registration + hooks)
```

Both harnesses treat `plugin/` as the plugin root, so `scripts/`, `skills/`, and the launcher are
shared — no per-harness duplication.

## MCP server & hook launcher

**MCP server** — both manifests launch it as `npx -y @rag-rat/bin@latest mcp`. npx runs in the
agent's project working directory (so the server resolves that repo's `rag-rat.toml`) and installs the
version-matched binary from the `@rag-rat/bin` npm package on first use — no `cargo install` / `brew`
/ `PATH` setup first.

**Hooks** — route through `scripts/launch.js` in resolve-only mode
(`node <launcher> --no-install agent-hook`), never npx: a hook must be a fast no-op when the binary
isn't present yet, never blocking a tool call on an install or a network fetch. (The launcher can also
be an MCP runtime — `node <launcher> mcp`, with install — for any other MCP agent that isn't
Claude/Codex.) Node, not `.sh`, so it works on native Windows too. Resolution order, first hit wins:

1. `$RAG_RAT_BIN` — explicit override (a local dev build).
2. Managed cache — `<XDG_CACHE_HOME|~/.cache>/rag-rat/bin/<version>/rag-rat[.exe]` (version-exact).
3. The `@rag-rat/bin` npx cache — the binary the MCP server's `npx` run already staged, so a
   `--no-install` hook resolves a real binary without a download of its own.
4. `PATH` rag-rat — only if `--version` matches the plugin's declared version.
5. Download — `rag-rat-<triple>.<ext>` + `.sha256` from the GitHub release, checksum-verified,
   extracted with `tar` (bsdtar reads `.tar.xz` and `.zip` on Win10+/macOS/Linux), atomically cached,
   lockfile-serialized. **Skipped under `--no-install`** — until the MCP server's first `npx` run has
   installed the binary, a hook is a harmless no-op.

Version comes from `plugin.json`. Intel Mac has no prebuilt → the launcher prints the source path
(`cargo install rag-rat --no-default-features --features model2vec`).

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

**Claude** (`hooks/hooks.json`, auto-discovered):
- `SessionStart` (`startup|clear|compact`, 5s) → repo orientation digest.
- `PreToolUse` on `Grep`/`Bash` (10s) → grep-augmentation.
- `PreToolUse` on `Write`/`Edit`/`MultiEdit` (10s) → write-time clone check.

**Codex** (`.codex-plugin/hooks/hooks.json`, `{"hooks":{…}}` wrapper):
- `SessionStart` → repo orientation digest.
- `PreToolUse` on `^Bash$` → grep-augmentation.
- `PreToolUse` on `^apply_patch$` → write-time clone check. The handler parses the V4A diff in
  `tool_input.command` for added lines (Codex/Cursor edit via `apply_patch`, not `Write`/`Edit`).

**opencode** (`opencode/rag-rat.ts`, a TS plugin module — no hooks manifest exists there):
- `config` hook → self-registers the `rag-rat` MCP server (`npx -y @rag-rat/bin@<version> mcp`)
  unless the user's `opencode.json` already defines one.
- `session.created` event → orientation digest, injected into the system prompt via
  `experimental.chat.system.transform`; `experimental.session.compacting` re-injects a fresh
  digest into the compaction prompt (parity with Claude's `SessionStart(compact)`).
- `tool.execute.after` on `grep`/`bash`/`read` → grep/read augmentation. opencode's
  `tool.execute.before` has no additionalContext channel, so the context rides **on the tool
  result** instead of preceding it.
- `tool.execute.after` on `write`/`edit` → PostToolUse scoped reindex + write-time clone check.
- The shim translates opencode's lowercase/camelCase tool calls (`grep`, `filePath`, `newString`)
  to the harness-neutral `agent-hook` payload (`Grep`, `file_path`, `new_string`) — the Rust hook
  handler is unchanged. Every path fails silent (never blocks a tool call), mirroring the
  `--no-install` contract.

### Installing in opencode

Two options:
- **Whole bundle** (keeps the shared launcher): copy or symlink the repo's `plugin/` tree
  somewhere stable and symlink `plugin/opencode/rag-rat.ts` into `~/.config/opencode/plugins/`
  (global) or `.opencode/plugins/` (project). The shim finds `../scripts/launch.js` relative to
  its real path.
- **Single file**: drop `opencode/rag-rat.ts` alone into the plugins dir. The shim then resolves
  the binary itself with the launcher's `--no-install` policy (`$RAG_RAT_BIN` → managed cache →
  npx cache → version-matched `PATH`); hooks no-op until the MCP server's first `npx` run has
  installed the binary. Its type-only `@opencode-ai/plugin` import is erased at compile time, so
  no `node_modules` is needed.

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

A CI harness (`plugin/test/` + `.github/workflows/plugin-test.yml`) runs the launcher across a Linux
glibc/musl Docker matrix and drives non-interactive Claude/Codex plugin install + manifest
validation — see [`plugin/test/README.md`](test/README.md).

## Status / open items

Verified end-to-end:

- **Download path** — against the v0.16.0 release: PATH version-mismatch detection → download the
  platform archive → sha256 verify → extract → atomic install to the version-exact cache → run.
  (Was blocked on a green release; v0.16.0 provides the assets.)
- **Marketplace placement** — the manifest lives at the repo root (`.claude-plugin/marketplace.json`,
  `source: ./plugin`), so `/plugin marketplace add cq27-dev/rag-rat` discovers it; the plugin content
  stays under `plugin/`.

Remaining:

- **Plugin version tracks the release (automated).** The launcher fetches
  `releases/download/v<plugin-version>`, so the manifests' `version` must match the released crate
  version. `.github/workflows/release-plz.yml` runs `tools/sync-plugin-version.mjs` on the Release PR
  to bump all three manifests in lockstep with `Cargo.toml` — no manual step (proves out on the next
  release). Aside: a release binary that carries a `+g<hash>` version suffix won't satisfy the strict
  PATH `--version` match, so the launcher downloads rather than reusing an on-PATH release build
  (correct, just not optimal).
- **Windows extraction** relies on the bundled `tar` (bsdtar, Win10 1803+); needs a real Windows run.
- **Codex hook *firing*** — install is verified (CI `codex-install`), but whether the `^Bash$` /
  `^apply_patch$` hooks fire in a live session (and **which** hooks file Codex loads — root
  `hooks/hooks.json` vs `.codex-plugin/hooks/hooks.json`) needs an authed session; deliberately out of
  scope (keyless smoke test). The `apply_patch` V4A clone-check is implemented + unit-tested.
- **First-run timing**: if the binary is not yet cached when a hook fires, the hook no-ops (by design)
  until the MCP server installs it.
