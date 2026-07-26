use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use path_slash::{PathBufExt, PathExt};
use tempfile::TempDir;

/// #427: `index` with no `[target_bindings]` must FAIL (non-zero exit) and name the section,
/// rather than silently registering an empty repo.
#[test]
fn index_refuses_zero_discovered_files() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
    // Config with a root + database but NO [target_bindings].
    let db = root.join("index.sqlite");
    let toml = format!(
        "[index]\nroot = \".\"\ndatabase = \"{}\"\n[llm.embedding]\nmodel = \"none\"\n",
        db.to_slash_lossy()
    );
    let config_path = root.join("rag-rat.toml");
    std::fs::write(&config_path, toml).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--config", config_path.to_str().unwrap(), "index"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!out.status.success(), "index should refuse a zero-file config");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("target_bindings"), "stderr should name target_bindings: {stderr}");
    assert!(!db.exists() || is_empty_db_absent(&db), "must not register an empty repo");
}

/// With `--allow-empty`, the same config succeeds (empty index).
#[test]
fn index_allow_empty_permits_zero_discovered_files() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let db = root.join("index.sqlite");
    let toml = format!(
        "[index]\nroot = \".\"\ndatabase = \"{}\"\n[llm.embedding]\nmodel = \"none\"\n",
        db.to_slash_lossy()
    );
    let config_path = root.join("rag-rat.toml");
    std::fs::write(&config_path, toml).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--config", config_path.to_str().unwrap(), "index", "--allow-empty"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "--allow-empty should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// #427: pointing `[index] root` at a fresh clone of an already-indexed repo (same identity, shared
/// database) must WARN that it merges into the existing scope and name the `[index] repo_id` remedy
/// — the same-identity-join hint, exercised through the real CLI so the warning wiring is covered.
#[test]
fn index_warns_when_a_clone_joins_an_indexed_repo() {
    if !git_available() {
        return; // identity resolution needs git; skip rather than fail.
    }
    let dir = TempDir::new().unwrap();
    let tmp = dir.path();
    let shared = tmp.join("shared.sqlite");

    // Repo A: a committed git repo, indexed into the shared database.
    let a = tmp.join("A");
    std::fs::create_dir_all(a.join("src")).unwrap();
    git_init_commit(&a);
    std::fs::write(a.join("rag-rat.toml"), shared_config(&shared)).unwrap();
    let out = run_index(&a.join("rag-rat.toml"), &a);
    assert!(out.status.success(), "indexing A should succeed: {}", stderr(&out));

    // Clone B (same root commit → same portable identity) pointed at the SAME database.
    let b = tmp.join("B");
    rag_rat_base::test_git::run(&a, &["clone", "-q", a.to_str().unwrap(), b.to_str().unwrap()]);
    std::fs::write(b.join("rag-rat.toml"), shared_config(&shared)).unwrap();

    let out = run_index(&b.join("rag-rat.toml"), &b);
    let stderr = stderr(&out);
    assert!(out.status.success(), "indexing the clone should still succeed: {stderr}");
    assert!(
        stderr.contains("shares the identity") && stderr.contains("repo_id"),
        "clone should warn it joins the existing scope and name the repo_id remedy: {stderr}"
    );
}

/// #427: when `[index] root` names a linked worktree, `Config::load` re-anchors it to the main
/// checkout and `index` must WARN that it is indexing the main tree, naming `--worktree` — the
/// re-anchor hint, exercised through the real CLI.
#[test]
fn index_warns_when_root_names_a_linked_worktree() {
    if !git_available() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let tmp = dir.path();
    let shared = tmp.join("shared.sqlite");

    // Main checkout with its own config (so the governing seam anchors root to main).
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    git_init_commit(&main);
    std::fs::write(main.join("rag-rat.toml"), shared_config(&shared)).unwrap();

    // A linked worktree with a local config whose root="." resolves to the worktree.
    let wt = tmp.join("wt");
    let added = rag_rat_base::test_git::command(&main, &[
        "worktree",
        "add",
        "--detach",
        "-q",
        wt.to_str().unwrap(),
    ])
    .status()
    .unwrap();
    assert!(added.success(), "git worktree add failed");
    std::fs::create_dir_all(wt.join("src")).unwrap();
    std::fs::write(wt.join("rag-rat.toml"), shared_config(&shared)).unwrap();

    // Run from inside the worktree so config discovery finds its local config, then re-anchors.
    let out = run_index(&wt.join("rag-rat.toml"), &wt);
    let stderr = stderr(&out);
    assert!(out.status.success(), "indexing from a worktree should succeed: {stderr}");
    assert!(
        stderr.contains("linked worktree") && stderr.contains("--worktree"),
        "a linked-worktree root should warn it indexes the main checkout: {stderr}"
    );
}

