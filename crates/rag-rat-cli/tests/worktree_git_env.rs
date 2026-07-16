//! Regression (#219): an inherited `GIT_DIR` / `GIT_WORK_TREE` (e.g. a tool operating in a linked
//! worktree — Claude Code, an IDE) must NOT hijack the index's git resolution.
//!
//! With the env honored unconditionally, `index --worktree` resolved BOTH the base repo and the
//! linked repo to the single env-specified worktree (ignoring their path arguments), so the
//! base↔linked overlay delta collapsed to empty and the real overlay rows were pruned/tombstoned —
//! the file-reported worktree flip-flop. `discover_repo` now resolves from the configured path
//! first, falling back to env overrides only when no repo is found upward. This test runs the CLI
//! in a subprocess (env-isolated) with the hijacking env set and asserts the overlay survives.

use std::fs;
use std::path::Path;
use std::process::Command;

use rag_rat_base::config::Config;
use rag_rat_base::language::Language;
use rag_rat_core::IndexDatabase;

mod common;

use common::{git, git_commit, unique_dir};

#[test]
fn worktree_overlay_ignores_inherited_git_dir_env() {
    let base = unique_dir("wtenv");
    let main = base.join("main");
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/keep.rs"), "pub fn keep_fn() {}\n").unwrap();
    fs::write(main.join("src/reinf.rs"), "pub fn classify_seg() {}\n").unwrap();
    // Nest the worktree under config.root and gitignore it (the perverse-but-real held layout).
    fs::write(main.join(".gitignore"), "/wt/\n").unwrap();
    fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e.com"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "."]);
    git_commit(&main, &["-q", "-m", "C1 has reinf"]);

    // Linked worktree forked at C1 (HAS reinf.rs), nested + gitignored.
    let wt = main.join("wt");
    git(&main, &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()]);

    // Main REMOVES reinf.rs at C2; the branch keeps it — so reinf.rs lives ONLY in the overlay.
    fs::remove_file(main.join("src/reinf.rs")).unwrap();
    git(&main, &["add", "."]);
    git_commit(&main, &["-q", "-m", "C2 removed reinf"]);

    let config_path = main.join("rag-rat.toml");
    let config = Config::load(&config_path).unwrap();
    IndexDatabase::rebuild(&config).unwrap();

    // Run `index --worktree` with the WORKTREE's GIT_DIR/GIT_WORK_TREE in the env — the exact shape
    // a worktree shell exports, which used to hijack resolution and prune the overlay.
    let git_dir = main.join(".git/worktrees/feat");
    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let out = Command::new(binary)
        .arg("--config")
        .arg(&config_path)
        .args(["index", "--worktree"])
        .arg(&wt)
        .env("RAG_RAT_NO_WATCH", "1")
        .env("GIT_DIR", &git_dir)
        .env("GIT_WORK_TREE", &wt)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "index --worktree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The overlay must serve reinf.rs READABLE in the worktree scope — the inherited GIT_DIR must
    // not have collapsed the delta and pruned/tombstoned the row.
    let mut db = IndexDatabase::open(&config.database).unwrap();
    db.use_worktree_scope(&main, Some(&wt)).unwrap();
    let hits = db.symbols("classify_seg", Some(Language::Rust), 10).unwrap();
    assert!(
        !hits.is_empty(),
        "an inherited worktree GIT_DIR hijacked resolution and pruned the overlay (reinf.rs \
         missing)"
    );

    let _ = fs::remove_dir_all(&base);
}

/// The DISCOVERY side of the governing seam (Codex batch 9): a linked worktree with no
/// branch-local `rag-rat.toml` — exactly the state `init`'s linked-worktree refusal leaves — must
/// find the MAIN worktree's config through default discovery instead of dying at the existence
/// check. And in a truly config-less repo, the missing-config hint names the path where the
/// config BELONGS: main's `rag-rat.toml`, not the linked checkout's.
#[test]
fn default_config_discovery_reaches_the_main_worktree_from_a_linked_checkout() {
    let base = unique_dir("wtdisc");
    let main = base.join("main");
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/lib.rs"), "pub fn discovery_anchor() {}\n").unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e.com"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "."]);
    git_commit(&main, &["-q", "-m", "seed"]);
    let linked = base.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

    // Truly config-less: the hint from the LINKED checkout names MAIN's path (where init runs).
    let run = |dir: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rag-rat"))
            .current_dir(dir)
            .args(args)
            .env("RAG_RAT_HOOK_DISABLE", "1")
            .env("RAG_RAT_NO_WATCH", "1")
            .env("RAG_RAT_DATA_DIR", base.join("data"))
            .env("RAG_RAT_MODEL_CACHE", base.join("cache"))
            .output()
            .unwrap()
    };
    let out = run(&linked, &["gc"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let main_toml = main.canonicalize().unwrap().join("rag-rat.toml");
    assert!(
        stderr.contains(&main_toml.display().to_string()),
        "the config-less hint names MAIN's config path from a linked checkout: {stderr}"
    );

    // The config exists in MAIN only (uncommitted — the linked checkout has NO rag-rat.toml,
    // the post-refusal state). Every default-config command from the linked checkout works.
    fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    assert!(!linked.join("rag-rat.toml").exists(), "the linked checkout has no branch config");
    let out = run(&linked, &["index", "--full"]);
    assert!(
        out.status.success(),
        "index from the linked checkout resolves main's config: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        main.join(".rag-rat/index.sqlite").exists(),
        "the index landed at MAIN's configured database"
    );
    assert!(!linked.join(".rag-rat").exists(), "nothing resolved against the linked checkout");
    let out = run(&linked, &["memory", "list"]);
    assert!(
        out.status.success(),
        "a read command from the linked checkout works too: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = fs::remove_dir_all(&base);
}
