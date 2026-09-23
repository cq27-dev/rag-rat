#!/bin/sh
# Verify the rag-rat plugin launcher (plugin/scripts/launch.js): it resolves a binary, runs it, and
# wires MCP stdio cleanly. POSIX sh so it runs under busybox (alpine) too.
#
# Usage: [RAG_RAT_BIN=/abs/rag-rat] [RAG_RAT_REQUIRE_HANDSHAKE=1] sh plugin/test/verify-launcher.sh [plugin-dir]
#   RAG_RAT_BIN set          → fast path; runs a real MCP initialize handshake against that binary.
#   RAG_RAT_REQUIRE_HANDSHAKE=1 (no RAG_RAT_BIN) → force the handshake via the launcher's DOWNLOAD path
#                              (the enclosing PLUGIN_ROOT's plugin.json version selects the release).
#   neither                  → only the no-binary paths (syntax, --no-install no-op); handshake skipped.
set -eu

PLUGIN_DIR="${1:-$(CDPATH= cd "$(dirname "$0")/.." && pwd)}"
LAUNCH="$PLUGIN_DIR/scripts/launch.js"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

command -v node >/dev/null 2>&1 || fail "node not found"
echo "node: $(node --version)"

node --check "$LAUNCH" || fail "launch.js failed --check"
pass "launcher syntax"

# --no-install must exit 0 even with no resolvable binary (hooks must never block). Use a throwaway
# plugin root pinned to a version nothing on PATH matches, and clear RAG_RAT_BIN for this check.
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
printf '{"version":"0.0.0-none"}\n' > "$TMP/plugin.json"
for harness in auto cursor vscode; do
  if env -u RAG_RAT_BIN PLUGIN_ROOT="$TMP" node "$LAUNCH" --no-install agent-hook "$harness" </dev/null >/dev/null 2>&1; then
    pass "--no-install $harness hook exits 0 (cold cache)"
  else
    fail "--no-install $harness hook did not exit 0 on a cold cache"
  fi
done

do_handshake=0
[ -n "${RAG_RAT_BIN:-}" ] && do_handshake=1
[ "${RAG_RAT_REQUIRE_HANDSHAKE:-0}" = "1" ] && do_handshake=1

if [ "$do_handshake" = "0" ]; then
  echo "SKIP: no RAG_RAT_BIN and handshake not required — download-path needs a published release."
  echo "launcher verification: OK (logic only)"
  exit 0
fi

if [ -n "${RAG_RAT_BIN:-}" ] && [ ! -x "$RAG_RAT_BIN" ]; then
  fail "RAG_RAT_BIN=$RAG_RAT_BIN is not executable here (glibc/arch mismatch — the binary's floor)"
fi

ver="$(node "$LAUNCH" --version 2>/dev/null | head -n1 || true)"
case "$ver" in
  "rag-rat "*) pass "launcher runs the binary ($ver)" ;;
  *) fail "unexpected --version via launcher: '$ver'" ;;
esac

req='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"ci","version":"0"}}}'
out="$(printf '%s\n' "$req" | timeout 45 node "$LAUNCH" mcp 2>/dev/null || true)"
case "$out" in
  *'"jsonrpc":"2.0"'*) pass "MCP initialize handshake returned a JSON-RPC result" ;;
  *) fail "no JSON-RPC result on stdout (binary run or stdio wiring)" ;;
esac
case "$out" in
  *"rag-rat-launch:"*) fail "launcher log leaked into stdout (must be stderr-only)" ;;
  *) pass "stdout clean of launcher logs" ;;
esac

echo "launcher verification: OK"
