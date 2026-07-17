# Measured benchmarks

Concrete numbers for the headline workload — indexing the whole Linux kernel — plus the memory
profile that workload exposes and how syntactic resolution compares to a real compiler. This is the
*results* companion to [`bencher.md`](./bencher.md), which documents the *harness* (Bencher, the CI
workflows, and `tools/bench-kernel.sh`). Numbers here are single cold runs, not statistically-gated
CI signals; treat them as "what one run looks like," not a regression gate.

This file stores only the **current** numbers — one recent release-tier run, identified where it is
quoted. The history (every release run, every measure) lives in the Bencher series `bench-release`
feeds; comparisons across versions belong there, not here.

## Test harness

The kernel index runs on a self-hosted big-memory box (the `bench-release` workflow's
`[self-hosted, bigmem]` runner), inside the pinned bench container so the toolchain and SCIP indexers
are reproducible. It runs here rather than on a hosted runner for a stable wall-clock — not because
it no longer fits a hosted box (see the peak below).

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
full index (`index --full`), whole tree (`RAG_RAT_KERNEL_SUBDIRS=.`), no embedding model
(`model = "none"`; built `--no-default-features`, so no model download), release build,
`RAG_RAT_INDEX_WAVE=2000`. Run: one **v0.19.0** release run (`bench-release`, self-hosted bigmem box), single cold rebuild.

