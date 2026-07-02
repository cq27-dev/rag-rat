#!/usr/bin/env bash
# Reproduce "git action -> local embedding -> items remaining" in an ISOLATED clone.
#
# Git hooks (post-commit/checkout/merge) spawn `rag-rat maintenance`, which runs a time-boxed,
# light/incremental reconcile that embeds changed chunks against the LOCAL query_endpoint. This
# harness fires those hooks against a throwaway clone with `[log] enabled`, then prints the resulting
# debug-log narrative + `doctor` so you can see WHICH phase embeds and why the plan backlog stays
# non-empty (often the unscoped cross-worktree count — see issue #360).
#
# It runs ENTIRELY OUTSIDE the source checkout (own temp dir, DB, logs) and fails closed if anything
# resolves back inside it, so it never touches an index other tools/agents are using.
set -euo pipefail

MODE="local"
REPO=""
while [ $# -gt 0 ]; do
  case "$1" in
    --local) MODE="local" ;;
    --hybrid) MODE="hybrid" ;;
    --repo) REPO="${2:?--repo needs a path or url}"; shift ;;
    -h | --help)
      echo "usage: $0 [--local|--hybrid] [--repo <url|path>]"
      echo "  --local   (default) embed against local infinity only (connect mode); no Modal"
      echo "  --hybrid  mirror the ephemeral setup: Modal L4 for the big reconcile + local infinity for queries"
      exit 0
      ;;
    *) echo "usage: $0 [--local|--hybrid] [--repo <url|path>]" >&2; exit 2 ;;
  esac
  shift
done

# The repo to clone: an explicit --repo, else the checkout this script lives in (rag-rat itself is a
# large-enough corpus that the reconcile is time-boxed and "Partial" shows).
script_dir="$(cd "$(dirname "$0")" && pwd)"
source_root="$(git -C "$script_dir" rev-parse --show-toplevel)"
src="${REPO:-$source_root}"

work="$(mktemp -d "${TMPDIR:-/tmp}/rag-rat-repro.XXXXXX")"
# Fail closed: the work dir must NOT be inside the source checkout.
case "$work/" in
  "$source_root"/*) echo "refusing: work dir '$work' is inside the source checkout" >&2; exit 1 ;;
esac
echo "source: $src"
echo "work:   $work"

clone="$work/repo"
git clone --quiet "$src" "$clone"
cd "$clone"

# Both modes embed QUERIES (and the light/incremental hook path) against local infinity, so it must
# be reachable. The `infinity` backend requires an explicit query_endpoint (only Ollama defaults).
if ! curl -fsS "http://localhost:7997/health" >/dev/null 2>&1; then
  echo "local infinity not reachable at http://localhost:7997 (needed for query/light embedding)" >&2
  exit 1
fi

if [ "$MODE" = "hybrid" ]; then
  cat > rag-rat.toml <<'TOML'
[index]
root = "."
[target_bindings]
rust = ["crates"]
[log]
enabled = true
level = "debug"
[llm.embedding]
model = "jinaai/jina-embeddings-v2-base-code"
[llm.embedding.remote]
cookbook = "@rag-rat/cookbook modal"
backend = "infinity"
gpu = "L4"
query_endpoint = "http://localhost:7997"
batch_size = 8
TOML
else
  cat > rag-rat.toml <<'TOML'
[index]
root = "."
[target_bindings]
rust = ["crates"]
[log]
enabled = true
level = "debug"
[llm.embedding]
model = "jinaai/jina-embeddings-v2-base-code"
[llm.embedding.remote]
endpoint = "http://localhost:7997"
backend = "infinity"
TOML
fi

# Confirm the resolved DB is inside the isolated clone before building anything.
db="$(rag-rat --config rag-rat.toml dump-config 2>/dev/null | grep -oE '/[^" ]*index\.sqlite' | head -1 || true)"
case "$db" in
  "$source_root"/*) echo "refusing: db '$db' resolves into the source checkout" >&2; exit 1 ;;
esac

rag-rat index --config rag-rat.toml
rag-rat hooks install

# Drive git actions to fire the hooks (the light/incremental maintenance path).
git checkout -q -b repro-branch
git commit -q --allow-empty -m "repro: fire post-commit hook"
git checkout -q -
git merge -q --no-edit repro-branch || true
# The hook backgrounds `rag-rat maintenance &`; give it a moment to write its log.
sleep 5

echo "=== doctor ==="
rag-rat --config rag-rat.toml doctor || true
echo "=== logs ($clone/.rag-rat/logs) ==="
ls -la "$clone/.rag-rat/logs/" 2>/dev/null || true
echo "=== maintenance / reconcile / embed narrative ==="
grep -h -E "maintenance pass|phase =|reconcile complete|light/incremental|no_budget|no_ephemeral|embed request|remaining backlog" \
  "$clone/.rag-rat/logs/"*.log 2>/dev/null | tail -50 || true
echo
echo "work dir kept for inspection: $work"
