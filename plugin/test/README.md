# Plugin install/run tests

Layered so each layer is as cheap and un-flaky as possible. Driven by `.github/workflows/plugin-test.yml`.

| Layer | What it proves | Auth? | Runner |
|---|---|---|---|
| **L1 launcher × glibc/musl** | the binary runs on a distro **and** the launcher wires MCP stdio | none | Docker matrix |
| **L2 Claude install** | manifests validate; MCP + skills + **hooks** register (asserts `Hooks (≥1)`) | none | `npm i -g @anthropic-ai/claude-code` |
| **L2 Codex install** | `plugin add`/`list` install + enable; components stage into cache | none | `npm i -g @openai/codex` |
| **L4 session e2e** | a real prompt drives the MCP tool + hooks (and *which* Codex hooks fire) | **API key** | scheduled/manual — not built |

Installing/validating a plugin is **offline config on both CLIs** — no API key. Codex `>= 0.144` has
shell `plugin add` / `plugin list` (the earlier "/plugins TUI only" was stale), so **no tmux is
needed**. Only L4 — confirming hooks/MCP actually *fire* in a live session — is auth-gated.

## Run locally

```bash
# L1 — launcher against a local build (full handshake):
RAG_RAT_BIN="$(command -v rag-rat)" sh plugin/test/verify-launcher.sh
# L1 — logic only (no binary): syntax + --no-install no-op; handshake skipped
sh plugin/test/verify-launcher.sh

# L2 — with the CLIs installed:
bash plugin/test/verify-claude-install.sh
bash plugin/test/verify-codex-install.sh
```

## CI modes

- **PR / dispatch (no version):** build a debug `rag-rat`, run L1 across the matrix, and the Claude +
  Codex install jobs. In L1, `expect: pass` distros (glibc ≥ 2.38) must handshake; `expect: fail`
  distros (older glibc / musl) are **informational** (`continue-on-error`) and document where the
  binary's glibc floor bites — the same floor the release build chose.
- **Dispatch with `release_version=X.Y.Z`:** the `download-path` job skips the local build and makes the
  launcher **download** that release's assets in each distro, then handshakes — validates the real
  download + checksum + extraction + per-distro run. Use it once a green cargo-dist release exists.

The Claude/Codex install jobs are real gates (not `continue-on-error`): they assert the manifests
validate and the components register / stage. They empirically resolved the layout questions the docs
left ambiguous (Claude hooks need the `{"hooks":{…}}` wrapper; Codex accepts root `.mcp.json` + the
`.codex-plugin/` manifest and installs + enables cleanly).

## What's still auth-gated (L4, not built)

`plugin add` + `list` prove the plugin installs, enables, and stages its files — but not that the hooks
actually **fire** or the MCP tools **register** in a running session (that needs an authed
`claude -p` / `codex exec`). The one open question it would settle: **which** hooks file Codex loads
(root `hooks/hooks.json`, currently Claude's, vs `.codex-plugin/hooks/hooks.json`) and whether the
`^Bash$` / `^apply_patch$` matchers fire. Gate an L4 job behind an API-key secret to close it.
