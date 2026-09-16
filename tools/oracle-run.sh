#!/usr/bin/env bash
# tools/oracle-run.sh — run the SCIP oracle for ONE declared corpus end to end and emit its C2
# resolution report JSON (#164, C3). The single, tier-agnostic runner that replaces the per-language
# rust-scip-oracle.sh / kernel-c-oracle.sh demos: it reads the corpus profile from
# tools/oracle-corpora.toml, clones the repo at its pinned rev, runs the corpus's prepare steps,
# indexes it with rag-rat, then runs `rag-rat oracle report`.
#
# `oracle report` is what runs the oracle (produces the .scip with the corpus's tool, joins it,
# assembles the typed report) AND applies the per-corpus health gate — so a corpus whose run falls
# outside its thresholds makes this script exit non-zero even though every step "succeeded". The
# report JSON is still written in that case, so a Δ glue script (tools/oracle-report-md.py, C5) can
# consume it. rag-rat emits JSON only; markdown/Bencher formatting is a glue concern.
#
# Env:
#   CORPUS          (required) corpus_id from the corpora file
#   CORPORA         corpora file (default: <repo>/tools/oracle-corpora.toml)
#   RAG_RAT_BIN     rag-rat binary (default: target/release/rag-rat)
#   ORACLE_WORK     working dir (default: a fresh mktemp dir)
#   REPORT_OUT      report JSON output path (default: $ORACLE_WORK/<corpus>-report.json)
#   RAG_RAT_COMMIT  provenance stamp baked into the report (default: this repo's HEAD)
#   KEEP_CHECKOUT   set to 1 to keep the corpus checkout + index DB (default: removed)
set -euo pipefail

CORPUS="${CORPUS:?set CORPUS to a corpus_id from the corpora file}"

# Resolve everything to ABSOLUTE paths while the CWD is still this repo, before any cd into the
# corpus tree (a relative path or `command -v` result breaks once we cd away).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$SCRIPT_DIR/oracle-corpus.py"
CORPORA="${CORPORA:-$SCRIPT_DIR/oracle-corpora.toml}"
CORPORA="$(readlink -f "$CORPORA")"
RAG_RAT_BIN="${RAG_RAT_BIN:-target/release/rag-rat}"
RAG_RAT_BIN="$(command -v "$RAG_RAT_BIN" || echo "$RAG_RAT_BIN")"
RAG_RAT_BIN="$(readlink -f "$RAG_RAT_BIN")"
RAG_RAT_COMMIT="${RAG_RAT_COMMIT:-$(git rev-parse HEAD 2>/dev/null || echo unknown)}"
WORK="${ORACLE_WORK:-$(mktemp -d)}"
mkdir -p "$WORK"
REPORT_OUT="${REPORT_OUT:-$WORK/$CORPUS-report.json}"
REPORT_OUT="$(readlink -f "$REPORT_OUT" 2>/dev/null || echo "$PWD/$REPORT_OUT")"

[ -x "$RAG_RAT_BIN" ] || { echo "oracle-run: rag-rat not found at '$RAG_RAT_BIN'" >&2; exit 1; }
[ -f "$CORPORA" ] || { echo "oracle-run: corpora file not found at '$CORPORA'" >&2; exit 1; }

field() { python3 "$HELPER" --corpora "$CORPORA" --corpus "$CORPUS" --field "$1"; }

REPO="$(field repo)"
REV="$(field rev)"
TOOL="$(field tool)"
TIMEOUT_MINUTES="$(field timeout_minutes)"
CHECKOUT="$WORK/checkout"
DB="$WORK/$CORPUS-index.sqlite"

# Enforce the corpus's wall-clock budget over the WHOLE run — clone + prepare (cargo fetch / a kernel
# build) + index + report — not just the report step (Codex on #177). Re-exec the script once under
# `timeout` after resolving the budget; the guard env var stops a second wrap, and the resolved paths
# are exported so the re-exec reuses the same work dir / binary rather than re-deriving them. `-k`
# escalates to SIGKILL if a hung tool ignores SIGTERM. A timeout exits 124 (a failure like any gate
# violation); the EXIT trap below still removes the checkout.
if [ -z "${ORACLE_RUN_TIMED:-}" ]; then
    export ORACLE_RUN_TIMED=1 ORACLE_WORK="$WORK" CORPORA RAG_RAT_BIN RAG_RAT_COMMIT REPORT_OUT
    export CORPUS KEEP_CHECKOUT="${KEEP_CHECKOUT:-0}"
    exec timeout -k 60s "${TIMEOUT_MINUTES}m" "$0"
fi

