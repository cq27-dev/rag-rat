//! Wall-clock full-index time (criterion).
//!
//! Complements the deterministic iai-callgrind instruction counts with a real wall-time signal for
//! a full index rebuild. No callgrind slowdown here, so — unlike the tiny iai subtree — this
//! indexes the **whole** cargo checkout (every `.rs` in the repo, ~1.3k files), which is what
//! `rag-rat index` actually does when pointed at a real repository. Noisier than iai — rely on
//! Bencher's statistical threshold (t-test) to gate regressions. The corpus harness is shared
//! (benches/shared).

mod shared;

use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use rag_rat_core::{Config, IndexDatabase};
use shared::{bench_config, corpus_dir};

/// Index the entire corpus checkout — the realistic "index this repo" workload, not a cherry-picked
/// subtree. `bench_config` targets every `**/*.rs` under this root.
const SUBDIR: &str = ".";

/// Owns a bench temp DB so its on-disk files are removed when the value is dropped.
/// `iter_batched_ref` drops the per-iteration value AFTER the timed routine, so a long
/// `full_rebuild_cargo` run keeps at most one corpus-sized DB on disk instead of leaking one
/// (~1.5-2 GiB) per iteration — ~55 iterations would otherwise accumulate ~80-110 GiB of
/// `/tmp/rag-rat-bench-*.sqlite` and panic with `database or disk is full` on a space-constrained
/// box (#251).
struct TempIndexDb {
    config: Config,
}

impl Drop for TempIndexDb {
    fn drop(&mut self) {
        let db = self.config.database.display().to_string();
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{db}{suffix}"));
        }
    }
}

fn full_index(c: &mut Criterion) {
    // Clone the corpus once, before timing.
    let _ = corpus_dir();

    // Build one index up front to report the real scale being measured (and to size throughput).
    // This is the headline number a user cares about: how big a repo are we actually indexing?
    let probe = bench_config(SUBDIR);
    let db = IndexDatabase::rebuild(&probe).expect("probe rebuild");
    let status = db.status(&probe.database).expect("index status");
    let files: u64 = status.file_count_by_language.values().sum();
    eprintln!(
        "index_time: indexing whole cargo checkout — {files} files {:?}",
        status.file_count_by_language
    );

    let mut group = c.benchmark_group("index_time");
    // Report files/sec, so the bench output shows real indexing throughput, not just opaque
    // latency.
    group.throughput(Throughput::Elements(files));
    // Each rebuild of the full repo takes seconds; take the criterion minimum sample count over a
    // window wide enough to avoid the "couldn't complete in time" warning leaking onto stdout.
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(180));
    group.bench_function("full_rebuild_cargo", |b| {
        // `iter_batched_ref` drops each setup value after the timed routine, so `TempIndexDb`'s
        // Drop deletes the per-iteration temp DB between iterations — keeping at most one
        // on disk rather than leaking one per iteration (#251). The deletion is outside the
        // measured section.
        b.iter_batched_ref(
            || TempIndexDb { config: bench_config(SUBDIR) },
            |bench_db| {
                let db = IndexDatabase::rebuild(&bench_db.config).expect("rebuild corpus index");
                std::hint::black_box(db);
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(benches, full_index);
criterion_main!(benches);
