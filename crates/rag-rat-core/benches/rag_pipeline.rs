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

fn resolver_config() -> rag_rat_base::config::Config {
    bench_config(SUBDIR)
}
fn resolver_index() -> IndexDatabase {
    built_index(SUBDIR)
}
fn resolver_built_config() -> Config {
    built_config(SUBDIR)
}

// Index cost: full rebuild of the subtree. Setup (clone + Config) is not measured; only `rebuild`.
#[library_benchmark]
#[bench::cargo_resolver(setup = resolver_config)]
fn index(config: rag_rat_base::config::Config) -> IndexDatabase {
    IndexDatabase::rebuild(&config).expect("rebuild corpus index")
}

// Cold query: open a freshly-built index from disk (cold page cache) the way production `search`
// does (open + active-checkout context, offline), then run one search — the open is INSIDE the
// measured body, so this captures the realistic cold-open cost. Setup (rebuild) is not measured.
#[library_benchmark]
#[bench::cargo_resolver(setup = resolver_built_config)]
fn query_cold(config: Config) -> usize {
    let db = open_like_production(&config);
    db.search(QUERY, 10, false).expect("search").len()
}

// Warm query: search against an already-open index (the build is in setup, not measured).
#[library_benchmark]
#[bench::cargo_resolver(setup = resolver_index)]
fn query_warm(db: IndexDatabase) -> usize {
    db.search(QUERY, 10, false).expect("search").len()
}

library_benchmark_group!(
    name = pipeline;
    benchmarks = index, query_cold, query_warm
);

main!(library_benchmark_groups = pipeline);