/// #427 review: an ALREADY-indexed repo whose last file is deleted must still index (pruning the
/// stale rows), NOT be refused by the empty-index guard — that refusal is only for first-time empty
/// registrations.
#[test]
fn index_allows_an_already_indexed_repo_to_go_empty() {
    if !git_available() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let tmp = dir.path();
    let db = tmp.join("index.sqlite");
    let repo = tmp.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git_init_commit(&repo);
    std::fs::write(repo.join("rag-rat.toml"), shared_config(&db)).unwrap();

    // First index registers the repo with one file.
    let out = run_index(&repo.join("rag-rat.toml"), &repo);
    assert!(out.status.success(), "first index should succeed: {}", stderr(&out));

    // Delete the only indexed file, then re-discover: the guard must let it through so the pass
    // prunes to empty instead of stranding the deleted file's row.
    std::fs::remove_file(repo.join("src/a.rs")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--config", repo.join("rag-rat.toml").to_str().unwrap(), "index", "--discover"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let err = stderr(&out);
    assert!(out.status.success(), "an existing index going empty must not be refused: {err}");
    assert!(
        !err.contains("no `[target_bindings]`"),
        "must not print the first-time-empty refusal for an existing index: {err}"
    );
    assert!(err.contains("discovered 0 files"), "should have run the discover/prune pass: {err}");
}

/// #427 review: `index --full` on an EXISTING index whose files were all deleted must MIGRATE +
/// PRUNE it, not refuse it as first-time-empty. The rebuild guard checks AFTER `create_or_migrate`
/// for an existing DB (so a pre-registry/older index becomes readable first) and recognizes it via
/// the persisted `source_root` — here a not-yet-adopted placeholder index (indexed while non-git).
#[test]
fn full_rebuild_prunes_an_existing_placeholder_index_instead_of_refusing() {
    if !git_available() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let repo = dir.path();
    let db = repo.join("index.sqlite");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(repo.join("rag-rat.toml"), shared_config(&db)).unwrap();

    // Index while NON-git → adopts the `__unassigned__` placeholder (a legacy-style,
    // not-yet-adopted index) and persists source_root under it.
    let out = run_index(&repo.join("rag-rat.toml"), repo);
    assert!(out.status.success(), "initial index should succeed: {}", stderr(&out));

    // Give the root a git identity the DB has NOT adopted, then delete all target files.
    let git = |args: &[&str]| {
        rag_rat_base::test_git::run(repo, args);
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    std::fs::remove_file(repo.join("src/a.rs")).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--config", repo.join("rag-rat.toml").to_str().unwrap(), "index", "--full"])
        .current_dir(repo)
        .output()
        .unwrap();
    let err = stderr(&out);
    assert!(
        out.status.success(),
        "an existing index going empty must be pruned by --full, not refused: {err}"
    );
    assert!(
        !err.contains("no `[target_bindings]`"),
        "must not print the first-time-empty refusal for an existing index: {err}"
    );
}

/// #427 root-cause: the empty refusal must fire on the INCREMENTAL path too, not just the fresh
/// rebuild. When the DB already exists (repo A indexed into a shared DB), a first-time-empty repo B
/// pointed at it takes the incremental/discover path, whose guard must refuse BEFORE adopting B —
/// otherwise adoption records B's root and a later check would wave the empty registration through.
#[test]
fn index_discover_refuses_a_first_time_empty_repo_against_an_existing_db() {
    if !git_available() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let tmp = dir.path();
    let db = tmp.join("shared.sqlite");

    // Repo A: real content, indexed into the shared DB — so the DB EXISTS and is non-empty, which
    // is what routes B below through the incremental path instead of the fresh-DB rebuild.
    let a = tmp.join("A");
    std::fs::create_dir_all(a.join("src")).unwrap();
    git_init_commit(&a);
    std::fs::write(a.join("rag-rat.toml"), shared_config(&db)).unwrap();
    let out = run_index(&a.join("rag-rat.toml"), &a);
    assert!(out.status.success(), "indexing A should succeed: {}", stderr(&out));

    // Repo B: a DISTINCT git repo (own first-commit content → own identity) pointed at A's SAME DB,
    // but with NO [target_bindings]. `index --discover` here hits the incremental empty-index
    // guard.
    let b = tmp.join("B");
    std::fs::create_dir_all(b.join("src")).unwrap();
    std::fs::write(b.join("src/b.rs"), "fn b() {}\n").unwrap();
    let git_b = |args: &[&str]| {
        rag_rat_base::test_git::run(&b, args);
    };
    git_b(&["init", "-q"]);
    git_b(&["config", "user.email", "t@example.com"]);
    git_b(&["config", "user.name", "t"]);
    git_b(&["add", "-A"]);
    git_b(&["commit", "-qm", "b"]);
    let no_targets = format!(
        "[index]\nroot = \".\"\ndatabase = \"{}\"\n[llm.embedding]\nmodel = \"none\"\n",
        db.to_slash_lossy()
    );
    std::fs::write(b.join("rag-rat.toml"), &no_targets).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--config", b.join("rag-rat.toml").to_str().unwrap(), "index", "--discover"])
        .current_dir(&b)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a first-time-empty repo on an existing DB must be refused on the incremental path"
    );
    let err = stderr(&out);
    assert!(err.contains("target_bindings"), "should name target_bindings: {err}");
}

/// #427 review: `index --watch` on a config with NO `[target_bindings]` can never discover
/// anything, so it must FAIL FAST at startup (naming the section) rather than sit as a silent,
/// unrecoverable watcher that watches nothing and never re-reads config. Contrast with
/// `index_watch_with_configured_targets_but_no_files_defers`.
#[test]
fn index_watch_on_a_no_target_config_errors() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let db = root.join("index.sqlite");
    // No [target_bindings], fresh database.
    let toml = format!(
        "[index]\nroot = \".\"\ndatabase = \"{}\"\n[llm.embedding]\nmodel = \"none\"\n",
        db.to_slash_lossy()
    );
    let config_path = root.join("rag-rat.toml");
    std::fs::write(&config_path, toml).unwrap();

    // `env_remove` so the failure is the no-target refusal, not the "watcher disabled" error.
    let out = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--config", config_path.to_str().unwrap(), "index", "--watch"])
        .current_dir(root)
        .env_remove("RAG_RAT_NO_WATCH")
        .output()
        .unwrap();
    assert!(!out.status.success(), "no-target index --watch should fail fast, not park");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("target_bindings"), "stderr should name target_bindings: {stderr}");
    assert!(!db.exists() || is_empty_db_absent(&db), "must not register an empty index");
}

