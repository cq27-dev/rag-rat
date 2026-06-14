#!/usr/bin/env bash
# Present C edge resolution on the Linux kernel via the scip-clang oracle (#71), repeatably.
#
# Companion to tools/bench-kernel.sh: same pinned kernel + C-target conventions, but it additionally
# BUILDS the kernel to produce a compile_commands.json (scip-clang's input), runs
# `oracle run --tool scip-clang`, and emits the heuristic-vs-compiler resolution delta as a BMF for
# Bencher (benchmark `linux-kernel-<tag>/c-oracle`).
#
# COVERAGE CAVEAT (load-bearing): scip-clang only resolves translation units present in
# compile_commands.json, which is exactly the set the chosen KERNEL_CONFIG compiles — `defconfig`
# is a few thousand TUs, `allmodconfig` is most of the tree. Every resolution metric below is over
# that COMPILED SUBSET, not the whole-kernel 62k/67.4% headline that bench-kernel.sh reports.
#
# Env:
#   KERNEL_TAG / KERNEL_SHA   pinned kernel (default v7.0 / 028ef9c9…, matches bench-kernel.sh)
#   KERNEL_CONFIG             make config target for the compdb (default: defconfig)
#   RAG_RAT_BIN               release binary (default: target/release/rag-rat)
#   KERNEL_WORK               working dir (default: a fresh mktemp dir)
#   BMF_OUT                   Bencher Metric Format output path (default: kernel_c_oracle_bmf.json)
set -euo pipefail

KERNEL_TAG="${KERNEL_TAG:-v7.0}"
KERNEL_SHA="${KERNEL_SHA:-028ef9c96e96197026887c0f092424679298aae8}"
KERNEL_CONFIG="${KERNEL_CONFIG:-defconfig}"
RAG_RAT_BIN="${RAG_RAT_BIN:-target/release/rag-rat}"
WORK="${KERNEL_WORK:-$(mktemp -d)}"
BMF_OUT="${BMF_OUT:-kernel_c_oracle_bmf.json}"
# Resolve to an ABSOLUTE path before any cd — `command -v` can return a relative path (e.g.
# `target/release/rag-rat`), which breaks once the script cd's into the kernel tree, so always
# canonicalize the result.
RAG_RAT_BIN="$(command -v "$RAG_RAT_BIN" || echo "$RAG_RAT_BIN")"
RAG_RAT_BIN="$(readlink -f "$RAG_RAT_BIN")"
BMF_OUT="$(readlink -f "$BMF_OUT" 2>/dev/null || echo "$PWD/$BMF_OUT")"
mkdir -p "$WORK"
DB="$WORK/kernel-index.sqlite"
KDIR="$WORK/linux"

[ -x "$RAG_RAT_BIN" ] || { echo "kernel-c-oracle: rag-rat not found at '$RAG_RAT_BIN'" >&2; exit 1; }
command -v scip-clang >/dev/null 2>&1 || {
  echo "kernel-c-oracle: scip-clang not on PATH (install from github.com/sourcegraph/scip-clang)" >&2
  exit 1
}

echo "kernel-c-oracle: fetching Linux ${KERNEL_TAG} (${KERNEL_SHA}, shallow)" >&2
git init -q "$KDIR"
git -C "$KDIR" remote add origin https://github.com/torvalds/linux.git
git -C "$KDIR" -c protocol.version=2 fetch -q --depth 1 origin "$KERNEL_SHA"
git -C "$KDIR" checkout -q "$KERNEL_SHA"

# Build the kernel so its compile_commands.json target can read the per-object .cmd files. Quiet
# build; failures in a few TUs don't abort (|| true) — a partial compdb still demonstrates the join.
echo "kernel-c-oracle: building ${KERNEL_CONFIG} + compile_commands.json" >&2
make -C "$KDIR" -s "$KERNEL_CONFIG"
make -C "$KDIR" -s -j"$(nproc)" 2>/dev/null || true
make -C "$KDIR" -s compile_commands.json
TUS="$(python3 -c "import json,sys; print(len(json.load(open('$KDIR/compile_commands.json'))))")"
echo "kernel-c-oracle: compile_commands.json covers $TUS translation units" >&2

cat > "$KDIR/rag-rat.toml" <<EOF
[index]
root = "$KDIR"
database = "$DB"

[target_bindings]
c = ["."]
EOF

echo "kernel-c-oracle: rag-rat index --full" >&2
( cd "$KDIR" && "$RAG_RAT_BIN" index --full >/dev/null )

# The scip-clang oracle pass over the compiled subset (stdout = clean JSON report).
echo "kernel-c-oracle: oracle run --tool scip-clang" >&2
( cd "$KDIR" && "$RAG_RAT_BIN" oracle run --tool scip-clang --json ) > "$WORK/oracle-report.json"

python3 - "$DB" "$WORK/oracle-report.json" "$TUS" "$BMF_OUT" "$KERNEL_TAG" <<'PY'
import json, sqlite3, sys
db, report_path, tus, bmf_out, tag = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4], sys.argv[5]
report = json.load(open(report_path)).get("report", {})
conn = sqlite3.connect(db)
q = lambda s: conn.execute(s).fetchone()[0]

# Whole-index heuristic baseline (rag-rat indexes the full tree; the oracle only covers the
# compiled subset, so these two populations differ — reported side by side, honestly).
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

print(f"\n=== C edge resolution on Linux {tag} (compiled subset: {tus} TUs) ===")
print(f"whole-index heuristic calls_name resolved: {heur_resolved}/{total_calls} ({heur_rate:.1f}%)")
print(f"oracle (compiled subset): confirmed={confirmed} contradicted={contradicted} "
      f"upgraded={upgraded} resolved_external={resolved_external}")
