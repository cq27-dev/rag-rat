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

use std::process::Command;

use rag_rat_base::config::Config;

mod common;

fn unique_temp_root() -> common::ScratchRoot {
    common::unique_dir("init-yes")
}

/// Initialize a git repo at `dir` WITH a first commit: an identity-bearing root, so the keyless
/// config init writes resolves to the (sandboxed) GLOBAL store — an unborn HEAD is identity-less
/// and stays on the per-root legacy path (covered by the config-level tests). The commit goes
/// through [`common::git_commit`], so its root-commit hash is deterministically unique and two
/// fixtures never collide on `repo_id`.
fn git_init(dir: &std::path::Path) {
    common::git(dir, &["init", "-q"]);
    common::git(dir, &["config", "user.email", "t@e.com"]);
    common::git(dir, &["config", "user.name", "t"]);
    common::git(dir, &["add", "-A"]);
    common::git_commit(dir, &["-qm", "seed"]);
}

#[test]
fn init_yes_writes_a_config_that_config_load_accepts() {
    let root = unique_temp_root();
    let cache = root.join("model-cache");
    // The keyless config (A7) resolves the database through RAG_RAT_DATA_DIR — sandbox it to this
    // test's temp dir so init's index build can never touch the developer's real global store.
    let data_dir = root.join("data");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("Sources/App")).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    // Trivial Rust and SwiftPM source trees make the scan bind both default source directories (a
    // non-empty plan; an empty plan would make `init` bail by design — that path is covered in
    // `init_dir_selection.rs`).
    std::fs::write(root.join("src/lib.rs"), "pub fn alpha() -> u32 {\n    1\n}\n").unwrap();
    std::fs::write(root.join("Sources/App/App.swift"), "struct App {}\n").unwrap();
    // `init --yes` auto-accepts the git-maintenance-hook install (its pre-wizard behavior, which
    // this test pins as unchanged), and that install needs a real git repo — so make one. The
    // hooks themselves never fire here: `RAG_RAT_HOOK_DISABLE=1` short-circuits them.
    git_init(&root);

    let output = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["init", "--yes"])
        .current_dir(&root)
        // Keep the run hermetic: no git hooks firing, no background watcher, and the model
        // cache/HOME/data dir sandboxed to this test's temp dir so a model install or the index
        // build can't touch the real cache or the real global database (offline → hash fallback;
        // online → downloads into `cache`).
        .env("RAG_RAT_HOOK_DISABLE", "1")
        .env("RAG_RAT_NO_WATCH", "1")
        .env("RAG_RAT_MODEL_CACHE", &cache)
        .env("RAG_RAT_DATA_DIR", &data_dir)
        .env("HOME", &*root)
        .env("XDG_CACHE_HOME", &cache)
        // The production subprocess honors the ambient git config (it must — hook install is a
        // real user-facing path), so the harness isolates it to keep the test machine-independent
        // (#970): a hostile ambient `core.hooksPath` must not reach the fixture's hook install.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        // Routing vars are scrubbed too: production discovery honors GIT_DIR/GIT_WORK_TREE
        // (#213 bare-dir/hook contexts), which would re-point the fixture at an unrelated repo.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
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

    // A7: the written config is KEYLESS — no active `database` key — and init's own index build
    // (`setup_index` → migrate → discover) resolved through it to the sandboxed GLOBAL store, not
    // to a per-repo `.rag-rat/index.sqlite`.
    let written = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        !written.lines().any(|line| line.trim_start().starts_with("database")),
        "init must write a keyless config (global database by default):\n{written}"
    );
    assert!(
        data_dir.join("rag-rat.sqlite").exists(),
        "init's index build must land in the RAG_RAT_DATA_DIR global store"
    );
    assert!(
        !root.join(".rag-rat/index.sqlite").exists(),
        "no legacy per-repo index may be created by a fresh init"
    );

    // The written config round-trips through the real loader, and reflects the `default_plan`
    // rendering the `--yes` path has always used (active `rust = ["src"]` binding + the documented
    // oracle/version surface).
    let config = Config::load(&config_path).expect("Config::load must accept the written config");
    assert!(
        config.targets.iter().any(|t| t.language == rag_rat_base::language::Language::Rust),
        "the rust source tree must bind a rust target, got: {:?}",
        config.targets
    );
    assert!(
        config.targets.iter().any(|target| {
            target.language == rag_rat_base::language::Language::Swift
                && target.directories == [std::path::PathBuf::from("Sources")]
                && target.include == ["**/*.swift"]
        }),
        "the SwiftPM source tree must bind a Swift target with default globs, got: {:?}",
        config.targets
    );
}

