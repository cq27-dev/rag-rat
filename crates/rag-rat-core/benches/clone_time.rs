//! Wall-clock clone-query time (criterion) — the perf half of the clone measurement infrastructure
//! (#279). Gives the three clone-perf prototypes a real before/after signal so none merges on faith
//! of a speedup:
//!
//! - `candidate_components` — `candidate_clone_components` (the SourcererCC candidate-pair gen +
//!   union-find, NO refine, NO cache). This is the bulk of what the #270 full-scan reverse lookup
//!   pays and the exact step the #271 hot-token cap prunes.
//! - `clones_for_symbol` — the #270 reverse-lookup entry point on a real clone-member subject. Its
//!   candidate scan is recomputed every call (only refine is cached), so this measures the
//!   full-scan cost the #270 BFS prototype targets.
//! - `find_clones_cold` — the whole ranked pass incl. refine, with an EMPTY refine cache each
//!   iteration (a fresh rebuild in the untimed `iter_batched` setup). This is the ~48–52 s pass the
//!   #272 global refine budget bounds. There is no public refine-cache clear, so a fresh build is
//!   the only honest cold measurement.
//!
//! Noisy (wall-clock, small sample) — rely on Bencher's statistical threshold to gate regressions.
//! Corpus harness shared (benches/shared); subdir kept moderate so the cold rebuild-per-iteration
//! stays tractable.

mod shared;

use std::path::PathBuf;
use std::time::Duration;
use std::{fs, hint};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use rag_rat_core::index::{CloneSymbolSelector, FindClonesOptions};
use shared::{built_config, corpus_dir, open_like_production};

/// A moderate, clone-dense subtree of the corpus — big enough to exercise real candidate generation
/// and refine, small enough that the cold bench's per-iteration rebuild stays bounded. `src/cargo`
/// is guaranteed present (the corpus-clone marker checks it).
const SUBDIR: &str = "src/cargo";

fn default_opts() -> FindClonesOptions {
    FindClonesOptions { min_similarity: None, min_copies: None, limit: None }
}

/// Best-effort removal of a bench DB's on-disk files (`.sqlite` + `-wal`/`-shm`) when dropped, so a
/// long run doesn't leak a corpus-sized DB per build — the same discipline as `index_time`'s
/// `TempIndexDb` (#251). Declared so it drops AFTER the `IndexDatabase` handle it pairs with (the
/// handle closes the connection first, then the files are unlinked).
struct DbFiles(PathBuf);

impl Drop for DbFiles {
    fn drop(&mut self) {
        let db = self.0.display().to_string();
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{db}{suffix}"));
        }
    }
}

fn clone_queries(c: &mut Criterion) {
    // Clone the corpus once, before timing.
    let _ = corpus_dir();

    // One shared index for the cache-free / warm benches and to resolve a real subject. Declared
    // before `db` so `db` (the open handle) drops first and `DbFiles` unlinks afterwards.
    let config = built_config(SUBDIR);
    let _shared_cleanup = DbFiles(config.database.clone());
    let db = open_like_production(&config);

    let components = db.candidate_clone_components().expect("candidate components");
    // Resolve a real clone-member `ref` (a `path::symbol`) to drive `clones_for_symbol`; `None`
    // when the subtree has no clone class (then that bench is skipped rather than measuring a
    // miss).
    let subject_ref = db
        .find_clones(FindClonesOptions { limit: Some(5), ..default_opts() })
        .ok()
        .and_then(|r| r.classes.into_iter().find_map(|cls| cls.members.into_iter().next()))
        .map(|m| m.r#ref);
    eprintln!(
        "clone_time: {SUBDIR} → {} candidate components; subject = {subject_ref:?}",
        components.len()
    );

    let mut group = c.benchmark_group("clone_time");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));

    // 1. Candidate generation only — cache-free, recomputed every call (#270 full-scan / #271 cap).
    group.bench_function("candidate_components", |b| {
        b.iter(|| hint::black_box(db.candidate_clone_components().expect("candidate components")));
    });

    // 2. The #270 reverse lookup on a real subject (skip if the subtree has no clone class).
    if let Some(subject) = subject_ref {
        group.bench_function("clones_for_symbol", |b| {
            b.iter(|| {
                let r = db
                    .clones_for_symbol(CloneSymbolSelector::Ref(subject.clone()))
                    .expect("clones_for_symbol");
                hint::black_box(r);
            });
        });
    }
    group.finish();

    // 3. Cold `find_clones` — the whole ranked+refined pass with an EMPTY refine cache each
    // iteration. A fresh rebuild in the (untimed) `iter_batched` setup gives the empty cache; only
    // the `find_clones` call is measured. The per-iteration DB is dropped via `DbFiles` so at most
    // one corpus-sized DB sits on disk at a time (#251).
    let mut cold = c.benchmark_group("clone_time_cold");
    cold.sample_size(10);
    cold.measurement_time(Duration::from_secs(180));
    cold.bench_function("find_clones_cold", |b| {
        b.iter_batched_ref(
            || {
                let config = built_config(SUBDIR);
                let cleanup = DbFiles(config.database.clone());
                let db = open_like_production(&config);
                // (db, cleanup): db drops first (closes the connection), then cleanup unlinks.
                (db, cleanup)
            },
            |(db, _cleanup)| {
                hint::black_box(db.find_clones(default_opts()).expect("find_clones"));
            },
            BatchSize::PerIteration,
        );
    });
    cold.finish();
}

criterion_group!(benches, clone_queries);
criterion_main!(benches);
