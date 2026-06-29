//! `rag-rat init --yes` regression guard (Task 19): wiring the ratatui wizard into the INTERACTIVE
//! branch of `init::run` must leave the non-interactive (`--yes`) path byte-for-byte unchanged. It
//! still renders from `default_plan` + `render_config`, writes the file, and produces a
//! `rag-rat.toml` that `Config::load` accepts.
//!
//! This drives the real `init -y` write path (NOT `--dry-run`), so the config is genuinely written
//! to disk and then re-parsed. The model install/reconcile runs downstream of the write; it is
//! sandboxed to a per-test cache (an offline run falls back to the dependency-free `hash` embedder,
//! an online run resolves from cache) so the test never leaks into the developer's model cache and
//! never depends on a specific model being present.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use rag_rat_core::Config;

static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_temp_root() -> PathBuf {
    let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rag-rat-init-yes-{}-{id}", std::process::id()))
}

/// Initialize a quiet git repo at `dir` (identity configured so any commit would succeed).
fn git_init(dir: &std::path::Path) {
    for args in
        [&["init", "-q"][..], &["config", "user.email", "t@e.com"], &["config", "user.name", "t"]]
    {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

#[test]
fn init_yes_writes_a_config_that_config_load_accepts() {
    let root = unique_temp_root();
    let cache = root.join("model-cache");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    // A trivial Rust source tree so the scan binds `rust = ["src"]` (a non-empty plan; an empty
    // plan would make `init` bail by design — that path is covered in `init_dir_selection.rs`).
    std::fs::write(root.join("src/lib.rs"), "pub fn alpha() -> u32 {\n    1\n}\n").unwrap();
    // `init --yes` auto-accepts the git-maintenance-hook install (its pre-wizard behavior, which
    // this test pins as unchanged), and that install needs a real git repo — so make one. The
    // hooks themselves never fire here: `RAG_RAT_HOOK_DISABLE=1` short-circuits them.
    git_init(&root);

    let output = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["init", "--yes"])
        .current_dir(&root)
        // Keep the run hermetic: no git hooks firing, no background watcher, and the model
        // cache/HOME sandboxed to this test's temp dir so a model install can't touch the real
        // cache (offline → hash fallback; online → downloads into `cache`).
        .env("RAG_RAT_HOOK_DISABLE", "1")
        .env("RAG_RAT_NO_WATCH", "1")
        .env("RAG_RAT_MODEL_CACHE", &cache)
        .env("HOME", &root)
        .env("XDG_CACHE_HOME", &cache)
        .output()
        .expect("run rag-rat init --yes");

    assert!(
        output.status.success(),
        "init --yes must succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The non-interactive path WROTE the config (it is not `--dry-run`).
    let config_path = root.join("rag-rat.toml");
    assert!(config_path.exists(), "init --yes must write rag-rat.toml");

    // The written config round-trips through the real loader, and reflects the `default_plan`
    // rendering the `--yes` path has always used (active `rust = ["src"]` binding + the documented
    // oracle/version surface).
    let config = Config::load(&config_path).expect("Config::load must accept the written config");
    assert!(
        config.targets.iter().any(|t| t.language == rag_rat_core::language::Language::Rust),
        "the rust source tree must bind a rust target, got: {:?}",
        config.targets
    );

    std::fs::remove_dir_all(&root).ok();
}
