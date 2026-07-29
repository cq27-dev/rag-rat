//! Shared corpus + config helpers for the rag-rat benches. Not a bench target itself (it lives in
//! a subdirectory, so cargo does not auto-detect it).

#![allow(dead_code)] // each bench uses a subset of these helpers

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use rag_rat_base::config::{Config, ResolvedTarget, TargetKind};
use rag_rat_base::language::Language;
use rag_rat_core::IndexDatabase;

const CORPUS_REPO: &str = "https://github.com/rust-lang/cargo.git";
/// cargo tag 0.97.1 — pinned by commit SHA for reproducibility.
const CORPUS_SHA: &str = "fc1044d6129608b3a3188566a919dc6126f7cb15";

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(status.success(), "git {args:?} failed");
}

/// Shallow-clone the corpus pinned to `CORPUS_SHA` into a cached dir, once. Idempotent — a present
/// checkout is reused (CI caches this path). Override the base dir with `RAG_RAT_BENCH_CORPUS`.
pub fn corpus_dir() -> PathBuf {
    let base = env::var_os("RAG_RAT_BENCH_CORPUS").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bench-corpus")
    });
    let dir = base.join(format!("cargo-{}", &CORPUS_SHA[..12]));
    if !dir.join("src/cargo").exists() {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create corpus dir");
        run_git(&dir, &["init", "-q"]);
        run_git(&dir, &["remote", "add", "origin", CORPUS_REPO]);
        run_git(&dir, &["fetch", "--depth", "1", "-q", "origin", CORPUS_SHA]);
        run_git(&dir, &["checkout", "-q", CORPUS_SHA]);
    }
    dir
}

/// A guarded scratch dir (removed when the bench's value drops) holding the bench database.
/// Every call is a fresh dir, so every bench run indexes into a truly fresh database — a stale
/// DB would silently change measured query cost (the old pid+counter name reuse, ±25% on
/// query_warm).
fn temp_db() -> rag_rat_base::test_scratch::ScratchDir {
    rag_rat_base::test_scratch::ScratchDir::new("bench")
}

/// A Config indexing `subdir` of the corpus into a fresh temp DB, plus the guard that removes
/// it on drop. The guard is the LAST tuple field: tuple fields drop in declaration order, so the
/// Config — and any handle built from it — drops before the directory is removed.
pub fn bench_config(subdir: &str) -> (Config, rag_rat_base::test_scratch::ScratchDir) {
    let scratch = temp_db();
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: rag_rat_base::test_scratch::canonical_config_root(corpus_dir()),
        database: scratch.join("bench.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from(subdir)],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    (config, scratch)
}

/// Build a fresh index of `subdir` and return the open handle (the guard drops after it).
pub fn built_index(subdir: &str) -> (IndexDatabase, rag_rat_base::test_scratch::ScratchDir) {
    let (config, scratch) = bench_config(subdir);
    let db = IndexDatabase::rebuild(&config).expect("rebuild corpus index");
    (db, scratch)
}

/// Build a fresh index of `subdir` and return its `Config`, so a cold-open bench can reopen the DB
/// the way production `open_config` does — see [`open_like_production`]. (Returns the Config, not
/// just the path, because the realistic reopen needs `config.root` to resolve the active-checkout
/// git context.)
pub fn built_config(subdir: &str) -> (Config, rag_rat_base::test_scratch::ScratchDir) {
    let (config, scratch) = bench_config(subdir);
    IndexDatabase::rebuild(&config).expect("rebuild corpus index");
    (config, scratch)
}

/// Open an already-built index from disk the way the production `search` path does
/// (`IndexDatabase::open_config`), but deterministic + offline so it's fit for a bench: `open`
/// restores `source_root` from meta, `set_context` installs the active-checkout scope view that
/// `search` filters through, and the GitHub context is pinned offline (no `gh` shell-out). Without
/// the context, a bare `open` leaves the scope empty and `search` measures an unrealistically light
/// path (the old `query_cold` under-measure; this is the #80 fix).
pub fn open_like_production(config: &Config) -> IndexDatabase {
    let mut db = IndexDatabase::open(&config.database).expect("open index");
    let (commit_sha, worktree_id) = rag_rat_core::index::resolve_git_context(&config.root);
    db.set_context(&commit_sha, &worktree_id).expect("install active-checkout scope");
    db.set_papertrail_context(None);
    db
}