| Metric | Value |
|---|---|
| Files indexed (C/H) | 62,903 |
| **Wall-clock** | **412.1 s** (6.87 min) |
| Throughput | 152.6 files/s |
| **Peak RSS** | **3.40 GiB** maxrss (3,321 MiB caught by the 1 Hz sampler) |
| On-disk index DB (structure) | 4.72 GiB (post-checkpoint; the structure half of the [size split](#db-size-structure-vs-vectors)) |
| Symbols | 1,057,527 |
| Edges (call graph) | 9,144,061 |
| Edges resolved | 5,320,333 (58.2%) |
| Chunks | 1,611,390 |

Wall-clock is one cold rebuild on a shared box (the runner also hosts unrelated services), so any
single run carries tens of seconds of noise; the Bencher series is the trend, one run is a sample.
The extracted-fact counts (symbols, edges, chunks) are deterministic for the pinned kernel commit.

Unresolved-edge taxonomy (the 41.8% / 3,823,728 edges the syntactic graph leaves
dangling, by kind): `calls_name` 1,861,733 (48.7%), `references_type` 1,560,142 (40.8%),
`imports` 401,853 (10.5%). The `calls_name` bucket is extern / macro / function-pointer call
targets the syntactic resolver can't bind without a compilation database; `references_type` is
dominated by references whose type *definition* lives in an uncompiled or external header — both are
exactly what the SCIP oracle recovers (below).

### DB size: structure vs vectors

"Indexing the kernel costs N GB" is only an honest number if it says what's *in* the N. The bench
reports the size as a **split** (#78), because the two halves are bought separately:

| Half | What it is | BMF measure | Measured |
|---|---|---|---|
| **Structure** (the headline) | files + symbols + graph + chunks + FTS + git history. `model = "none"` — zero vectors. What a base install actually builds. | `db_size` | 4.72 GiB |
| **+ Vectors** | every chunk the embedding policy admits, embedded on top of the same index. Opt-in second pass. | `db_size_with_vectors`, `db_size_vectors_delta` | +1.54 GiB (24.6% of the full DB) |

The headline pass runs `model = "none"` — the honest config for this corpus (nobody does vector
recall over the Linux kernel with a hash embedder), and `index --full` computes no embeddings
regardless: the reconcile is a separate, explicit pass that refuses to embed under a model that was
never installed. The split is what makes the degradability claim ("base install ships zero AI; add
what you use") a **measured** number: `db_size_vectors_delta` is exactly the disk you avoid by not
adding the embedding tier. Vectors are stored int8-quantized (#112: a 4-byte scale + one byte per
dim, ~4× smaller than the f32 blob), which is why the tier is a modest fraction rather than the
dominant one.

The `+vectors` pass is off by default. Turn it on with:

```bash
RAG_RAT_KERNEL_VECTORS=1 RAG_RAT_BIN=target/release/rag-rat bash tools/bench-kernel.sh
```

## C edge resolution: heuristic vs compiler (SCIP oracle)

The headline resolves 58.2% of edges *syntactically* — by name, no compiler. The SCIP
oracle (#61) measures how good that syntactic resolution actually is: it replays a real compilation
through `scip-clang` (a clang-based SCIP indexer) and diffs its ground-truth bindings against the
heuristic's. Numbers below are one heavy-tier `scip-clang` oracle run (`oracle.yml`, `scip-clang
0.4.0`, kernel `defconfig`, containerized bench image on the self-hosted bigmem box), so they cover
the **compiled subset** — the translation units `defconfig` actually compiles — not the whole
62,903-file tree the headline indexes. Resolution *quality* and tree-wide *coverage* are different
populations, reported side by side, not merged.

| Metric | Value | Meaning |
|---|---|---|
| Compiled TUs | 2,956 | the `defconfig` compilation database scip-clang consumes |
| **Compiler precision (blended)** | **92.8%** | confirmed / (confirmed + contradicted), over every judged edge kind |
| — `calls_name` | **91.8%** | function-call resolution (315,205 confirm / 28,071 contradict) |
| — `references_type` | **94.2%** | type references (211,868 / 13,122) |
| Call recall | 32.0% | covered / (covered + oracle-only) — of calls the compiler saw, the share the graph had a `calls_name` edge for |
| Confirmed | 527,073 | heuristic target matched the compiler's |
| Contradicted | 41,193 | heuristic bound a different target than the compiler |
| Upgraded | 346,091 | edges promoted to `Compiler`-tier confidence |
| Resolved-external | 29,535 | dangling refs the compiler bound to a cross-TU / external symbol the heuristic couldn't reach |

On the compiled subset, name-based resolution agrees with the compiler **~93% of the time** on the
edges it commits to — and that figure exists because the oracle first caught a real indexing bug.
The first oracle read was an alarming 50.1% blended; splitting by edge kind exposed
`references_type` at 18% while `calls_name` was already 85%. The root cause: the C/C++ parser
emitted a symbol for *every* `struct`/`union`/`enum` specifier — forward declarations and bare uses
included — so type references bound to tiny bodyless pseudo-symbols instead of real definitions.
The **definitions-only** fix (#61) — a specifier must carry its body; bare prototypes are dropped —
moved both kinds into the 90s, and as a side effect removed ~⅔ of the kernel's "symbols" and chunks
(bodyless prototype/forward-decl occurrences, not real code), which is also what made whole-kernel
indexing fast. Lower symbol counts and a lower resolved *rate* than pre-#61 both reflect higher
precision, not lost coverage.

("committed to": the resolver leaves `NameOnly`/`Ambiguous` when it can't pick a target, so
precision is over edges where the graph made a definite claim. The bulk of edges are
`no_occurrence` — call sites outside the compiled subset or with no SCIP occurrence at their byte
range — expected, since the index spans the whole tree while the compilation database spans
`defconfig`.)

## Rust edge resolution: heuristic vs compiler (rust-analyzer SCIP oracle)

The Rust sibling, and the contrast that matters: one heavy-tier `rust-analyzer` oracle run over
**rust-lang/cargo** (tag 0.97.1, `rust-analyzer 0.3.2929`). The corpus binds the `src` + `crates`
workspace paths — **355** `.rs` files, cargo's library and workspace-member sources (not its large
`tests/` tree). Unlike scip-clang's compiled subset, rust-analyzer analyzes the whole workspace, so
the oracle covers every one of those indexed `.rs` files — no compiled-subset gap like C.

| Metric | Value | Meaning |
|---|---|---|
| Rust files indexed | 355 | the `src` + `crates` workspace paths |
| **Compiler precision (blended)** | **81.7%** | confirmed / (confirmed + contradicted), over every judged edge kind |
| — `calls_name` | **83.1%** | function-call resolution (5,378 / 1,096) |
| — `references_type` | **80.9%** | type references (10,743 / 2,536) |
| **Call recall** | **93.2%** | covered / (covered + oracle-only) — the graph had a `calls_name` edge for ~all calls the compiler saw |
| Confirmed | 16,348 | heuristic target matched the compiler's |
| Contradicted | 3,668 | heuristic bound a different target than the compiler |
| Upgraded | 12,777 | edges promoted to `Compiler`-tier confidence |
| Resolved-external | 51,201 | calls/refs the compiler bound to std / a dependency crate (the bulk of cargo's) |

Read the two side by side: **call resolution `calls_name` — C 91.8% vs Rust 83.1%**; blended C 92.8%
vs Rust 81.7%. Rust's lower blend is spread across both kinds, each a few points under C's. Three
resolution fixes got the Rust blend to where it is:

- **type-only resolution** — a `references_type` reference no longer binds to a non-type symbol (an
  `impl` block, module, etc.) in Rust/C/C++.
- **semantic scope paths** — symbols carry a `scope_path` like `Workspace::new` that aligns with an
  edge's source path, so the strong qualified match fires for methods instead of bare-name guessing.
- **crate-aware import scope** — a name `use`d from an external dependency crate isn't bound to a
  local same-named symbol; local workspace crates are told apart from deps by scanning the corpus's
  Cargo.toml manifests, per package.

Part of what keeps `references_type` shy of C's is **not a resolver bug**: a share is **cross-crate
workspace references** — a type defined in one workspace crate but referenced from a sibling member
is emitted by rust-analyzer as an external moniker with no local definition, so the oracle can't
credit rag-rat's *correct* in-corpus binding. That's a measurement floor; real `references_type`
precision is somewhat above the reported number. Recall: C 32.0% (oracle sees only the compiled
`defconfig` subset) vs Rust 93.2% (rust-analyzer sees the whole workspace). The low in-corpus call
rate is not a weakness: cargo calls overwhelmingly into `std`/dependency crates, and the oracle
correctly bins **51k** of those as `resolved-external` rather than forcing a wrong in-corpus target.

Run them yourself via the unified runner: `CORPUS=linux-kernel bash tools/oracle-run.sh` (C) and
`CORPUS=rust-cargo bash tools/oracle-run.sh` (Rust), or dispatch `oracle.yml` with `tier=heavy` to
reproduce them on the Bencher box. The corpora are declared in `tools/oracle-corpora.toml`; the
heavy tier pins the SCIP indexer via `tools/bench.Containerfile`, so the `tool_version` baked into
every verdict is reproducible.

## Memory profile: where the peak lives

The kernel index peaks at **3.40 GiB** maxrss (the 1 Hz `/proc` VmRSS sampler undershoots
the instantaneous peak slightly). The run has **two** memory humps, and the higher one is missed by
the named per-phase probes:

1. **The edge-resolution window** (inside the rebuild transaction). The whole symbol + edge graph is
   held in memory until the single resolve-and-insert pass; once `index_targets` frees it, RSS drops
   to a flat resident baseline through logical-symbol building, FTS, and COMMIT.
2. **A post-COMMIT plateau** (where the true peak lives), **cause not yet attributed**. A few GiB
   land on top of the resident baseline after COMMIT for a sustained plateau — the process peak —
   and it is **not** covered by the `RAG_RAT_MEM_TRACE=1` per-phase probes (which instrument the
   rebuild transaction and stop at COMMIT). Only the whole-process sampler catches it. It is **not**
   embedding work — `index --full` computes no embeddings (a bench DB holds zero vector rows) — so
   the open suspect is the pinned kernel's full-history `git log --numstat` walk, the other
   post-rebuild bulk pass (unverified).

Ruled out as the peak, each checked on the real artifact rather than by reasoning: the **SQLite
checkpoint is flat at ~28 MB** (shown with the default cache, a 256 MB cache, and
`synchronous=OFF`, replaying the real multi-GB WAL; `mmap_size=0`, no hooks), and **glibc arenas**
account for only ~1.2 GiB (`MALLOC_ARENA_MAX=1`, a live-process malloc setting).

To regenerate the per-phase curve on your own run, set `RAG_RAT_MEM_TRACE=1` (rebuild transaction
phases, to stderr) plus the sampler CSV the bench writes (`RSS_CSV`, the post-COMMIT plateau the
MEM_TRACE probes miss).

## Knobs

- `RAG_RAT_INDEX_WAVE` (default 2000) — full-rebuild wave size: files are prepared in parallel
  waves, so the rebuild peak ≈ one wave of prepared files + the accumulating graph. Lower it to
  trade speed for peak RSS on a memory-constrained box.
- `RAG_RAT_MEM_TRACE=1` — emit the per-phase rebuild RSS + sqlite-memory curve to stderr. (Note: it
  does not instrument the post-COMMIT phases — use the sampler CSV for those.)
- `RAG_RAT_KERNEL_SUBDIRS` (bench only) — bound the indexed subtree (e.g. `"kernel mm fs net lib"`)
  to go faster while iterating; `.` is the whole tree (the headline).
- `RAG_RAT_KERNEL_VECTORS=1` (bench only) — add the opt-in `+vectors` pass to the run, reporting
  `db_size_with_vectors` / `db_size_vectors_delta` on top of the structure-only headline.

See [`bencher.md`](./bencher.md) and `tools/bench-kernel.sh` for running it yourself or in CI.

## Note on git-history depth

The kernel bench shallow-clones (one commit of history), so it stresses *file count*, not *history
depth* — these are independent axes. Git-history indexing reads the full reachable history
(`git log --numstat`, O(total history)); on a deep-history repo that cost is gated to run only when
HEAD actually changes (`git_history::is_history_current`), so the steady-state watcher cost does not
scale with history depth.
