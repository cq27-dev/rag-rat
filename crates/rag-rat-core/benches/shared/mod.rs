//! Shared corpus + config helpers for the rag-rat benches. Not a bench target itself (it lives in
//! a subdirectory, so cargo does not auto-detect it).

#![allow(dead_code)] // each bench uses a subset of these helpers

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fs};

use rag_rat_core::config::{ResolvedTarget, TargetKind};
use rag_rat_core::language::Language;
use rag_rat_core::{Config, IndexDatabase};

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

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn temp_db_path() -> PathBuf {
    let n = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!("rag-rat-bench-{}-{n}.sqlite", std::process::id()))
}

/// A Config indexing `subdir` of the corpus into a fresh temp DB.
pub fn bench_config(subdir: &str) -> Config {
    Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: corpus_dir(),
        database: temp_db_path(),
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
    }
}

/// Build a fresh index of `subdir` and return the open handle.
pub fn built_index(subdir: &str) -> IndexDatabase {
    IndexDatabase::rebuild(&bench_config(subdir)).expect("rebuild corpus index")
}

/// Build a fresh index of `subdir` and return its `Config`, so a cold-open bench can reopen the DB
/// the way production `open_config` does — see [`open_like_production`]. (Returns the Config, not
/// just the path, because the realistic reopen needs `config.root` to resolve the active-checkout
/// git context.)
pub fn built_config(subdir: &str) -> Config {
    let config = bench_config(subdir);
    IndexDatabase::rebuild(&config).expect("rebuild corpus index");
    config
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
    db.set_papertrail_context(None, false);
    db
}
