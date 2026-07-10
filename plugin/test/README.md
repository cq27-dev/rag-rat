# Plugin install/run tests

Layered so each layer is as cheap and un-flaky as possible. Driven by `.github/workflows/plugin-test.yml`.

| Layer | What it proves | Auth? | Runner |
|---|---|---|---|
| **L1 launcher × glibc/musl** | the binary runs on a distro **and** the launcher wires MCP stdio | none | Docker matrix |
| **L2 Claude install** | manifests validate; MCP + skills + **hooks** register (asserts `Hooks (≥1)`) | none | `npm i -g @anthropic-ai/claude-code` |
| **L2 Codex install** | `plugin add`/`list` install + enable; components stage into cache | none | `npm i -g @openai/codex` |

This is a **keyless smoke test** — install + validate + component staging on both CLIs, all offline
config (no API key). Codex `>= 0.144` has shell `plugin add` / `plugin list` (the earlier "/plugins TUI
only" was stale), so **no tmux is needed** either. Confirming hooks/MCP actually *fire* in a live
session would need an API key and is **deliberately out of scope** — see the note at the bottom.

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

- **PR / dispatch (no version):** build a debug `rag-rat`, run L1 across a **modern-glibc** matrix
  (`ubuntu:24.04`, `fedora:41`), and the Claude + Codex install jobs. Old OSes and Alpine/musl are out
  of scope — `ort`/ONNX is glibc-only, so a musl build would need a separate model2vec-only pipeline
  (deemed not worth it).
- **Dispatch with `release_version=X.Y.Z`:** the `download-path` job skips the local build and makes the
  launcher **download** that release's assets in each distro, then handshakes — validates the real
  download + checksum + extraction + per-distro run. Use it once a green cargo-dist release exists.

The Claude/Codex install jobs are real gates (not `continue-on-error`): they assert the manifests
validate and the components register / stage. They empirically resolved the layout questions the docs
left ambiguous (Claude hooks need the `{"hooks":{…}}` wrapper; Codex accepts root `.mcp.json` + the
`.codex-plugin/` manifest and installs + enables cleanly).

## Out of scope by choice — in-session firing (would need an API key)

`plugin add` + `list` prove the plugin installs, enables, and stages its files. They do **not** prove
the hooks actually **fire** or the MCP tools **register** in a running session — that needs an authed
`claude -p` / `codex exec`, and we're **not** standing up API keys just for a hook test. The harness
stops at the keyless smoke-test boundary on purpose.

The one thing a keyed session would additionally settle (noted, not owed): **which** hooks file Codex
loads (root `hooks/hooks.json`, currently Claude's, vs `.codex-plugin/hooks/hooks.json`) and whether
the `^Bash$` / `^apply_patch$` matchers fire. Left for a manual check if it ever matters.
