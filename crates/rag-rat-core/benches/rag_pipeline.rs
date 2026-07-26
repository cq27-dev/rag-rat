//! Instruction-count benchmarks for the rag-rat pipeline (iai-callgrind).
//!
//! These measure deterministic CPU instruction counts (via callgrind), not wall time, so they are
//! immune to CI noise and good for catching *relative* regressions. Needs `valgrind` + the
//! matching `iai-callgrind-runner` on PATH (`cargo install iai-callgrind-runner@<lib version>`).
//!
//! Kept on a tiny corpus subtree because iai-callgrind runs even the (uncounted) `setup` index
//! builds under valgrind's ~50x slowdown. Wall-clock full-index time on a larger corpus is a
//! separate criterion bench (index_time.rs). The corpus harness is shared (benches/shared).

mod shared;

use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use rag_rat_base::config::Config;
use rag_rat_core::IndexDatabase;
use shared::{bench_config, built_config, built_index, open_like_production};

/// Small subtree — keeps each callgrind run (including setups) fast.
const SUBDIR: &str = "src/cargo/core/resolver";
const QUERY: &str = "resolve dependency version conflict";

fn resolver_config() -> (Config, rag_rat_base::test_scratch::ScratchDir) {
    bench_config(SUBDIR)
}
fn resolver_index() -> (IndexDatabase, rag_rat_base::test_scratch::ScratchDir) {
    built_index(SUBDIR)
}
fn resolver_built_config() -> (Config, rag_rat_base::test_scratch::ScratchDir) {
    built_config(SUBDIR)
}

// Index cost: full rebuild of the subtree. Setup (clone + Config) is not measured; only `rebuild`.
#[library_benchmark]
#[bench::cargo_resolver(setup = resolver_config)]
fn index(setup: (Config, rag_rat_base::test_scratch::ScratchDir)) -> IndexDatabase {
    IndexDatabase::rebuild(&setup.0).expect("rebuild corpus index")
}

// Cold query: open a freshly-built index from disk (cold page cache) the way production `search`
// does (open + active-checkout context, offline), then run one search — the open is INSIDE the
// measured body, so this captures the realistic cold-open cost. Setup (rebuild) is not measured.
#[library_benchmark]
#[bench::cargo_resolver(setup = resolver_built_config)]
fn query_cold(setup: (Config, rag_rat_base::test_scratch::ScratchDir)) -> usize {
    let db = open_like_production(&setup.0);
    db.search(QUERY, 10, false).expect("search").len()
}

// Warm query: search against an already-open index (the build is in setup, not measured).
#[library_benchmark]
#[bench::cargo_resolver(setup = resolver_index)]
fn query_warm(setup: (IndexDatabase, rag_rat_base::test_scratch::ScratchDir)) -> usize {
    setup.0.search(QUERY, 10, false).expect("search").len()
}

library_benchmark_group!(
    name = pipeline;
    benchmarks = index, query_cold, query_warm
);

main!(library_benchmark_groups = pipeline);
