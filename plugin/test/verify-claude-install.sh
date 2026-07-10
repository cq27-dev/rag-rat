#!/usr/bin/env bash
# Non-interactive Claude Code plugin install + validation. Installing/validating a plugin is offline
# config (no API key) — the shell subcommands `claude plugin marketplace add` / `plugin install` /
# `plugin details` exist for exactly this. Stdin is /dev/null and every call is timed so a stray
# trust/confirm prompt can't hang CI. This job is exploratory (continue-on-error in CI) until the
# non-interactive behaviour is confirmed.
set -u
PLUGIN_DIR="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
step() { echo; echo "### $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

command -v claude >/dev/null 2>&1 || fail "claude CLI not found"
echo "claude: $(claude --version 2>&1 | head -n1)"

step "add the local marketplace (validates plugin/.claude-plugin/marketplace.json + plugin.json)"
timeout 90 claude plugin marketplace add "$PLUGIN_DIR" </dev/null || fail "marketplace add failed"
timeout 30 claude plugin marketplace list </dev/null || true

step "install the plugin to user scope (non-interactive)"
timeout 90 claude plugin install rag-rat@rag-rat --scope user </dev/null || fail "plugin install failed"

step "inspect the installed plugin (expect MCP server + skills + hooks, no load errors)"
det="$(timeout 30 claude plugin details rag-rat </dev/null 2>&1)"; echo "$det"
echo "$det" | grep -qiE "rag-rat" || fail "plugin details did not report rag-rat"

step "on-disk state"
ls -la "$HOME/.claude/plugins" 2>/dev/null || true

echo "claude install verification: OK"
