//! Shared fixtures for the `rag-rat` CLI integration tests.
//!
//! Every git fixture repo built here gets a DETERMINISTIC, wall-clock-independent commit identity,
//! so two fixtures built in the same test process can never share a root commit. rag-rat's portable
//! `repo_id` is the smallest root-commit hash; identical fixture content committed on the system
//! clock (1-second granularity, no `GIT_*_DATE`) used to produce byte-identical root commits →
//! the SAME `repo_id`, and two such fixtures landing in one consolidated/global DB collided (#434).
//!
//! The commit date is a fixed base epoch plus a per-commit counter — the SAME atomic that keys
//! temp-dir uniqueness — so fixture identities are both DISTINCT across fixtures and REPRODUCIBLE
//! across runs (stable hashes, independent of CI load). No sleeps, retries, or `--no-verify`.
//!
//! Not every integration binary uses every helper here; `#![allow(dead_code)]` keeps the shared
//! module warning-clean under `clippy --all-targets -D warnings`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// 2021-01-01T00:00:00Z — a fixed, wall-clock-independent anchor for fixture commit dates.
const FIXTURE_EPOCH_BASE: u64 = 1_609_459_200;

/// One process-wide counter drives BOTH temp-dir names and fixture commit dates, so every fixture
/// gets a unique path AND a unique, reproducible root-commit hash. nextest runs each test in its
/// own process, so the call order — and thus the assigned counters — is deterministic per test.
static SEQ: AtomicU64 = AtomicU64::new(0);

fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// One uniquely owned scratch directory, removed on drop — panics and early returns included.
///
/// Wraps [`rag_rat_base::test_scratch::ScratchDir`] (tag + PID + process-wide sequence naming, the
/// #726 stale sweep as backstop) and derefs to `PathBuf`, so call sites use it exactly like the
/// bare path this replaces. The path itself is left ABSENT (any stale dir removed, nothing
/// created): fixtures create it — or, for `git worktree add` destinations, keep it absent —
/// exactly as before. The guard is deliberately not cloneable: exactly one owner removes each
/// path. Bind it for the whole test (never `unique_dir(..).join(..)` in one expression, which
/// drops the guard at the statement boundary), and declare it before any DB connection, watcher,
/// or child process using the path so those drop first (required for cleanup on Windows).
#[derive(Debug)]
pub struct ScratchRoot {
    path: PathBuf,
    _scratch: rag_rat_base::test_scratch::ScratchDir,
}

impl std::ops::Deref for ScratchRoot {
    type Target = PathBuf;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<Path> for &ScratchRoot {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// A throwaway scratch dir under the shared, self-healing test-scratch root
/// (see [`rag_rat_base::test_scratch`]), keyed by `tag`, the process id, and a unique counter,
/// returned as its owning [`ScratchRoot`] guard. The path is ABSENT on return (any stale dir at
/// the path is removed first); the guard removes whatever the test leaves there on drop, and the
/// shared helper sweeps dirs stranded by a killed run so they can't accumulate in the system
/// temp (#586, #726).
pub fn unique_dir(tag: &str) -> ScratchRoot {
    let scratch = rag_rat_base::test_scratch::ScratchDir::new(tag);
    let path = scratch.path().to_path_buf();
    // Fixtures historically allocate an absent path, including Git worktree destinations.
    let _ = std::fs::remove_dir_all(&path);
    ScratchRoot { path, _scratch: scratch }
}

/// A filesystem path formatted for embedding inside a double-quoted TOML string value. On Windows a
/// raw `db.display()` yields `C:\Users\…`, whose `\U`/`\R`/… are invalid TOML escape sequences
/// (`too few unicode value digits` at parse). `to_slash_lossy` (path-slash) rewrites the SEPARATOR
/// to `/` on Windows — TOML-safe, and `Path` treats `/`≡`\` there so the parsed config still equals
/// the original `PathBuf` — while leaving a literal backslash in a Unix filename intact.
pub fn toml_path(path: &Path) -> String {
    use path_slash::PathExt;
    path.to_slash_lossy().into_owned()
}

/// Run `git -C dir args`, asserting success. Use [`git_commit`] for commits so the commit carries a
/// deterministic, unique date (a plain `git(dir, &["commit", ...])` would commit on the wall clock
/// and could collide on `repo_id`).
pub fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// `git commit` with `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` pinned to a fixed base epoch plus a
/// unique counter, so two fixtures with identical content never share a root-commit hash (and thus
/// never collide on the portable `repo_id`), while the hashes stay reproducible across runs. `args`
/// are the commit flags/message, e.g. `&["-q", "-m", "seed"]`.
pub fn git_commit(dir: &Path, args: &[&str]) {
    let date = format!("@{} +0000", FIXTURE_EPOCH_BASE + next_seq());
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("commit")
        .args(args)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git commit {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