print(f"compiler-confirmed precision of heuristic-resolved edges: {precision:.1f}%  "
      f"(confirm/(confirm+contradict))")
print(f"call recall (oracle-seen calls a calls_name edge covered): {recall:.1f}%")

# Precision split by edge_kind (#61): the blended `precision` above mixes function calls
# (`calls_name`) with type references (`references_type`) etc. They have very different
# characters — type refs suffer the forward-declaration-vs-definition problem — so the blended
# number under-sells call resolution. Report per-kind precision and surface calls vs types to the
# BMF.
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

# Contradiction attribution (#61): which heuristic RESOLUTION PATH produces the disagreements, and
# is the disagreement a same-NAME collision (heuristic bound the right name, wrong definition —
# improvable with linkage / include-scope disambiguation) or a name MISMATCH (call site is a macro
# expansion / function pointer the compiler resolved elsewhere — not syntactically fixable). This is
# the data that decides whether tree-sitter resolution can be pushed further or the oracle is the
# only lever. `edges` is a view exposing `resolution` (the path) and `confidence` (the tier).
print("\n--- contradiction attribution by heuristic resolution path ---")
print(f"{'confidence':<10} {'resolution':<22} {'confirm':>9} {'contra':>9} {'precision':>9}")
for conf, res, c, x in conn.execute(
    "SELECT e.confidence, e.resolution, "
    "SUM(CASE WHEN o.kind='confirm' THEN 1 ELSE 0 END), "
    "SUM(CASE WHEN o.kind='contradict' THEN 1 ELSE 0 END) "
    "FROM edge_oracle o JOIN edges e ON e.id = o.edge_id "
    "WHERE o.kind IN ('confirm','contradict') "
    "GROUP BY e.confidence, e.resolution ORDER BY 4 DESC"
).fetchall():
    prec = 100.0 * c / (c + x) if (c + x) else 0.0
    print(f"{conf or '':<10} {res or '':<22} {c:>9} {x:>9} {prec:>8.1f}%")

same, tot = conn.execute(
    "SELECT SUM(CASE WHEN o.scip_symbol LIKE '%'||e.to_name||'%' THEN 1 ELSE 0 END), COUNT(*) "
    "FROM edge_oracle o JOIN edges e ON e.id = o.edge_id WHERE o.kind='contradict'"
).fetchone()
print(f"\ncontradictions where the call name appears in the compiler's symbol: {same}/{tot} "
      f"({100.0 * same / tot if tot else 0:.1f}%)")
print("  high → same-name collision (improvable: linkage/include scoping); "
      "low → macro/fn-pointer (oracle-only)")

# logical_variant hypothesis (#61): does this path contradict because the heuristic picks a C
# function's prototype DECLARATION (smaller byte span, parsed first) while the compiler resolves to
# the DEFINITION (larger span, has a body) of the SAME path::name? If so, a definition-preference
# tiebreak fixes it. `hs` = heuristic-chosen symbol, `os` = oracle-resolved symbol.
lv = conn.execute(
    "SELECT "
    " SUM(CASE WHEN hs.qualified_name = os.qualified_name THEN 1 ELSE 0 END), "          # same file+name
    " SUM(CASE WHEN (hs.end_byte-hs.start_byte) < (os.end_byte-os.start_byte) THEN 1 ELSE 0 END), "  # heuristic smaller (decl)
    " COUNT(*) "
    "FROM edge_oracle o JOIN edges e ON e.id=o.edge_id "
    "LEFT JOIN symbols hs ON hs.id=e.to_symbol_id "
    "LEFT JOIN symbols os ON os.id=o.resolved_symbol_id "
    "WHERE o.kind='contradict' AND e.resolution='logical_variant' "
    "AND hs.id IS NOT NULL AND os.id IS NOT NULL"
).fetchone()
sq, hsm, lvt = lv
print(f"\n--- logical_variant contradiction shape (n={lvt}) ---")
print(f"  heuristic & oracle share qualified_name (same file+name, i.e. decl-vs-def): {sq}/{lvt} "
      f"({100.0*sq/lvt if lvt else 0:.1f}%)")
print(f"  heuristic span < oracle span (heuristic picked the smaller = declaration): {hsm}/{lvt} "
      f"({100.0*hsm/lvt if lvt else 0:.1f}%)")
print("  both high → definition-preference tiebreak among same-path::name candidates is the fix")
print("  sample rows (call | heuristic name@span | oracle name@span):")
for tn, hn, hsp, on, osp in conn.execute(
    "SELECT e.to_name, hs.name, hs.end_byte-hs.start_byte, os.name, os.end_byte-os.start_byte "
    "FROM edge_oracle o JOIN edges e ON e.id=o.edge_id "
    "JOIN symbols hs ON hs.id=e.to_symbol_id JOIN symbols os ON os.id=o.resolved_symbol_id "
    "WHERE o.kind='contradict' AND e.resolution='logical_variant' LIMIT 12"
).fetchall():
    print(f"    {tn:<24} {hn or '?'}@{hsp}  ->  {on or '?'}@{osp}")

bmf = {f"linux-kernel-{tag}/c-oracle": {
    "compiled_tus": {"value": tus},
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

# Free the multi-GB kernel checkout + index DB (they accumulate per run on the self-hosted box);
# keep the small oracle-report.json in WORK for artifact upload and the BMF at $BMF_OUT.
rm -rf "$KDIR" "$DB" "$DB"-wal "$DB"-shm
echo "kernel-c-oracle: done (report: $WORK/oracle-report.json, BMF: $BMF_OUT)" >&2
