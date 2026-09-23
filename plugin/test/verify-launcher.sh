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

# ---- PATH shim (~/.local/bin/rag-rat) --------------------------------------------------------
# Stub binaries in a fake managed cache stand in for releases, so these run without a download.
SHIM_CACHE="$TMP/cache"
SHIM_DIR="$TMP/shim"
stub() { # stub <version>: a managed-cache binary that reports that version
  d="$SHIM_CACHE/rag-rat/bin/$1"
  mkdir -p "$d"
  printf '#!/bin/sh\necho "rag-rat %s"\n' "$1" > "$d/rag-rat"
  chmod +x "$d/rag-rat"
}
launch_as() { # launch_as <plugin version> [env...]: one launcher run under that plugin version
  v="$1"; shift
  mkdir -p "$TMP/p-$v"
  printf '{"version":"%s"}\n' "$v" > "$TMP/p-$v/plugin.json"
  env -u RAG_RAT_BIN PLUGIN_ROOT="$TMP/p-$v" XDG_CACHE_HOME="$SHIM_CACHE" RAG_RAT_SHIM_DIR="$SHIM_DIR" \
    "$@" node "$LAUNCH" --no-install --version </dev/null >/dev/null 2>&1 || fail "launcher run as $v failed"
}
target() { readlink "$SHIM_DIR/rag-rat" 2>/dev/null || echo "<none>"; }
stub 1.2.0
stub 1.3.0

launch_as 1.2.0 RAG_RAT_NO_PATH_SHIM=1
[ ! -e "$SHIM_DIR/rag-rat" ] || fail "RAG_RAT_NO_PATH_SHIM=1 still created a shim"
pass "shim: opt-out creates nothing"

launch_as 1.2.0
[ "$(target)" = "$SHIM_CACHE/rag-rat/bin/1.2.0/rag-rat" ] || fail "shim not created: $(target)"
[ "$("$SHIM_DIR/rag-rat" --version)" = "rag-rat 1.2.0" ] || fail "shim does not run the binary"
pass "shim: created, pointing at the managed binary"

launch_as 1.3.0
[ "$(target)" = "$SHIM_CACHE/rag-rat/bin/1.3.0/rag-rat" ] || fail "shim not upgraded: $(target)"
pass "shim: a newer plugin re-points it"

launch_as 1.2.0
[ "$(target)" = "$SHIM_CACHE/rag-rat/bin/1.3.0/rag-rat" ] || fail "shim downgraded: $(target)"
pass "shim: an older plugin does not downgrade it"

rm -rf "$SHIM_CACHE/rag-rat/bin/1.3.0"
launch_as 1.2.0
[ "$(target)" = "$SHIM_CACHE/rag-rat/bin/1.2.0/rag-rat" ] || fail "dangling shim kept: $(target)"
pass "shim: a dangling link is replaced"

# The npx cache is linked too, when that is where the plugin's binary is.
NPX_BIN="$TMP/npm/_npx/abc123/node_modules/@rag-rat/bin/node_modules/.bin_real"
mkdir -p "$NPX_BIN"
printf '#!/bin/sh\necho "rag-rat 1.5.0"\n' > "$NPX_BIN/rag-rat"
chmod +x "$NPX_BIN/rag-rat"
launch_as 1.5.0 npm_config_cache="$TMP/npm"
[ "$(target)" = "$NPX_BIN/rag-rat" ] || fail "npx-cached binary not linked: $(target)"
pass "shim: links the npx-cached binary"

ln -sf /usr/bin/true "$SHIM_DIR/rag-rat"
stub 1.4.0
launch_as 1.4.0
[ "$(target)" = "/usr/bin/true" ] || fail "foreign symlink replaced: $(target)"
pass "shim: a symlink it did not create is left alone"

rm -f "$SHIM_DIR/rag-rat"
printf '#!/bin/sh\necho mine\n' > "$SHIM_DIR/rag-rat"
launch_as 1.4.0
[ "$(cat "$SHIM_DIR/rag-rat")" = "$(printf '#!/bin/sh\necho mine')" ] || fail "user's own rag-rat replaced"
pass "shim: a user's own rag-rat is left alone"

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