/// #427 review: `index --watch` on a config WITH `[target_bindings]` that currently match zero
/// files starts and DEFERS — the watcher keeps running (a file added under a watched target dir
/// will register + index it) and registers nothing until then. Bounded: sleep briefly, confirm it
/// is still watching and registered nothing, then kill it.
#[test]
fn index_watch_with_configured_targets_but_no_files_defers() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let db = root.join("index.sqlite");
    // [target_bindings] present, but the target dir currently holds no matching files.
    std::fs::create_dir_all(root.join("src")).unwrap();
    let toml = format!(
        "[index]\nroot = \".\"\ndatabase = \"{}\"\n[llm.embedding]\nmodel = \
         \"none\"\n[target_bindings]\nrust = [\"src\"]\n",
        db.to_slash_lossy()
    );
    let config_path = root.join("rag-rat.toml");
    std::fs::write(&config_path, toml).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--config", config_path.to_str().unwrap(), "index", "--watch"])
        .current_dir(root)
        .env_remove("RAG_RAT_NO_WATCH")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Give the watcher time to start and run its first (deferring) maintenance pass.
    std::thread::sleep(Duration::from_secs(3));
    let still_watching = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        still_watching,
        "index --watch with configured-but-empty targets should keep watching, not exit/refuse"
    );
    assert!(!db.exists() || is_empty_db_absent(&db), "watch must not register an empty index");
}

