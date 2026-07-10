#!/usr/bin/env bash
# Non-interactive Codex plugin install + verification. codex-cli >= 0.144 exposes shell `plugin add`
# / `plugin list` verbs — the earlier "/plugins TUI only" claim is stale, so no tmux is needed.
# Install/validate is offline config (no API key); auth is only for a running session. This verifies
# the manifest is accepted, the plugin installs + enables, and the component files stage into cache.
set -u
PLUGIN_DIR="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
step() { echo; echo "### $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

command -v codex >/dev/null 2>&1 || fail "codex CLI not found"
echo "codex: $(codex --version 2>&1 | head -n1)"

step "add the local dir as a marketplace"
timeout 90 codex plugin marketplace add "$PLUGIN_DIR" </dev/null || fail "marketplace add failed (marketplace.json rejected?)"

step "install the plugin non-interactively (validates the plugin manifest)"
out="$(timeout 90 codex plugin add rag-rat@rag-rat </dev/null 2>&1)"; echo "$out"
echo "$out" | grep -qiE "Installed plugin root|Added plugin" || fail "codex plugin add did not install rag-rat"
pass "plugin installed"

step "list installed plugins (expect installed + enabled)"
lst="$(timeout 30 codex plugin list </dev/null 2>&1)"; echo "$lst"
echo "$lst" | grep -qE "rag-rat@rag-rat" || fail "codex plugin list does not show rag-rat"
echo "$lst" | grep -qiE "enabled" || fail "rag-rat is not enabled"
pass "plugin installed + enabled"

step "assert the staged component files in the cache"
cache="$(printf '%s\n' "$out" | sed -n 's/.*Installed plugin root: //p' | head -n1)"
[ -n "$cache" ] || cache="$(find "$HOME/.codex/plugins/cache/rag-rat/rag-rat" -maxdepth 1 -type d -name '*.*' 2>/dev/null | head -n1)"
echo "cache root: $cache"
[ -f "$cache/.mcp.json" ] && pass "MCP config staged" || fail "cache missing .mcp.json"
[ -f "$cache/hooks/hooks.json" ] && pass "root hooks/hooks.json staged" || fail "cache missing hooks/hooks.json"
{ [ -d "$cache/skills/using-rag-rat" ] && [ -d "$cache/skills/dream-review" ]; } && pass "both skills staged" || fail "cache missing skills"
[ -f "$cache/scripts/launch.js" ] && pass "launcher staged" || fail "cache missing scripts/launch.js"

echo
echo "NOTE: whether Codex actually FIRES the hooks + registers the MCP tools in a LIVE session is"
echo "      auth-gated (L4). Install + manifest validity + component staging are verified here; the"
echo "      correct Codex hooks *location* (root hooks/ vs .codex-plugin/hooks/) still needs a session."
echo "codex install verification: OK"
