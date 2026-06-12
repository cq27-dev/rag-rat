#!/usr/bin/env bash
# Headline "indexes the Linux kernel in X seconds" benchmark.
#
# Clones a pinned Linux kernel tag, indexes its C/H sources once with the *release* rag-rat binary
# (dogfooding the shipped `index` command), and writes a Bencher Metric Format (BMF) JSON file with
# three measures: latency (ns), throughput (files/s), and peak memory (bytes). The release workflow
# (.github/workflows/bench_release.yml) feeds that file to `bencher run --adapter json`.
#
# Single-shot on purpose: a full kernel index is ~tens of minutes, so criterion's 10-sample loop
# isn't viable — this is one cold rebuild, the number a user actually sees. Runs only on release
# (or manual dispatch), so the long runtime is acceptable.
#
# Env knobs:
#   KERNEL_TAG               kernel tag to index            (default: v7.0)
#   RAG_RAT_BIN              path to the release binary     (default: target/release/rag-rat)
#   RAG_RAT_KERNEL_SUBDIRS   space-separated subtrees to    (default: ".", the whole tree)
#                            index — set e.g. "kernel mm fs net lib" to bound scope/memory
#   RAG_RAT_INDEX_WAVE       full-rebuild wave size (files prepared/inserted per wave before the
#                            wave's prepared form is dropped) — the memory/speed knob. Read by the
#                            binary; passed through this script's environment. (binary default: 2000)
#   BMF_OUT                  output BMF JSON path           (default: kernel_bmf.json)
#   TAXONOMY_CSV             unresolved-edge-by-kind CSV    (default: kernel_unresolved_by_kind.csv)
#   KERNEL_WORK              working dir                    (default: a fresh mktemp dir)
set -euo pipefail

KERNEL_TAG="${KERNEL_TAG:-v7.0}"
# Pinned by commit SHA so the corpus is immutable even if the tag is ever moved (v7.0 = this commit).
KERNEL_SHA="${KERNEL_SHA:-028ef9c96e96197026887c0f092424679298aae8}"
RSS_CSV="${RSS_CSV:-kernel_rss_samples.csv}"
TAXONOMY_CSV="${TAXONOMY_CSV:-kernel_unresolved_by_kind.csv}"
RAG_RAT_BIN="${RAG_RAT_BIN:-target/release/rag-rat}"
RAG_RAT_KERNEL_SUBDIRS="${RAG_RAT_KERNEL_SUBDIRS:-.}"
BMF_OUT="${BMF_OUT:-kernel_bmf.json}"
WORK="${KERNEL_WORK:-$(mktemp -d)}"
mkdir -p "$WORK"
# The work dir holds the kernel checkout (~2 GB) + the SQLite index DB (~10 GB). Always remove it on
# exit so reruns don't accumulate — critical when WORK lands on a size-limited tmpfs (e.g. a box
# where /tmp is RAM-backed), which otherwise fills up and the index hits a SQLite disk-I/O error.
# The BMF result is written to BMF_OUT (outside WORK), so cleanup doesn't lose it.
trap 'rm -rf "$WORK"' EXIT

command -v "$RAG_RAT_BIN" >/dev/null 2>&1 || [ -x "$RAG_RAT_BIN" ] || {
  echo "bench-kernel: rag-rat binary not found at '$RAG_RAT_BIN' (build with: cargo build --release --no-default-features --bin rag-rat)" >&2
  exit 1
}

# Shallow-fetch the pinned commit directly (not the tag), so the corpus can't drift.
echo "bench-kernel: fetching Linux kernel ${KERNEL_TAG} (${KERNEL_SHA}, shallow) into ${WORK}/linux" >&2
git init -q "$WORK/linux"
git -C "$WORK/linux" remote add origin https://github.com/torvalds/linux.git
git -C "$WORK/linux" -c protocol.version=2 fetch -q --depth 1 origin "$KERNEL_SHA"
git -C "$WORK/linux" checkout -q "$KERNEL_SHA"

# Render a C-language config over the requested subtree(s). `c = [...]` maps to **/*.c + **/*.h.
subdirs_toml="$(printf '"%s", ' $RAG_RAT_KERNEL_SUBDIRS)"
cat > "$WORK/rag-rat.toml" <<EOF
[index]
root = "$WORK/linux"
database = "$WORK/kernel-index.sqlite"

[target_bindings]
c = [${subdirs_toml%, }]
EOF

# Single cold full index, measured. We capture TWO memory numbers:
#   - maxrss: getrusage(RUSAGE_CHILDREN).ru_maxrss — the kernel-true peak (catches sub-sample spikes),
#     used for the cross-tool comparison (the competitor's number is also maxrss).
#   - a /proc/<pid>/VmRSS time-series sampled at 1 s — the before/after memory CURVE for the README,
#     and a sampled peak. rag-rat is a SINGLE process (rayon = threads, not forked workers), so the
#     main pid's VmRSS is the complete resident set — no process-group summing needed.
# Release binary built --no-default-features (hash embedder; no model download, no network), so the
# number is the indexing machinery, reproducible across runs.
echo "bench-kernel: indexing (this takes a while)…" >&2
python3 - "$RAG_RAT_BIN" "$WORK/rag-rat.toml" "$RSS_CSV" > "$WORK/measure.txt" <<'PY'
import resource, subprocess, sys, time