# Inside the timed re-exec. Remove the checkout + index DB on ANY exit (gate failure, timeout, error)
# while leaving the report JSON; idempotent, so the trap firing on both a signal and the final exit
# is harmless.
cleanup_checkout() {
    if [ "${KEEP_CHECKOUT:-0}" != "1" ]; then
        rm -rf "$CHECKOUT" "$DB" "$DB"-wal "$DB"-shm
    fi
    rm -rf "${PYTHON_SHIM:-}"
}
trap cleanup_checkout EXIT

# A python corpus's prepare step spells `python`, which Debian and Ubuntu leave absent unless
# `python-is-python3` is installed — so it works in CI, whose setup action provides a bare `python`,
# and fails on a stock dev box with `python: command not found`.
#
# The environment is adjusted rather than the command, and that is the whole point: the prepare
# string is hashed into `corpus_profile_hash`, which is what lets a run be declared comparable with
# the recorded ones. Rewriting it to `python3` would silently make every future run incomparable
# with every prior one, to fix a portability nit.
if ! command -v python >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
    PYTHON_SHIM="$(mktemp -d)"
    ln -s "$(command -v python3)" "$PYTHON_SHIM/python"
    export PATH="$PYTHON_SHIM:$PATH"
    echo "oracle-run: no bare 'python' on PATH; shimmed to $(command -v python3)" >&2
fi

echo "oracle-run: corpus=$CORPUS tool=$TOOL repo=$REPO rev=$REV (budget ${TIMEOUT_MINUTES}m)" >&2

# Shallow-clone the pinned rev. A tag or branch name resolves via `fetch <ref>`; a full SHA fetches
# directly. `--depth 1` keeps the corpus small — the oracle never needs history.
echo "oracle-run: cloning $REPO @ $REV (shallow)" >&2
git init -q "$CHECKOUT"
git -C "$CHECKOUT" remote add origin "$REPO"
git -C "$CHECKOUT" -c protocol.version=2 fetch -q --depth 1 origin "$REV"
git -C "$CHECKOUT" checkout -q FETCH_HEAD

# Corpus prepare steps (cargo fetch, cmake compdb, venv install, kernel build, …). Each line is one
# shell command run in the checkout root. A failing prepare step aborts the run (set -e through the
# subshell) — a broken environment must not be reported as a clean resolution number.
while IFS= read -r prepare_cmd; do
    [ -n "$prepare_cmd" ] || continue
    echo "oracle-run: prepare> $prepare_cmd" >&2
    ( cd "$CHECKOUT" && bash -c "$prepare_cmd" )
done < <(field prepare)

# Activate a virtualenv the prepare steps created, so the oracle's indexer subprocess resolves
# against the project's installed deps rather than the global interpreter (Codex on #177). scip-python
# (pyright) finds dependency monikers only when its `python` is the venv's — prepare runs in child
# shells whose activation doesn't survive, so the runner re-establishes it for the index/report steps.
if [ -d "$CHECKOUT/.venv/bin" ]; then
    echo "oracle-run: activating $CHECKOUT/.venv" >&2
    export VIRTUAL_ENV="$CHECKOUT/.venv"
    export PATH="$CHECKOUT/.venv/bin:$PATH"
fi

# Render the corpus's rag-rat.toml: index the checkout into $DB with the declared per-language
# bindings. The oracle report reads the SAME bindings from the corpora file for provenance; this
# file is what `rag-rat index` walks.
{
    echo "[index]"
    echo "root = \"$CHECKOUT\""
    echo "database = \"$DB\""
    echo
    echo "[target_bindings]"
    field bindings_toml
} > "$CHECKOUT/rag-rat.toml"

echo "oracle-run: rag-rat index --full" >&2
( cd "$CHECKOUT" && "$RAG_RAT_BIN" index --full >/dev/null )

# `oracle report` runs the oracle + assembles the typed report + applies the health gate. The whole
# run is already inside the corpus wall-clock budget (the re-exec `timeout` above). Keep its exit
# code so the caller (CI) sees the gate result; the report JSON is always written (it's emitted
# before the gate fails), and the EXIT trap removes the checkout.
echo "oracle-run: oracle report --corpus $CORPUS" >&2
set +e
( cd "$CHECKOUT" && RAG_RAT_COMMIT="$RAG_RAT_COMMIT" \
    "$RAG_RAT_BIN" --json oracle report --corpus "$CORPUS" --corpora "$CORPORA" ) > "$REPORT_OUT"
rc=$?
set -e

if [ "$rc" -eq 0 ]; then
    echo "oracle-run: done (healthy) — report: $REPORT_OUT" >&2
elif [ "$rc" -eq 124 ]; then
    echo "oracle-run: TIMED OUT after ${TIMEOUT_MINUTES}m — report: $REPORT_OUT" >&2
else
    echo "oracle-run: health gate FAILED (exit $rc) — report: $REPORT_OUT" >&2
fi
exit "$rc"
