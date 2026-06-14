#!/usr/bin/env bash
# Present Rust edge resolution via the rust-analyzer SCIP oracle (#61), repeatably.
#
# The Rust mirror of tools/kernel-c-oracle.sh. Simpler than the C path: rust-analyzer analyzes the
# WHOLE Cargo workspace (no per-TU compilation database, no compiled subset), so the resolution
# metrics cover the entire indexed crate — not a configured subset. Steps: fetch the pinned corpus,
# `rag-rat index --full`, `oracle run --tool rust-analyzer`, emit the heuristic-vs-compiler delta as
# a BMF for Bencher (benchmark `rust-cargo-<tag>/rust-oracle`).
#
# Corpus is rust-lang/cargo pinned by SHA — the SAME snapshot the iai/criterion benches use
# (benches/shared/mod.rs), so all bench corpora stay consistent.
#
# Env:
#   CARGO_TAG / CARGO_SHA   pinned corpus (default 0.97.1 / fc1044d6…, matches benches/shared/mod.rs)
#   RAG_RAT_BIN             release binary (default: target/release/rag-rat)
#   RUST_WORK               working dir (default: a fresh mktemp dir)
#   BMF_OUT                 Bencher Metric Format output path (default: rust_scip_oracle_bmf.json)
set -euo pipefail

CARGO_TAG="${CARGO_TAG:-0.97.1}"
CARGO_SHA="${CARGO_SHA:-fc1044d6129608b3a3188566a919dc6126f7cb15}"
RAG_RAT_BIN="${RAG_RAT_BIN:-target/release/rag-rat}"
WORK="${RUST_WORK:-$(mktemp -d)}"
BMF_OUT="${BMF_OUT:-rust_scip_oracle_bmf.json}"
# Resolve to an ABSOLUTE path before any cd — `command -v` can return a relative path, which breaks
# once the script cd's into the corpus tree, so always canonicalize the result.
RAG_RAT_BIN="$(command -v "$RAG_RAT_BIN" || echo "$RAG_RAT_BIN")"
RAG_RAT_BIN="$(readlink -f "$RAG_RAT_BIN")"
BMF_OUT="$(readlink -f "$BMF_OUT" 2>/dev/null || echo "$PWD/$BMF_OUT")"
mkdir -p "$WORK"
DB="$WORK/rust-index.sqlite"
RDIR="$WORK/cargo"

[ -x "$RAG_RAT_BIN" ] || { echo "rust-scip-oracle: rag-rat not found at '$RAG_RAT_BIN'" >&2; exit 1; }
command -v rust-analyzer >/dev/null 2>&1 || {
  echo "rust-scip-oracle: rust-analyzer not on PATH (install from rust-lang/rust-analyzer releases)" >&2
  exit 1
}

echo "rust-scip-oracle: fetching rust-lang/cargo ${CARGO_TAG} (${CARGO_SHA}, shallow)" >&2
git init -q "$RDIR"
git -C "$RDIR" remote add origin https://github.com/rust-lang/cargo.git
git -C "$RDIR" -c protocol.version=2 fetch -q --depth 1 origin "$CARGO_SHA"
git -C "$RDIR" checkout -q "$CARGO_SHA"

# rust-analyzer loads the workspace via `cargo metadata`; pre-fetch the dependency graph so the SCIP
# pass doesn't race a cold registry. Non-fatal: rust-analyzer still resolves in-workspace symbols
# even if a dep can't be fetched.
echo "rust-scip-oracle: cargo fetch (warm the dep graph for rust-analyzer)" >&2
( cd "$RDIR" && cargo fetch -q 2>/dev/null ) || true

cat > "$RDIR/rag-rat.toml" <<EOF
[index]
root = "$RDIR"
database = "$DB"

[target_bindings]
rust = ["."]
EOF

echo "rust-scip-oracle: rag-rat index --full" >&2
( cd "$RDIR" && "$RAG_RAT_BIN" index --full >/dev/null )

# The rust-analyzer oracle pass over the whole workspace (stdout = clean JSON report).
echo "rust-scip-oracle: oracle run --tool rust-analyzer" >&2
( cd "$RDIR" && "$RAG_RAT_BIN" oracle run --tool rust-analyzer --json ) > "$WORK/oracle-report.json"

