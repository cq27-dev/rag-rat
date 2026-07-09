# rag-rat plugin — self-installing MCP launcher (prototype)

A coding-agent plugin that registers rag-rat's MCP server through a launcher which **installs a
version-matched rag-rat binary on first run**. Installing the plugin is one step — the user does not
need to `cargo install` / `brew install` / put `rag-rat` on `PATH` first.

The launcher fetches cargo-dist's release assets and is written in Node so it works on **every OS**
(a `.sh` launcher would break native Windows).

## Layout

```
scripts/mcp-launch.js      the launcher (Node, all-OS)
.claude-plugin/plugin.json     Claude Code plugin → node scripts/mcp-launch.js mcp
.claude-plugin/marketplace.json
.codex-plugin/plugin.json      Codex plugin
.codex-plugin/.mcp.json        → node scripts/mcp-launch.js mcp
```

Both harnesses point their MCP `command` at the same Node launcher, resolved from each harness's
plugin-root env var (`CLAUDE_PLUGIN_ROOT` / `CODEX_PLUGIN_ROOT`).

## How the launcher resolves the binary (first hit wins)

1. `$RAG_RAT_BIN` — explicit override (a local dev build); used unconditionally.
2. Managed cache — `<XDG_CACHE_HOME|~/.cache>/rag-rat/bin/<version>/rag-rat[.exe]` (version-exact).
3. Plugin-local `bin/<binary>` — a pre-seeded binary shipped next to the plugin.
4. `PATH` rag-rat — only if its `--version` matches the plugin's declared version.
5. Download — fetch `rag-rat-<triple>.<ext>` + `.sha256` from the GitHub release, verify the
   checksum, extract with `tar` (bsdtar reads both `.tar.xz` and `.zip` on Win10+/macOS/Linux),
   atomically install into the cache. Concurrent launches serialize on a lockfile.

Version comes from `plugin.json` (single source of truth). Intel Mac (`x86_64-apple-darwin`) has no
prebuilt — the launcher exits with the source-install path
(`cargo install rag-rat --no-default-features --features model2vec`).

Not `npx @rag-rat/bin mcp` as the runtime: `npx` stages into a shared `_npx/<hash>` dir, so
concurrent agent sessions race and fail with `ENOENT`, and it re-resolves over the network every
launch. `@rag-rat/bin` stays a fine *install* channel — just not the per-launch runtime.

## Testing it

```bash
# Fast path against a local build (works today, no release needed):
RAG_RAT_BIN="$(command -v rag-rat)" node scripts/mcp-launch.js --version

# Real MCP handshake through the launcher:
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
  | RAG_RAT_BIN="$(command -v rag-rat)" node scripts/mcp-launch.js mcp
# → JSON-RPC result on stdout; launcher logs on stderr.
```

Verified so far: syntax; `$RAG_RAT_BIN` override; `PATH` version-match; exit-code propagation;
end-to-end MCP `initialize` handshake with clean stdout/stderr separation.

## Status / open items

- **Download path (#5) is untested** until a cargo-dist release publishes prebuilt assets — v0.15.0's
  release build failed, so no `rag-rat-<triple>.tar.xz` assets exist yet; the first green release
  (v0.15.1+) is needed to exercise it live. URL/asset naming matches cargo-dist's output.
- **Windows extraction** relies on the bundled `tar` (bsdtar, Win10 1803+); needs a real Windows run
  to confirm. Fallback (PowerShell `Expand-Archive`) not yet wired.
- **Final placement**: for a real Claude marketplace, `.claude-plugin/marketplace.json` must sit at
  the marketplace repo root; this prototype keeps everything under `plugin/` for isolated review.
- **Skills / hooks / commands** are not bundled yet — this prototype is only the launcher + MCP
  registration. rag-rat already has skills (`.agents/skills`) and a Claude hook to wire in next.
- **Checksum format**: assumes cargo-dist's `<hash>  <file>` `.sha256` sibling; confirm against a
  real release artifact.