/// `init` in a NEW repo must not heal/mutate an EXISTING repo's meta in the shared global DB
/// (Codex batch 6 — the second unregistered-subject migrate caller, after consolidate):
/// `setup_index` runs its migration config-less BEFORE the new repo is registered, so a healing
/// migrate's witness would resolve the sole registered repo — the SIBLING — and a heal-owed pass
/// would clear its model meta. `setup_index` is schema-only; the config-bearing index open right
/// after registers the new repo and heals correctly scoped.
#[test]
fn init_in_a_new_repo_leaves_an_existing_repos_heal_owed_meta() {
    let base = unique_temp_root();
    let cache = base.join("model-cache");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&cache).unwrap();
    let global = data_dir.join("rag-rat.sqlite");

    let make_repo = |name: &str, file: &str| {
        let dir = base.join(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), format!("pub fn {file}() {{}}\n")).unwrap();
        git_init(&dir);
        dir
    };
    let run = |dir: &std::path::Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rag-rat"))
            .args(args)
            .current_dir(dir)
            .env("RAG_RAT_HOOK_DISABLE", "1")
            .env("RAG_RAT_NO_WATCH", "1")
            .env("RAG_RAT_MODEL_CACHE", &cache)
            .env("RAG_RAT_DATA_DIR", &data_dir)
            .env("HOME", &*base)
            .env("XDG_CACHE_HOME", &cache)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_CONFIG")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .unwrap()
    };

    // Repo 1 populates the global DB (keyless config → global store).
    let repo1 = make_repo("first", "first_anchor");
    std::fs::write(
        repo1.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    let out = run(&repo1, &["index", "--full"]);
    assert!(out.status.success(), "repo1 index: {}", String::from_utf8_lossy(&out.stderr));

    // Repo 1 now OWES a heal: its active-model meta names a legacy id.
    let repo1_id: String = {
        let conn = rusqlite::Connection::open(&global).unwrap();
        let id: String = conn
            .query_row(
                "SELECT repo_id FROM repos WHERE repo_id != '__unassigned__' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO repo_meta(repo_id, key, value) VALUES (?1, \
             'active_embedding_model', 'fastembed-all-minilm-l6-v2')",
            [&id],
        )
        .unwrap();
        id
    };

    // A brand-new repo runs `init -y` against the SAME global store.
    let repo2 = make_repo("second", "second_anchor");
    let out = run(&repo2, &["init", "--yes"]);
    assert!(out.status.success(), "init -y: {}", String::from_utf8_lossy(&out.stderr));

    // Repo 1's heal-owed meta is untouched — init's pre-registration migration never healed it.
    let conn = rusqlite::Connection::open(&global).unwrap();
    let survived: Option<String> = conn
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = ?1 AND key = 'active_embedding_model'",
            [&repo1_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        survived.as_deref(),
        Some("fastembed-all-minilm-l6-v2"),
        "init of a new repo healed (cleared) the existing repo's model meta",
    );
}

/// Init from a SUBDIRECTORY of the normal MAIN worktree proceeds (Codex batch 8, finding 1): the
/// linked-ness predicate is topology-derived (this checkout's workdir vs the designated main), so
/// a main-worktree subdir is main — the old path-equality check falsely refused here. Uses
/// `--dry-run` so nothing is written; the point is that the refusal does NOT fire.
#[test]
fn init_proceeds_from_a_subdirectory_of_the_main_worktree() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("tools/sub/src")).unwrap();
    std::fs::write(root.join("tools/sub/src/lib.rs"), "pub fn alpha() {}\n").unwrap();
    git_init(&root);

    let output = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["init", "--yes", "--dry-run"])
        .current_dir(root.join("tools/sub"))
        .env("RAG_RAT_HOOK_DISABLE", "1")
        .env("RAG_RAT_NO_WATCH", "1")
        .env("RAG_RAT_DATA_DIR", root.join("data"))
        .env("HOME", &*root)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "init from a main-worktree SUBDIR is not a linked worktree — it must proceed: {stderr}"
    );
    assert!(
        !stderr.contains("linked git worktree"),
        "no linked-worktree refusal from a main subdir: {stderr}"
    );
}

/// Init REFUSES to run in a LINKED git worktree (batch-7 worktree audit): the governing seam in
/// `Config::load` resolves every worktree through the MAIN worktree's `rag-rat.toml`, so a config
/// authored in a linked checkout would be ignored the moment main gains one. The refusal points
/// at the main worktree instead of writing a file that doesn't do what it says.
#[test]
fn init_refuses_to_run_in_a_linked_worktree() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();
    git_init(&root);

    let linked = common::unique_dir("init-yes-linked");
    rag_rat_base::test_git::run(&root, &[
        "worktree",
        "add",
        "--detach",
        "-q",
        linked.to_str().unwrap(),
    ]);

    let output = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["init", "-y"])
        .current_dir(&linked)
        .output()
        .unwrap();
    assert!(!output.status.success(), "init in a linked worktree is refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("main worktree"), "the refusal points at the main worktree: {stderr}");
    let root_canonical = rag_rat_base::paths::canonicalize(&root).unwrap();
    assert!(
        stderr.contains(&root_canonical.display().to_string()),
        "the refusal names the main worktree path: {stderr}"
    );
    assert!(!linked.join("rag-rat.toml").exists(), "no branch-local config was written");
}

