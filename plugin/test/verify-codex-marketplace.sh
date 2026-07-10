#!/usr/bin/env bash
# Codex plugin validation. Codex has a shell `plugin marketplace add` (which validates the plugin
# manifests — the highest-value check, and what catches a mis-placed .mcp.json / hooks path) but NO
# shell `install` verb: per-plugin install is the interactive /plugins TUI. So this validates the
# manifest here and leaves the actual install to a tmux/expect follow-up (see plugin/test/README.md).
# Exploratory (continue-on-error in CI) until Codex's non-interactive surface is confirmed.
set -u
PLUGIN_DIR="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
fail() { echo "FAIL: $*" >&2; exit 1; }

command -v codex >/dev/null 2>&1 || fail "codex CLI not found"
echo "codex: $(codex --version 2>&1 | head -n1)"

echo "### add the local dir as a marketplace (validates plugin manifests)"
timeout 90 codex plugin marketplace add "$PLUGIN_DIR" </dev/null || fail "codex plugin marketplace add failed (manifest rejected?)"
timeout 30 codex plugin marketplace list </dev/null || true

echo
echo "NOTE: per-plugin install is the /plugins TUI — drive it via tmux/expect to finish L3."
echo "codex marketplace validation: OK"