/// #427 review: the git-hook `rag-rat maintenance` pass must NOT first-time-register an empty index
/// either. It defers (creates no database, no error) until real content appears — the same policy
/// as the watcher, so a post-commit/checkout hook on a misconfigured repo doesn't silently register
/// an empty scope.
#[test]
fn maintenance_defers_a_first_time_empty_config() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let db = root.join("index.sqlite");
    // No [target_bindings] → no discoverable files.
    let toml = format!(
        "[index]\nroot = \".\"\ndatabase = \"{}\"\n[llm.embedding]\nmodel = \"none\"\n",
        db.to_slash_lossy()
    );
    let config_path = root.join("rag-rat.toml");
    std::fs::write(&config_path, toml).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--config", config_path.to_str().unwrap(), "maintenance"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "maintenance must not error on an empty config: {}",
        stderr(&out)
    );
    assert!(!db.exists(), "maintenance must not register an empty first-time index");
}

/// #427 review: a target with an EMPTY directory list (`[target_bindings] rust = []`) leaves
/// `config.targets` non-empty but `target_directories()` empty — the watcher would place no source
/// watches and sit stuck. `index --watch` must refuse it at startup, same as a no-target config.
#[test]
fn index_watch_on_empty_target_dirs_errors() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let db = root.join("index.sqlite");
    // A binding present but with no directories → non-empty targets, zero watchable dirs.
    let toml = format!(
        "[index]\nroot = \".\"\ndatabase = \"{}\"\n[llm.embedding]\nmodel = \
         \"none\"\n[target_bindings]\nrust = []\n",
        db.to_slash_lossy()
    );
    let config_path = root.join("rag-rat.toml");
    std::fs::write(&config_path, toml).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--config", config_path.to_str().unwrap(), "index", "--watch"])
        .current_dir(root)
        .env_remove("RAG_RAT_NO_WATCH")
        .output()
        .unwrap();
    assert!(!out.status.success(), "empty target dirs should fail fast, not park");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("target_bindings"), "stderr should name target_bindings: {stderr}");
    assert!(!db.exists() || is_empty_db_absent(&db), "must not register an empty index");
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn git_init_commit(dir: &Path) {
    let git = |args: &[&str]| {
        rag_rat_base::test_git::run(dir, args);
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
}

fn shared_config(db: &Path) -> String {
    format!(
        "[index]\nroot = \".\"\ndatabase = \"{}\"\n[llm.embedding]\nmodel = \
         \"none\"\n[target_bindings]\nrust = [\"src\"]\n",
        db.to_slash_lossy()
    )
}

fn run_index(config_path: &Path, cwd: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--config", config_path.to_str().unwrap(), "index"])
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// The refuse path must not leave a registered repo behind. A never-created DB file, or a DB with
// no `repos` rows, both satisfy "nothing registered".
fn is_empty_db_absent(db: &std::path::Path) -> bool {
    let Ok(conn) = rusqlite::Connection::open(db) else { return true };
    let n: i64 = conn
        .query_row("SELECT count(*) FROM repos WHERE repo_id <> '__unassigned__'", [], |r| r.get(0))
        .unwrap_or(0);
    n == 0
}