rag_rat, cfg, samples_path = sys.argv[1], sys.argv[2], sys.argv[3]

def vmrss_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as fh:
            for line in fh:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except (FileNotFoundError, ProcessLookupError):
        pass
    return None

proc = subprocess.Popen([rag_rat, "--config", cfg, "index", "--full"], stdout=subprocess.DEVNULL)
start = time.monotonic()
sampled_peak_kb = 0
samples = []
while proc.poll() is None:
    rss = vmrss_kb(proc.pid)
    if rss is not None:
        sampled_peak_kb = max(sampled_peak_kb, rss)
        samples.append((round(time.monotonic() - start, 1), rss))
    time.sleep(1.0)
rc = proc.wait()
seconds = time.monotonic() - start
maxrss_kb = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
with open(samples_path, "w") as fh:
    fh.write("t_seconds,vmrss_kb\n")
    for t, r in samples:
        fh.write(f"{t},{r}\n")
print(f"{seconds} {maxrss_kb} {sampled_peak_kb} {rc}")
PY
read -r seconds maxrss_kb sampled_peak_kb rc < "$WORK/measure.txt"
if [ "${rc:-1}" -ne 0 ]; then
  echo "bench-kernel: rag-rat index exited non-zero ($rc)" >&2
  exit 1
fi

# Read the extracted-fact counts from the DB and emit the BMF. Capturing symbols/edges/chunks (not
# just files/s) makes the run comparable per extracted fact, not just per file — different tools
# compute different products, so throughput alone is mush.
python3 - "$WORK/kernel-index.sqlite" "$seconds" "$maxrss_kb" "$sampled_peak_kb" "$BMF_OUT" "$KERNEL_TAG" "$TAXONOMY_CSV" <<'PY'
import json, os, sqlite3, sys
db, seconds = sys.argv[1], float(sys.argv[2])
maxrss_kb, sampled_peak_kb = int(sys.argv[3]), int(sys.argv[4])
out, tag, taxonomy_csv = sys.argv[5], sys.argv[6], sys.argv[7]
conn = sqlite3.connect(db)
# Checkpoint the WAL into the main file first so db_size measures the real durable footprint.
conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
db_size = os.path.getsize(db)
count = lambda table: conn.execute(f"select count(*) from {table}").fetchone()[0]
files, symbols, edges, chunks = count("files"), count("symbols"), count("edges"), count("chunks")
resolved = conn.execute("select count(*) from edges where to_symbol_id is not null").fetchone()[0]
resolved_pct = 100.0 * resolved / edges if edges else 0.0

# Unresolved-edge taxonomy by edge_kind: the denominator semantics for the resolved rate. The
# unresolved bucket is NOT all error — calls_name to extern/macro/function-pointer targets and
# references_type to out-of-corpus types legitimately don't resolve in a tree-sitter pass. Bucketing
# by edge_kind is the rough, honest breakdown; it is also the natural "before" baseline for the SCIP
# oracle pass (issue #61), which upgrades exactly these buckets.
unresolved_by_kind = conn.execute(
    "select edge_kind, count(*) from edges where to_symbol_id is null "
    "group by edge_kind order by count(*) desc"
).fetchall()
unresolved_total = edges - resolved
with open(taxonomy_csv, "w") as fh:
    fh.write("edge_kind,unresolved_count,pct_of_unresolved\n")
    for kind, n in unresolved_by_kind:
        pct = 100.0 * n / unresolved_total if unresolved_total else 0.0
        fh.write(f"{kind},{n},{pct:.1f}\n")
wave = os.environ.get("RAG_RAT_INDEX_WAVE", "2000 (default)")

bmf = {
    # Benchmark name carries the tag so re-pinning to a newer kernel starts a distinct series.
    f"linux-kernel-{tag}/full-index": {
        "latency": {"value": seconds * 1e9},        # nanoseconds — Bencher's built-in Latency measure
        "throughput": {"value": files / seconds},   # files per second
        "memory": {"value": maxrss_kb * 1024},      # kernel-true peak RSS (maxrss), bytes — cross-tool
        "memory_sampled": {"value": sampled_peak_kb * 1024},  # 1 s-sampled peak VmRSS, bytes
        "symbols": {"value": symbols},
        "edges": {"value": edges},
        "resolved_edges": {"value": resolved},
        "chunks": {"value": chunks},
        # On-disk index size, bytes (post-checkpoint) — the #79 interning headline at kernel
        # scale; tracked so size regressions surface alongside latency/memory.
        "db_size": {"value": db_size},
    }
}
with open(out, "w") as f:
    json.dump(bmf, f, indent=2)

taxonomy = " ".join(f"{kind}={n}" for kind, n in unresolved_by_kind)
print(
    f"bench-kernel: indexed {files} files of Linux {tag} in {seconds:.1f}s "
    f"({files/seconds:.1f} files/s, wave={wave}) — peak {maxrss_kb/1024:.0f} MiB maxrss "
    f"(sampled {sampled_peak_kb/1024:.0f} MiB) — db {db_size/1024/1024:.0f} MiB — "
    f"{symbols} symbols, {edges} edges ({resolved} resolved, {resolved_pct:.1f}%), {chunks} chunks → {out}",
    file=sys.stderr,
)
print(
    f"bench-kernel: {unresolved_total} unresolved edges by kind → {taxonomy} (full breakdown in {taxonomy_csv})",
    file=sys.stderr,
)
PY
