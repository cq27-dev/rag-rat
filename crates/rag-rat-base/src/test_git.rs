//! Shared, environment-isolated `git` invocation for the workspace's test fixtures (#970).
//!
//! Test fixtures shell out to `git` to build throwaway repos. A helper that runs with the AMBIENT
//! environment inherits whatever git configuration the developer or CI runner happens to have —
//! `core.hooksPath` (a hostile global hook fails every fixture commit), `commit.gpgsign`,
//! `init.defaultBranch` (fixture branch names drift between machines), `core.autocrlf` (GitHub's
//! windows-latest runner rewrites LF fixtures as CRLF, changing content sha256), `user.*`, and
//! templates. Tests then pass or fail depending on the machine rather than the code (#581).
//!
//! [`command`] pins the entire class at one seam, so a new fixture cannot reintroduce it:
//!   * `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` are `/dev/null` — no user/system gitconfig leaks
//!     in at all (hooks, signing, aliases, templates);
//!   * `HOME` is pinned to the fixture directory, so nothing (credential helpers, global ignore)
//!     resolves to the developer's real home;
//!   * the author/committer identity is pinned (fixtures must not depend on `git config user.*`
//!     calls, and commit identity stays deterministic across machines);
//!   * command-scope config (`GIT_CONFIG_COUNT`) pins `init.defaultBranch = main` and
//!     `core.autocrlf = false`, so a plain `git init` and every checkout are byte-identical
//!     everywhere. Command scope deliberately outranks repo-local config.
//!
//! Not `#[cfg(test)]`: that gate does not propagate across crates, and fixtures in sibling crates
//! (`rag-rat-core`, `rag-rat-cli`, …) consume this helper. It is not part of the semver-stable
//! API surface. Matches the [`crate::test_scratch`] convention. Production code that must honor
//! the ambient environment (config discovery, eval replay) does NOT belong here.

use std::path::Path;
use std::process::{Command, Output};

/// A `git` invocation in `dir` with the full fixture isolation applied (see module docs). Callers
/// needing non-standard output handling or extra environment (e.g. pinned commit dates) chain
/// onto the returned [`Command`].
pub fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        // Repository-routing variables are removed outright: an exported `GIT_DIR` /
        // `GIT_WORK_TREE` / `GIT_INDEX_FILE` (a worktree shell, an IDE, a hook context) beats
        // `current_dir` and would operate the fixture on an UNRELATED repository (#219's exact
        // hijack shape), and `GIT_CONFIG` / `GIT_CONFIG_PARAMETERS` are inline-config vectors
        // that could smuggle a hostile `hooksPath` past the `/dev/null` files below.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_AUTHOR_NAME", "rag-rat-test")
        .env("GIT_AUTHOR_EMAIL", "rag-rat-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "rag-rat-test")
        .env("GIT_COMMITTER_EMAIL", "rag-rat-test@example.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("HOME", dir)
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "init.defaultBranch")
        .env("GIT_CONFIG_VALUE_0", "main")
        .env("GIT_CONFIG_KEY_1", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_1", "false");
    cmd
}

/// Run `git` to completion, panicking with the captured stderr on failure.
pub fn run(dir: &Path, args: &[&str]) -> Output {
    let out = command(dir, args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    out
}

/// Run `git` to completion, returning its trimmed stdout (panicking with stderr on failure).
pub fn output(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned environment: a fixture's branch name and config view never drift with the
    /// developer's/CI's ambient git config (#970).
    #[test]
    fn fixtures_get_the_pinned_environment() {
        let root = crate::test_scratch::ScratchDir::new("git-isolation");
        run(&root, &["init", "-q"]);
        // `init.defaultBranch` is pinned, so a plain `git init` lands on `main` everywhere.
        assert_eq!(output(&root, &["branch", "--show-current"]), "main");
        std::fs::write(root.join("f.txt"), "x\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "seed"]);
        // No hooks configuration is visible to fixture git, so a hostile global `core.hooksPath`
        // cannot fire inside a fixture.
        let hooks = command(&root, &["config", "core.hooksPath"]).output().unwrap();
        assert!(!hooks.status.success(), "no core.hooksPath is visible to fixture git");
    }

    /// Guard the guard: the ambient failure this isolates against (#581's repro) really happens
    /// WITHOUT the helper, so the isolation can never silently degrade to a no-op.
    #[test]
    fn the_ambient_failure_mode_is_real() {
        let root = crate::test_scratch::ScratchDir::new("git-isolation-repro");
        let hooks = root.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let pre_commit = hooks.join("pre-commit");
        std::fs::write(&pre_commit, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&pre_commit, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            root.join("gitconfig"),
            format!("[core]\nhooksPath = {}\n", hooks.display()),
        )
        .unwrap();
        run(&root, &["init", "-q"]);
        std::fs::write(root.join("f.txt"), "x\n").unwrap();
        run(&root, &["add", "."]);
        let out = Command::new("git")
            .current_dir(root.path())
            .args(["commit", "-q", "-m", "seed"])
            .env("GIT_CONFIG_GLOBAL", root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // The repro pins the hostile GLOBAL file, but ambient command-scope config
            // (`GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS`/`GIT_CONFIG`) outranks it and could
            // silence — or mis-trigger — the hook; routing vars get the same scrub so the repro
            // always measures exactly the pinned file.
            .env_remove("GIT_CONFIG")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(!out.status.success(), "the hostile global hook must fire without isolation");
    }
}
