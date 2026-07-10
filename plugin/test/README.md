# Plugin install/run tests

Layered so each layer is as cheap and un-flaky as possible. Driven by `.github/workflows/plugin-test.yml`.

| Layer | What it proves | Auth? | Runner |
|---|---|---|---|
| **L1 launcher × glibc/musl** | the binary runs on a distro **and** the launcher wires MCP stdio | none | Docker matrix |
| **L2 Claude install** | `plugin.json`/`marketplace.json` validate; MCP + skills + hooks register | none | `npm i -g @anthropic-ai/claude-code` |
| **L2 Codex validate** | the Codex plugin manifest passes `marketplace add` validation | none | `npm i -g @openai/codex` |
| **L3 Codex install (TUI)** | end-to-end plugin install | none | tmux/expect — **not yet built** |
| **L4 session e2e** | a real prompt drives the MCP tool + hooks | **API key** | scheduled/manual — not built |

Installing/validating a plugin is offline config on both CLIs (no API key) — only L4 needs a key.

## Run locally

```bash
# L1 — launcher against a local build (full handshake):
RAG_RAT_BIN="$(command -v rag-rat)" sh plugin/test/verify-launcher.sh
# L1 — logic only (no binary): syntax + --no-install no-op; handshake skipped
sh plugin/test/verify-launcher.sh

# L2 — with the CLIs installed:
bash plugin/test/verify-claude-install.sh
bash plugin/test/verify-codex-marketplace.sh
```

## CI modes

- **PR / dispatch (no version):** build a debug `rag-rat`, run L1 across the matrix, and the Claude/Codex
  probes. In L1, `expect: pass` distros (glibc ≥ 2.38) must handshake; `expect: fail` distros (older
  glibc / musl) are **informational** (`continue-on-error`) and document where the binary's glibc floor
  bites — the same floor the release build chose.
- **Dispatch with `release_version=X.Y.Z`:** the `download-path` job skips the local build and makes the
  launcher **download** that release's assets in each distro, then handshakes — this validates the real
  download + checksum + extraction + per-distro run. Use it once a green cargo-dist release exists.

The Claude/Codex jobs are `continue-on-error` while their non-interactive behaviour is being confirmed
— surfacing, not gating, is the point. They also **empirically resolve the Codex layout** (root
`.mcp.json` vs `.codex-plugin/…`, where `hooks/hooks.json` must live, and whether Claude + Codex can
share one `hooks/hooks.json`) instead of guessing from docs.

## L3 (tmux) — the one interactive gap

Codex per-plugin install is the in-session `/plugins` TUI (no shell verb). To automate:

```bash
tmux new-session -d -s cx 'codex'
tmux send-keys -t cx '/plugins' Enter      # wait for the pane, then navigate + install
tmux capture-pane -t cx -p                 # assert the screen shows the plugin installed
```

Prefer `expect` / Python `pexpect` over raw tmux (programmatic match + timeout beats `sleep` +
screen-scrape). Keep it minimal — TUI layout/timing make it the most brittle layer.
