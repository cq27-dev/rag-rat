# Measured benchmarks

Concrete numbers for the headline workload — indexing the whole Linux kernel — plus the memory
profile that workload exposes and how syntactic resolution compares to a real compiler. This is the
*results* companion to [`bencher.md`](./bencher.md), which documents the *harness* (Bencher, the CI
workflows, and `tools/bench-kernel.sh`). Numbers here are single cold runs, not statistically-gated
CI signals; treat them as "what one run looks like," not a regression gate.

These numbers are the **v0.5.0** baseline. v0.5.0 indexes the kernel ~2.7× faster than v0.4.x — and
that is a *correctness* change, not lost coverage: see [v0.5.0 rebaseline](#v050-rebaseline-why-the-kernel-got-27-faster).

## Test harness

The kernel index runs on a self-hosted big-memory box (the `bench-release` workflow's
`[self-hosted, bigmem]` runner), inside the pinned bench container so the toolchain and SCIP indexers
are reproducible. It runs here rather than on a hosted runner for a stable, uncontended wall-clock —
not because it no longer fits a hosted box (the peak is 4.15 GiB; see below).

| | |
|---|---|
| CPU | AMD Ryzen 5 3600 — 6 cores / 12 threads, up to ~4.2 GHz |
| RAM | 64 GB (62 GiB) DDR4 |
| Storage | 2× Samsung MZVLB512HBJQ NVMe SSD in Linux mdraid **RAID1**, ext4 — kernel checkout + index DB both on NVMe (`KERNEL_WORK` set off the box's RAM-backed `/tmp`) |
| Build + run env | `tools/bench.Containerfile` — `debian:trixie-slim`, rustc/cargo **1.96.0** (stable), release build (`cargo build --release --no-default-features`) |

The rebuild's per-wave prepare stage is parallel (rayon), but the bulk of the wall-clock — the edge
resolve + insert + index rebuild + FTS pipeline — is serial and storage-bound, so storage speed
dominates wall-clock more than core count does. Peak RSS is governed by the graph/chunk set and
`RAG_RAT_INDEX_WAVE`, not by core count.

## Headline: indexing the Linux kernel

Linux kernel **v7.0** (pinned at commit `028ef9c96e96197026887c0f092424679298aae8`, shallow-cloned),
full index (`index --full`), whole tree (`RAG_RAT_KERNEL_SUBDIRS=.`), hash embedder
(`--no-default-features`, no model download), release build, `RAG_RAT_INDEX_WAVE=2000`.

| Metric | Value |
|---|---|
| Files indexed (C/H) | 62,903 |
| **Wall-clock** | **269.1 s** (4.49 min) |
| Throughput | 233.7 files/s |
| **Peak RSS** | **4.15 GiB** (4,249 MiB maxrss; 1 Hz sampler agrees at 4,250 MiB) |
| On-disk index DB | 4.98 GiB (5,099 MiB, post-checkpoint) |
| Symbols | 1,057,527 |
| Edges (call graph) | 9,144,061 |
| Edges resolved | 5,371,873 (58.7%) |
| Chunks | 1,611,390 |

Unresolved-edge taxonomy (the 41.3% / 3,772,188 edges the syntactic graph leaves dangling, by kind):
`calls_name` 1,861,690 (49.4%), `references_type` 1,508,646 (40.0%), `imports` 401,852 (10.7%). The
`calls_name` bucket is extern / macro / function-pointer call targets the syntactic resolver can't
bind without a compilation database; `references_type` is dominated by references whose type
*definition* lives in an uncompiled or external header — both are exactly what the SCIP oracle
recovers (below).

### v0.5.0 rebaseline: why the kernel got ~2.7× faster

v0.4.x indexed the same 62,903 files in **~724 s**; v0.5.0 does it in **269 s**. Nothing is being
skipped at the file level — the file count is identical. The difference is the **#61 C/C++
definitions-only** extraction change: the parser now emits a symbol for a struct/union/enum/function
only when it carries its **body** (a definition), not for forward declarations (`struct X;`), bare
type uses (`struct X *p`), or function prototypes. The kernel's headers repeat those prototypes and
forward-decls across thousands of includes, so dropping them as "symbols" removes most of the work:

| Metric | v0.4.x (pre-#61) | v0.5.0 | Δ |
|---|---|---|---|
| Files indexed | 62,903 | 62,903 | — (no scope loss) |
| Wall-clock | 723.6 s | 269.1 s | **−63%** |
| Symbols | 3,536,897 | 1,057,527 | −70% |
| Chunks | 4,246,140 | 1,611,390 | −62% |
| Edges | 11,213,107 | 9,144,061 | −18% |
| Resolved | 67.4% | 58.7% | −8.7 pp |
| Peak RSS | 5.60 GiB | 4.15 GiB | −26% |
| On-disk DB | 8.01 GiB | 4.98 GiB | −38% |

The two numbers that look *worse* are both improvements:

- **Fewer symbols/chunks** is the point — a forward declaration is not a definition, and the dropped
  ~2.5M symbols were bodyless prototype/forward-decl/use occurrences, not real code. Real
  definitions are still indexed.
- **Lower resolved rate** (67.4% → 58.7%) reflects *higher* precision, not lost resolution. The old
  rate was inflated by edges binding to declaration pseudo-symbols — frequently the wrong target
  (`references_type` would land on a 14-byte forward-decl of `pt_regs` instead of the real
  1556-byte definition). The same change took measured C `references_type` precision from **18% →
  91%** (see below). Lower rate, far higher correctness.

So v0.5.0's 269 s is the correct baseline for "indexes the Linux kernel" — a correctness-driven
speedup, not a coverage regression.

## C edge resolution: heuristic vs compiler (SCIP oracle)

The headline resolves 58.7% of edges *syntactically* — by name, no compiler. The SCIP oracle (#61)
measures how good that syntactic resolution actually is: it replays a real compilation through
`scip-clang` (a clang-based SCIP indexer) and diffs its ground-truth bindings against the heuristic's.
Numbers below are one heavy-tier `scip-clang` oracle run (`oracle.yml`, `scip-clang 0.4.0`, kernel `defconfig`, containerized
bench image on the self-hosted bigmem box), so they cover the **compiled subset** — the translation
units `defconfig` actually compiles — not the whole 62,903-file tree the headline indexes. Resolution
*quality* and tree-wide *coverage* are different populations, reported side by side, not merged.

| Metric | Value | Meaning |
|---|---|---|
| Compiled TUs | 2,956 | the `defconfig` compilation database scip-clang consumes |
| **Compiler precision (blended)** | **91.5%** | confirmed / (confirmed + contradicted), over every judged edge kind |
| — `calls_name` | **91.8%** | function-call resolution (315,099 confirm / 28,068 contradict) |
| — `references_type` | **91.1%** | type references (215,997 / 21,226) — after the definitions-only fix |
| Call recall | 44.3% | covered / (covered + oracle-only) — of calls the compiler saw, the share the graph had a `calls_name` edge for |
| Confirmed | 531,096 | heuristic target matched the compiler's |
| Contradicted | 49,294 | heuristic bound a different target than the compiler |
| Upgraded | 334,259 | edges promoted to `Compiler`-tier confidence |
| Resolved-external | 28,951 | dangling refs the compiler bound to a cross-TU / external symbol the heuristic couldn't reach |

The headline: on the compiled subset, name-based resolution agrees with the compiler **~92% of the
time** on the edges it commits to — and that's after the oracle caught a real indexing bug. Getting
here is the instructive part, and it is the same change that rebaselined the kernel headline above:

- **The first measurement read 50.1% blended** — alarming, and it looked like "C resolution is a
  coin flip." It wasn't a measurement artifact (a `#93` logical-symbol comparison fix moved it <1 pt),
  so the number was real — but it was a *blend* hiding two very different populations.
- **Splitting by edge kind exposed the culprit:** `calls_name` was already **85%**, while
  `references_type` was **18%**. Type references, not call resolution, dragged the headline down.
- **Root cause (#61):** the C/C++ parser emitted a symbol for *every* `struct`/`union`/`enum`
  specifier — definitions, forward declarations (`struct X;`), *and* bare uses (`struct X *p`) — plus
  function prototypes. A `references_type` edge then bound to a tiny bodyless forward-decl/use
  occurrence instead of the real definition.
- **The fix:** index **definitions only** — a specifier must carry its body, and bare prototypes are
  dropped. `references_type` precision jumped **18% → 91.1%**, `calls_name` rose **85% → 91.8%** (the
  same change removed mis-resolutions to prototypes), and the blend went **50.1% → 91.5%**. This is
  also what cut the kernel-index symbol/chunk counts (and therefore wall-clock) above.

So the honest story is the opposite of the first read: C heuristic resolution is **~92% precise**,
and the SCIP oracle's value showed up twice — it *found* the forward-declaration bug, then quantified
the fix. Beyond precision, this run **upgraded 334k** unresolved/low-confidence edges to
compiler-grade confidence and recovered **29k cross-TU externals** the heuristic couldn't bind.
("committed to": the resolver leaves `NameOnly`/`Ambiguous` when it can't pick a target, so precision
is over edges where the graph made a definite claim. The bulk of edges are `no_occurrence` — call
sites outside the compiled subset or with no SCIP occurrence at their byte range — expected, since the
index spans the whole tree while the compilation database spans `defconfig`.)

## Rust edge resolution: heuristic vs compiler (rust-analyzer SCIP oracle)

The Rust sibling, and the contrast that matters: one heavy-tier `rust-analyzer` oracle run over **rust-lang/cargo**
(tag 0.97.1, the same pinned corpus as the iai/criterion benches), `rust-analyzer 0.3.2929`. Unlike
scip-clang's compiled subset, rust-analyzer analyzes the **whole workspace**, so these cover every
indexed `.rs` file (no subset caveat).

| Metric | Value | Meaning |
|---|---|---|
| Rust files indexed | 1,349 | the cargo workspace |
| **Compiler precision (blended)** | **81.4%** | confirmed / (confirmed + contradicted), over every judged edge kind |
| — `calls_name` | **88.6%** | function-call resolution (15,907 / 2,045) |
| — `references_type` | **73.4%** | type references (12,012 / 4,356) — after the resolution fixes below |
| **Call recall** | **95.4%** | covered / (covered + oracle-only) — the graph had a `calls_name` edge for ~all calls the compiler saw |
| Confirmed | 28,216 | heuristic target matched the compiler's |
| Contradicted | 6,437 | heuristic bound a different target than the compiler |
| Upgraded | 56,916 | edges promoted to `Compiler`-tier confidence |
| Resolved-external | 68,358 | calls/refs the compiler bound to std / a dependency crate (the bulk of cargo's) |

Read the two side by side: **call resolution `calls_name` — C 91.8% vs Rust 88.6%**; blended C 91.5%
vs Rust 81.4%. Rust's lower blend is its `references_type` at 73.4%. Three resolution fixes lifted the
Rust numbers from a 78.4% blend:

- **type-only resolution** (a `references_type` reference no longer binds to a non-type symbol — an
  `impl` block, module, etc. — in Rust/C/C++): removed 621 mis-binds, `references_type` 70.2% → 72.5%.
- **semantic scope paths** (symbols carry a `scope_path` like `Workspace::new` that aligns with an
  edge's source path, so the strong qualified match fires for methods instead of bare-name guessing):
  `calls_name` 87.5% → 88.6%, blended → 80.9%.
- **crate-aware import scope** (a name `use`d from an external dependency crate isn't bound to a local
  same-named symbol — local workspace crates are told apart from deps by scanning the corpus's
  Cargo.toml manifests, per package): `references_type` 72.5% → 73.4%, blended → 81.4%.

What keeps `references_type` at ~73% rather than higher is mostly **not a resolver bug**: a large
share is **cross-crate workspace references** — a type defined in the `cargo` crate but referenced
from a sibling member is emitted by rust-analyzer as an external moniker with no local definition, so
the oracle can't credit rag-rat's *correct* in-corpus binding. That's a measurement floor; real
`references_type` precision is meaningfully above the reported number. Recall: C 44.3% (oracle sees
only the compiled `defconfig` subset) vs Rust 95.4% (rust-analyzer sees the whole workspace). The low
in-corpus call rate is not a weakness: cargo calls overwhelmingly into `std`/dependency crates, and
the oracle correctly bins **68k** of those as `resolved-external` rather than forcing a wrong
in-corpus target.

Run them yourself via the unified runner: `CORPUS=linux-kernel bash tools/oracle-run.sh` (C) and
`CORPUS=rust-cargo bash tools/oracle-run.sh` (Rust), or dispatch `oracle.yml` with `tier=heavy` to
reproduce them on the Bencher box. The corpora are declared in `tools/oracle-corpora.toml`; the
heavy tier pins the SCIP indexer via `tools/bench.Containerfile`, so the `tool_version` baked into
every verdict is reproducible.

## Memory profile: where the peak lives

The v0.5.0 kernel index peaks at **4.15 GiB** (4,249 MiB maxrss; the 1 Hz `/proc` VmRSS sampler
agrees at 4,250 MiB). The run has **two** memory humps, and the higher one is missed by the named
per-phase probes:

1. **The edge-resolution window** (inside the rebuild transaction). The whole symbol + edge graph is
   held in memory until the single resolve-and-insert pass; once `index_targets` frees it, RSS drops
   to a flat resident baseline through logical-symbol building, FTS, and COMMIT.
2. **The embedding reconcile** (after COMMIT, where the true peak lives). `index --full` runs the
   embedding reconcile once the rebuild commits; the hash embedder is always ready, so embeddings are
   actually computed. The reconcile materializes chunk rows to embed them, adding a few GiB on top of
   the resident baseline for a sustained plateau — this is the process peak, and it is **not**
   covered by the `RAG_RAT_MEM_TRACE=1` per-phase probes (which instrument the rebuild transaction and
   stop at COMMIT). Only the whole-process sampler catches it.

Both humps shrank from v0.4.x with the #61 symbol/chunk reduction: a smaller in-memory graph lowers
the edge window, and ~62% fewer chunks lower the reconcile's materialization — together taking the
peak from 5.60 GiB to 4.15 GiB. To regenerate the per-phase curve on your own run, set
`RAG_RAT_MEM_TRACE=1` (rebuild transaction phases, to stderr) plus the sampler CSV the bench writes
(`RSS_CSV`, the post-COMMIT reconcile the MEM_TRACE probes miss).

### How the peak got here

Two earlier fixes set the pre-#61 ceiling that #61 then lowered further:

- **Streaming reconcile fix** (`d5b834e`, ~9.3 GiB → 5.5 GiB). The reconcile used to count
  policy-skipped chunks by materializing *every* chunk row — including each chunk's full `text` — into
  a `Vec`, ~4 GiB resident purely for a count, even when zero chunks end up embedded. Streaming the
  count row-by-row removed that ~4 GiB (the isolated skip-summary dropped 3950 MB → 11 MB, counts
  identical).
- **Interned edge accumulator** (`#79`/`#92`). The rebuild's edge phase was made ~4 GiB cheaper by
  interning the edge accumulator to `Sym(u32)` ids (`CompactEdge` ≈ 64 B vs 176 B), verified
  byte-identical against a golden index.

Ruled out as the peak, each checked on the real artifact rather than by reasoning:

- The **SQLite checkpoint is flat at ~28 MB**, shown three ways (default cache, 256 MB cache,
  `synchronous=OFF`) replaying the real multi-GB WAL. `mmap_size=0`, no hooks.
- **glibc arenas** account for only ~1.2 GiB (`MALLOC_ARENA_MAX=1`, a live-process malloc setting).

## Knobs

- `RAG_RAT_INDEX_WAVE` (default 2000) — full-rebuild wave size: files are prepared in parallel
  waves, so the rebuild peak ≈ one wave of prepared files + the accumulating graph. Lower it to
  trade speed for peak RSS on a memory-constrained box.
- `RAG_RAT_MEM_TRACE=1` — emit the per-phase rebuild RSS + sqlite-memory curve to stderr. (Note: it
  does not instrument the post-COMMIT reconcile — use the sampler CSV for that.)
- `RAG_RAT_KERNEL_SUBDIRS` (bench only) — bound the indexed subtree (e.g. `"kernel mm fs net lib"`)
  to go faster while iterating; `.` is the whole tree (the headline).

See [`bencher.md`](./bencher.md) and `tools/bench-kernel.sh` for running it yourself or in CI.

## Note on git-history depth

The kernel bench shallow-clones (one commit of history), so it stresses *file count*, not *history
depth* — these are independent axes. Git-history indexing reads the full reachable history
(`git log --numstat`, O(total history)); on a deep-history repo that cost is gated to run only when
HEAD actually changes (`git_history::is_history_current`), so the steady-state watcher cost does not
scale with history depth.
