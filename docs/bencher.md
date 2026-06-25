# Continuous benchmarking (Bencher)

rag-rat tracks performance over time with [Bencher](https://bencher.dev). Two lightweight
*performance* signals are recorded on every push to `main` and gated on every pull request; a
*search-quality* signal (commit-replay recall) is tracked on `main` only; and a heavyweight
Linux-kernel indexing signal runs only on a published release:

| Signal | Harness | When | What it measures | Why |
|---|---|---|---|---|
| `rag_pipeline` | [iai-callgrind](https://github.com/iai-callgrind/iai-callgrind) | push + PR | CPU **instruction counts** for index rebuild + cold/warm query | Deterministic — immune to CI runner noise, so it catches small *relative* regressions a wall-clock bench would drown out. |
| `index_time` | [criterion](https://github.com/bheisler/criterion.rs) | push + PR | **Wall-clock** time for a full index rebuild (whole cargo checkout) | The number users actually feel. Noisy, so it's gated statistically (t-test), not on a tight percentage. |
| `search_replay` | commit-replay eval → [`tools/eval-report-bmf.py`](../tools/eval-report-bmf.py) | **push to `main` only** | **recall@3 / recall@10 / mrr@10** of hybrid search over the self-index at HEAD | The retrieval quality an agent actually feels ("did the right chunk land in the first 3 reads"). ~8–10 min + noisy (the replay gold is recent commit paths), so it's tracked + alerted on `main`, never a PR gate. |
| `linux-kernel-v7.0/full-index` | single-shot ([`tools/bench-kernel.sh`](../tools/bench-kernel.sh)) | **release only** | **Wall-clock + throughput + peak memory** to index the whole Linux kernel | The headline "indexes the Linux kernel in X seconds" number on a huge real C codebase. Too slow (~tens of minutes) for per-push. |

The push/PR harnesses run on different corpus sizes on purpose — see "Corpus" below.

## Benches

Both benches live in `crates/rag-rat-core/benches/` and share a corpus harness
(`benches/shared/mod.rs`, kept in a subdirectory so cargo doesn't auto-detect it as a bench):

- `rag_pipeline.rs` (iai-callgrind, `harness = false`) — three `#[library_benchmark]`s: `index`
  (rebuild), `query_cold` (`open` + search from disk), `query_warm` (search on an open db).
- `index_time.rs` (criterion, `harness = false`) — one `full_rebuild` benchmark.

Run them locally:

```bash
# Instruction counts (needs valgrind + iai-callgrind-runner on PATH — see below)
cargo bench --no-default-features --bench rag_pipeline

# Wall-clock full-index time
cargo bench --no-default-features --bench index_time
```

`--no-default-features` builds the **hash embedder only** — no ONNX / model downloads, no network,
fully deterministic. The benches assert nothing about embedding *quality*; they measure the
indexing and retrieval *machinery*, which must stay model-agnostic to be reproducible in CI.

### Local prerequisites for the iai bench

iai-callgrind runs the benched code under valgrind/callgrind and needs a matching runner binary:

```bash
sudo apt-get install -y valgrind                     # or your distro's package
cargo install --locked iai-callgrind-runner@0.16.1   # MUST match the iai-callgrind lib version in Cargo.toml
```

The criterion bench has no special prerequisites.

## Corpus

Both benches index a pinned snapshot of an external repo so the workload is realistic and stable
across runs. The corpus is **not vendored into git**; the harness shallow-clones it on first use and
caches it under `target/bench-corpus/`:

- Repo: `rust-lang/cargo`, pinned by **commit SHA** (tag `0.97.1`).
- Shallow fetch of just that commit (`git init` + `fetch --depth 1 origin <sha>`), so the clone is
  small and the content can never drift.
- Override the location with `RAG_RAT_BENCH_CORPUS=/path/to/checkout` to point at an existing
  checkout (useful offline).

The iai bench indexes a **small** subtree (`src/cargo/core/resolver`, ~10 files) because callgrind's
~50x slowdown applies even to the uncounted `setup` index builds — a large subtree would time the
job out. The criterion bench indexes the **whole checkout** (~1.3k `.rs` files) — the realistic
"index this repo" workload `rag-rat index` actually performs — since it runs at native speed; it
reports the indexed `file_count_by_language` and files/sec throughput so the output shows real usage.
If you bump the pinned SHA, update the cache key (`bench-corpus-cargo-<version>`) in all three
workflow files too.

## Search-quality: commit-replay recall

`bench.yml`'s `eval_recall` job tracks how well search retrieves the right code over time, using the
commit-replay eval (#120): each recent commit becomes a case (commit message = query, the commit's
changed paths = recall gold), scored against the hybrid (lexical + semantic) index at HEAD. Three
measures go to Bencher under the `search_replay` benchmark:

- `recall_at_3` — the headline: |gold ∩ top-3| / |gold|, "did the right chunk land in the first 3
  reads". It carries a statistical **lower**-boundary t-test threshold (a recall regression is a
  *drop*, unlike the latency/instruction signals, which alert on an *increase*).
- `recall_at_10`, `mrr_at_10` — tracked without a threshold (context for a recall@3 move).

Values are fractions in [0, 1] — the same numbers `eval` prints — so a Bencher plot reads identically
to a local run.

The job builds an embedded index at HEAD (`index --discover` → `models install
fastembed-all-minilm-l6-v2` → `reconcile`), then `rag-rat --json eval --replay --replay-max-cases
200` → [`tools/eval-report-bmf.py`](../tools/eval-report-bmf.py) → `bencher run --adapter json`. Two
footguns it has to handle:

- **Off-head ⇒ bogus 0.000.** `reconcile` only embeds *at-head* chunks, so the index must be built +
  embedded at HEAD before scoring; a stale/absent index makes every recall read 0.000.
- **Shallow checkout ⇒ no cases.** The replay walks git history, so the job checks out with
  `fetch-depth: 0` — the default depth-1 checkout would leave it with only HEAD and zero cases.

**Main-only, no `--err`, by design.** At ~8–10 min over 200 cases it's too slow for the PR critical
path, and the metric is noisy — the gold is recent commit paths, so a single PR's recall delta is
usually below the per-case noise floor (≥200 cases are needed for signal). So it's tracked +
*alerted* on `main` (the statistical threshold flags a real regression in Bencher) rather than
hard-failing a run — the same "headline signal, not a gate" stance as the iai/criterion series. Reset
the baseline after an embedder or replay-parameter change with **Actions → bench → Run workflow** (the
`workflow_dispatch` reruns `--thresholds-reset`).

Run it locally (needs the `eval` feature + an embedded at-head index):

```bash
cargo build --release -p rag-rat --features eval
target/release/rag-rat --json eval --replay --replay-max-cases 200 > eval.json
python3 tools/eval-report-bmf.py eval.json > eval_bmf.json
```

## CI workflows

Four workflows under `.github/workflows/`:

| Workflow | Trigger | Has the token? | Role |
|---|---|---|---|
| `bench.yml` | push to `main` | yes | Records the lightweight perf signals **and** the `search_replay` recall signal (a separate `eval_recall` job) against `main`; sets/refreshes the thresholds new PRs are compared against. |
| `bench-pr-run.yml` | `pull_request` (incl. forks) | **no** | Runs the benches, uploads raw output + the PR event as an artifact. Never sees secrets, so a fork PR can't exfiltrate the token. |
| `bench-pr-track.yml` | `workflow_run` of bench-pr-run | yes | Runs in the base-repo context: downloads the artifact, uploads results to Bencher, comments the comparison on the PR. |
| `bench-release.yml` | `release: published` + manual `workflow_dispatch` | yes | The heavyweight Linux-kernel headline bench (below). Not a gate — tracks latency/throughput/memory over releases. |

The `pull_request` → `workflow_run` split is Bencher's documented fork-safe pattern: untrusted fork
code runs without secrets, and only the trusted base-repo workflow (which never executes fork code)
holds the API token. PR runs use `--start-point main --start-point-clone-thresholds
--start-point-reset` so each PR is measured against a fresh clone of `main`'s baseline + thresholds.

## Release headline: indexing the Linux kernel

`tools/bench-kernel.sh` (run by `bench-release.yml`) is the "indexes the Linux kernel in X seconds"
benchmark. It shallow-clones a pinned kernel tag (`KERNEL_TAG`, default `v7.0`), indexes its C/H
sources **once** with the release `rag-rat` binary (`index --full`, `--no-default-features` =
hash embedder, no model download), and writes a Bencher Metric Format JSON file with eight measures:

- `latency` — wall-clock seconds to index, in nanoseconds (Bencher's built-in Latency measure).
- `throughput` — indexed files per second.
- `memory` — peak resident set size in bytes, from `getrusage(RUSAGE_CHILDREN).ru_maxrss` (the
  kernel-true peak, catches sub-sample spikes).
- `memory_sampled` — peak of a 1 Hz `/proc/<pid>/status` VmRSS time-series (also written to
  `kernel_rss_samples.csv` as the before/after curve).
- `symbols`, `edges`, `resolved_edges`, `chunks` — extracted-fact counts, so a run is comparable
  per fact and not just per file (the unresolved-edge taxonomy goes to `kernel_unresolved_by_kind.csv`).

It's a **single cold rebuild**, not a criterion loop: the whole kernel is ~63k C/H files and takes
tens of minutes, so criterion's 10-sample minimum (×10) is a non-starter. One run is also exactly
the number a user sees.

Run it locally or trigger it manually before relying on it for a release:

```bash
cargo build --release --no-default-features --bin rag-rat
RAG_RAT_BIN=target/release/rag-rat bash tools/bench-kernel.sh   # whole tree (~tens of min, several GB RAM)

# Bound the scope (faster, less memory) while iterating:
RAG_RAT_KERNEL_SUBDIRS="kernel mm fs net lib" RAG_RAT_BIN=target/release/rag-rat bash tools/bench-kernel.sh
```

Or in CI: **Actions → bench-release → Run workflow** (the `workflow_dispatch` input sets the subtree).

**Runner sizing.** This job runs on the self-hosted `[self-hosted, bigmem]` runner, not a
hosted one — the whole-tree index peaks at ~5.5 GiB (see [`benchmarks.md`](./benchmarks.md)), which
plus the OS page cache over a ~10 GB index DB is borderline for a hosted runner and, more to the
point, would give a noisy, contended wall-clock. If a run OOMs or overruns `timeout-minutes` on a
constrained box, bound `RAG_RAT_KERNEL_SUBDIRS` to the core subsystems. Validate with a manual
dispatch first.