/// Go module-root coverage, non-interactive (`complete-default-bindings-coverage` task 10,
/// BIND-01/BIND-02): a real on-disk fixture with a root `go.mod`, no root-level `.go` file, and
/// more than 32 leaf `pkgN/file.go` packages must, through the ACTUAL `init --yes` CLI path (not
/// just the unit-level `default_plan`), collapse to a single Go target bound at the module root
/// (`.`) rather than emitting 40 individual per-package bindings.
#[test]
fn init_yes_binds_a_large_go_module_at_its_root() {
    let root = unique_temp_root();
    let cache = root.join("model-cache");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&cache).unwrap();

    // Root go.mod, no root-level .go file, >32 package dirs: module root must absorb every leaf
    // package via manifest-root promotion + dedup_ancestors (BIND-01), mirroring the unit-level
    // fixture in `init/scan.rs::go_module_root_absorbs_more_than_32_leaf_packages`.
    std::fs::write(root.join("go.mod"), "module example.com/big\n\ngo 1.22\n").unwrap();
    for i in 0..40 {
        let dir = root.join(format!("pkg{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.go"), "package pkg\n").unwrap();
    }
    git_init(&root);

    let output = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["init", "--yes"])
        .current_dir(&root)
        .env("RAG_RAT_HOOK_DISABLE", "1")
        .env("RAG_RAT_NO_WATCH", "1")
        .env("RAG_RAT_MODEL_CACHE", &cache)
        .env("RAG_RAT_DATA_DIR", &data_dir)
        .env("HOME", &*root)
        .env("XDG_CACHE_HOME", &cache)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .output()
        .expect("run rag-rat init --yes");

    assert!(
        output.status.success(),
        "init --yes must succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config_path = root.join("rag-rat.toml");
    assert!(config_path.exists(), "init --yes must write rag-rat.toml");

    let written = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        written.contains("go = [\".\"]"),
        "the [target_bindings] table must bind go at the module root only, got:\n{written}"
    );

    let config = Config::load(&config_path).expect("Config::load must accept the written config");
    let go_targets: Vec<_> = config
        .targets
        .iter()
        .filter(|t| t.language == rag_rat_base::language::Language::Go)
        .collect();
    assert_eq!(
        go_targets.len(),
        1,
        "a large Go module must collapse to a single target, got: {go_targets:?}"
    );
    assert_eq!(
        go_targets[0].directories,
        [std::path::PathBuf::from(".")],
        "the sole Go target must bind at the module root, got: {go_targets:?}"
    );
}

/// Language-neutral >32-default regression, non-interactive (`complete-default-bindings-coverage`
/// task 10, BIND-02): unlike the Go fixture above, Rust has no manifest-root promotion, so 40
/// sibling `pkgN/src` directories never collapse to a single `.` binding — they stay 40 distinct
/// defaults. This exercises the raw `candidate_dirs` 32-entry cap directly: before the fix, the
/// bindings written into `rag-rat.toml` were sourced from that same capped list, so packages 33-40
/// were silently dropped from the index with no warning. `resolved_bindings`/`default_dirs` must
/// now emit all 40 through the ACTUAL `init --yes` CLI path.
#[test]
fn init_yes_does_not_truncate_more_than_32_rust_defaults() {
    let root = unique_temp_root();
    let cache = root.join("model-cache");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&cache).unwrap();

    let mut expected_dirs = Vec::new();
    for i in 0..40 {
        let dir = root.join(format!("pkg{i}")).join("src");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.rs"), "pub fn f() {}\n").unwrap();
        expected_dirs.push(std::path::PathBuf::from(format!("pkg{i}/src")));
    }
    expected_dirs.sort();
    git_init(&root);

    let output = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["init", "--yes"])
        .current_dir(&root)
        .env("RAG_RAT_HOOK_DISABLE", "1")
        .env("RAG_RAT_NO_WATCH", "1")
        .env("RAG_RAT_MODEL_CACHE", &cache)
        .env("RAG_RAT_DATA_DIR", &data_dir)
        .env("HOME", &*root)
        .env("XDG_CACHE_HOME", &cache)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .output()
        .expect("run rag-rat init --yes");

    assert!(
        output.status.success(),
        "init --yes must succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config_path = root.join("rag-rat.toml");
    assert!(config_path.exists(), "init --yes must write rag-rat.toml");

    let config = Config::load(&config_path).expect("Config::load must accept the written config");
    let mut rust_dirs: Vec<_> = config
        .targets
        .iter()
        .filter(|t| t.language == rag_rat_base::language::Language::Rust)
        .flat_map(|t| t.directories.clone())
        .collect();
    rust_dirs.sort();

    assert_eq!(
        rust_dirs, expected_dirs,
        "all 40 rust package dirs must be bound, none silently dropped by the 32-entry UI cap"
    );
}
