//! `rag-rat init` Python dir-selection (#181): the scan honors the same gitignore + floor rules as
//! the index walk, so a GITIGNORED virtualenv at the root does not drop a real `manage.py`
//! entrypoint, while a NON-gitignored env-only repo produces no usable config and fails instead of
//! writing an empty `[target_bindings]`.
//!
//! These use `init -y --dry-run`, which renders the planned config to STDOUT and returns BEFORE
//! writing the file, indexing, or installing MCP servers/hooks — so the test never touches the
//! filesystem outside its temp repo, never mutates the developer's/CI's MCP config, and never
//! shells out to `claude`/`codex` (the real `init` install flow does all of those).

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_temp_root() -> PathBuf {
    let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rag-rat-init-dirsel-{}-{id}", std::process::id()))
}

fn write(root: &std::path::Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Run `init -y --dry-run` in `root`. Dry-run prints the rendered config and returns before any
/// side effect, so this is pure planning — no config write, no index, no MCP/hook install.
fn init_dry_run(root: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["init", "-y", "--dry-run"])
        .current_dir(root)
        .output()
        .expect("run rag-rat init --dry-run")
}

#[test]
fn gitignored_root_venv_does_not_drop_the_root_entrypoint() {
    let root = unique_temp_root();
    fs::create_dir_all(&root).unwrap();
    // A Django-style layout: a root `manage.py`, a real package, and a GITIGNORED virtualenv.
    write(&root, "manage.py", "def main():\n    pass\n");
    write(&root, "myapp/__init__.py", "");
    write(&root, "myapp/models.py", "class A:\n    pass\n");
    write(&root, "env/bin/activate_this.py", "x = 1\n");
    write(&root, ".gitignore", "env/\n");

    let output = init_dry_run(&root);
    assert!(output.status.success(), "init --dry-run should succeed: {output:?}");
    let rendered = String::from_utf8_lossy(&output.stdout);
    // The gitignored `env/` is skipped by the scan (as the index would skip it), so `.` is still a
    // safe default for the root entrypoint.
    assert!(
        rendered.contains("python = [\".\", \"myapp\"]"),
        "expected `.` + package binding, got:\n{rendered}"
    );
    assert!(!root.join("rag-rat.toml").exists(), "dry-run must not write a config");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn non_gitignored_env_only_repo_fails_instead_of_empty_config() {
    let root = unique_temp_root();
    fs::create_dir_all(&root).unwrap();
    // The only Python lives under a NON-gitignored, unfloored `env/` virtualenv — there is no safe
    // binding, so init must fail rather than emit an empty `[target_bindings]`.
    write(&root, "env/bin/activate_this.py", "x = 1\n");

    let output = init_dry_run(&root);
    assert!(!output.status.success(), "init should fail with no indexable source: {output:?}");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn first_party_virtualenv_package_is_bindable() {
    let root = unique_temp_root();
    fs::create_dir_all(&root).unwrap();
    // The `virtualenv` PyPI package's own layout: `src/virtualenv/` is FIRST-PARTY source (no
    // `pyvenv.cfg`). It must not be treated as a dependency tree — Python stays bound.
    write(&root, "src/virtualenv/__init__.py", "class Session:\n    pass\n");
    write(&root, "src/virtualenv/run.py", "def cli():\n    pass\n");

    let output = init_dry_run(&root);
    assert!(output.status.success(), "init should bind the first-party package: {output:?}");
    let rendered = String::from_utf8_lossy(&output.stdout);
    // Bound as ordinary source under the `src` layout — NOT dropped as a "virtualenv" dependency.
    assert!(
        rendered.contains("python = [\"src\"]"),
        "the virtualenv package must remain a Python binding, got:\n{rendered}"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn nested_real_venv_does_not_promote_its_ancestor() {
    let root = unique_temp_root();
    fs::create_dir_all(&root).unwrap();
    // A real venv (has `pyvenv.cfg`) nested under `tools/`, plus a genuine package. The venv's
    // files must not count, so `tools` is never promoted and `.` is blocked; only `myapp`
    // binds.
    write(&root, "myapp/__init__.py", "");
    write(&root, "myapp/models.py", "class A:\n    pass\n");
    write(&root, "manage.py", "def main():\n    pass\n");
    write(&root, "tools/env/pyvenv.cfg", "home = /usr\n");
    write(&root, "tools/env/bin/activate_this.py", "x = 1\n");
    write(&root, "tools/env/lib/site.py", "y = 2\n");

    let output = init_dry_run(&root);
    assert!(output.status.success(), "init should succeed binding the real package: {output:?}");
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("python = [\"myapp\"]"),
        "only the real package should bind (not `.` or `tools`), got:\n{rendered}"
    );

    fs::remove_dir_all(&root).ok();
}