python3 - "$DB" "$WORK/oracle-report.json" "$BMF_OUT" "$CARGO_TAG" <<'PY'
import json, sqlite3, sys
db, report_path, bmf_out, tag = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
report = json.load(open(report_path)).get("report", {})
conn = sqlite3.connect(db)
q = lambda s: conn.execute(s).fetchone()[0]

rust_files = q("SELECT COUNT(*) FROM files WHERE language='rust'")
total_calls = q("SELECT COUNT(*) FROM edges WHERE edge_kind='calls_name' AND callee_start_byte IS NOT NULL")
heur_resolved = q("SELECT COUNT(*) FROM edges WHERE edge_kind='calls_name' AND callee_start_byte IS NOT NULL AND to_symbol_id IS NOT NULL")
heur_rate = 100.0 * heur_resolved / total_calls if total_calls else 0.0

confirmed = report.get("confirmed", 0)
contradicted = report.get("contradicted", 0)
upgraded = report.get("upgraded", 0)
resolved_external = report.get("resolved_external", 0)
covered = report.get("covered_calls", 0)
oracle_only = report.get("oracle_only_calls", 0)
judged = confirmed + contradicted
precision = 100.0 * confirmed / judged if judged else 0.0   # compiler-confirmed fraction of resolved
recall = 100.0 * covered / (covered + oracle_only) if (covered + oracle_only) else 0.0

print(f"\n=== Rust edge resolution on rust-lang/cargo {tag} ({rust_files} .rs files) ===")
print(f"heuristic calls_name resolved: {heur_resolved}/{total_calls} ({heur_rate:.1f}%)")
print(f"oracle: confirmed={confirmed} contradicted={contradicted} "
      f"upgraded={upgraded} resolved_external={resolved_external}")
print(f"compiler-confirmed precision of heuristic-resolved edges: {precision:.1f}%  "
      f"(confirm/(confirm+contradict))")
print(f"call recall (oracle-seen calls a calls_name edge covered): {recall:.1f}%")

# Precision split by edge_kind (#61): separate function calls (`calls_name`) from type references
# (`references_type`) so the blended number doesn't hide per-kind differences (the C oracle showed
# 85% calls vs 18% types). Surface calls vs types to the BMF.
print("\n--- compiler precision by edge_kind ---")
print(f"{'edge_kind':<18} {'confirm':>10} {'contra':>10} {'precision':>10}")
kind_prec = {}
for ek, c, x in conn.execute(
    "SELECT e.edge_kind, "
    "SUM(CASE WHEN o.kind='confirm' THEN 1 ELSE 0 END), "
    "SUM(CASE WHEN o.kind='contradict' THEN 1 ELSE 0 END) "
    "FROM edge_oracle o JOIN edges e ON e.id = o.edge_id "
    "WHERE o.kind IN ('confirm','contradict') "
    "GROUP BY e.edge_kind ORDER BY 2 DESC"
).fetchall():
    prec = 100.0 * c / (c + x) if (c + x) else 0.0
    kind_prec[ek] = prec
    print(f"{ek or '':<18} {c:>10} {x:>10} {prec:>9.1f}%")
calls_precision = kind_prec.get("calls_name", 0.0)
types_precision = kind_prec.get("references_type", 0.0)

bmf = {f"rust-cargo-{tag}/rust-oracle": {
    "rust_files": {"value": rust_files},
    "heuristic_resolved_rate": {"value": heur_rate},
    "compiler_precision": {"value": precision},
    "compiler_precision_calls": {"value": calls_precision},
    "compiler_precision_types": {"value": types_precision},
    "call_recall": {"value": recall},
    "confirmed": {"value": confirmed},
    "contradicted": {"value": contradicted},
    "upgraded": {"value": upgraded},
    "resolved_external": {"value": resolved_external},
}}
json.dump(bmf, open(bmf_out, "w"), indent=2)
print(f"wrote BMF -> {bmf_out}")
PY

# Free the corpus checkout + index DB (they accumulate per run on the self-hosted box); keep the
# small oracle-report.json in WORK for artifact upload and the BMF at $BMF_OUT.
rm -rf "$RDIR" "$DB" "$DB"-wal "$DB"-shm
echo "rust-scip-oracle: done (report: $WORK/oracle-report.json, BMF: $BMF_OUT)" >&2
