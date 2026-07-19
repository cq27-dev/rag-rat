#!/usr/bin/env bash
# Non-interactive opencode plugin verification. opencode has no plugin-install CLI — a plugin is
# a TS/JS file in a plugins dir — so this verifies the shim itself end-to-end against a local
# rag-rat binary: MCP self-registration, grep augmentation, SessionStart digest injection, and
# the compaction hook. Set RAG_RAT_BIN to a built binary (e.g. target/debug/rag-rat).
set -u
PLUGIN_DIR="$(cd "$(dirname "$0")/.." && pwd)"
step() { echo; echo "### $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

command -v bun >/dev/null 2>&1 || fail "bun not found (opencode's plugin runtime)"
[ -n "${RAG_RAT_BIN:-}" ] || fail "set RAG_RAT_BIN to a built rag-rat binary"
[ -x "$RAG_RAT_BIN" ] || fail "RAG_RAT_BIN=$RAG_RAT_BIN is not executable"
echo "bun: $(bun --version)"; echo "rag-rat: $("$RAG_RAT_BIN" --version)"

step "drive the plugin hooks through bun"
# Run from an indexed repo so the hooks have something to augment. Session id is randomized —
# agent-hook dedups repeated searches per session, so a fixed id would make reruns report nothing.
out="$(cd "$PLUGIN_DIR/.." && RAG_RAT_BIN="$RAG_RAT_BIN" bun -e '
import("./plugin/opencode/rag-rat.ts").then(async (m) => {
  const sid = "verify-" + Date.now();
  const hooks = await m.RagRat({ directory: process.cwd() });
  const result = {};
  const cfg = {};
  await hooks.config(cfg);
  result.mcp = cfg.mcp?.["rag-rat"]?.command?.join(" ") ?? "";
  const grepOut = { title: "t", output: "RESULT", metadata: {} };
  await hooks["tool.execute.after"](
    { tool: "grep", sessionID: sid, callID: "c1", args: { pattern: "HookInput" } }, grepOut);
  result.grep = grepOut.output;
  await hooks.event({ event: { type: "session.created", properties: { info: { id: sid } } } });
  const sys = { system: [] };
  await hooks["experimental.chat.system.transform"]({ sessionID: sid }, sys);
  result.system = sys.system.join(" ");
  const comp = { context: [] };
  await hooks["experimental.session.compacting"]({ sessionID: sid }, comp);
  result.compact = comp.context.join(" ");
  console.log(JSON.stringify(result));
})' 2>&1)" || fail "bun run failed: $out"
echo "$out" | head -c 400; echo

echo "$out" | grep -q '@rag-rat/bin@[0-9][^"]* mcp' || fail "config hook did not register the pinned MCP server"
pass "config hook registers the pinned MCP server"
echo "$out" | grep -q "rag-rat index context" || fail "grep augmentation missing from tool.execute.after output"
pass "grep augmentation appended to the tool result"
echo "$out" | grep -q "repo intelligence" || fail "SessionStart digest missing from system.transform"
pass "SessionStart digest injected via system.transform"
echo "$out" | grep -q "repo intelligence" || fail "compaction digest missing"
pass "compaction digest injected"

step "cold-cache no-op (the --no-install contract)"
# An unresolvable RAG_RAT_BIN must make every hook a silent no-op, never an error.
noop="$(cd "$PLUGIN_DIR/.." && RAG_RAT_BIN=/nonexistent/rag-rat bun -e '
import("./plugin/opencode/rag-rat.ts").then(async (m) => {
  const hooks = await m.RagRat({ directory: process.cwd() });
  const out = { title: "t", output: "RESULT", metadata: {} };
  await hooks["tool.execute.after"](
    { tool: "grep", sessionID: "s", callID: "c", args: { pattern: "x" } }, out);
  const sys = { system: [] };
  await hooks["experimental.chat.system.transform"]({ sessionID: "s" }, sys);
  console.log(JSON.stringify({ tool: out.output, sys: sys.system.length }));
})' 2>&1)" || fail "no-op path threw: $noop"
echo "$noop"
echo "$noop" | grep -q '"tool":"RESULT"' || fail "no-op path mutated the tool output"
echo "$noop" | grep -q '"sys":0' || fail "no-op path injected a system entry"
pass "hooks are silent no-ops when no binary is resolvable"

echo
echo "opencode plugin verification: OK"
