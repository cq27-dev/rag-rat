use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use path_slash::PathExt;

use super::{
    self as config, Config, ConfigError, DEFAULT_DISCOVERY_NODE, DEFAULT_SYNC_RELAY,
    DistillLlmConfig, EmbeddingRuntimeConfig, LlmConfig, LogConfig, LogFormat, LogLevel,
    MemoryConfig, MemorySurface, OracleConfig, OracleLiveConfig, RawConfig, RawMemory, RawOracle,
    RawSearch, RawSync, RawTarget, RawVersionCheck, RawWatch, RemoteBackend, RemoteDreamConfig,
    RemoteEmbeddingConfig, ResolvedTarget, SearchConfig, SyncConfig, TargetKind, TrackerAuth,
    VersionCheckConfig, WatchConfig,
};
use crate::language::Language;

mod governance;
mod llm;
mod sections;
mod targets;
mod trackers;

/// A fresh scratch directory under the shared, self-healing test-scratch namespace (see
/// [`crate::test_scratch`]), keyed by `tag`, the process id, and a process-wide counter — removed
/// on drop, panics and early returns included. Fixtures nest repos and worktree destinations under
/// it. Bind it for the whole test: a temporary drops the guard at the statement boundary and
/// deletes the directory mid-test.
fn scratch(tag: &str) -> crate::test_scratch::ScratchDir {
    crate::test_scratch::ScratchDir::new(tag)
}

/// The smallest config that loads: the root itself, with `src` bound as Rust.
const MINIMAL_CONFIG: &str = r#"[index]
root = "."
[target_bindings]
rust = ["src"]
"#;

/// A config naming only its root: loads, with no targets.
const ROOT_ONLY_CONFIG: &str = r#"[index]
root = "."
"#;

/// Write `body` as `dir`'s `rag-rat.toml`.
fn write_config(dir: &Path, body: &str) {
    std::fs::write(dir.join("rag-rat.toml"), body).unwrap();
}

/// Run an isolated fixture `git` in `dir` (see [`crate::test_git`]), panicking on failure.
fn git(dir: &Path, args: &[&str]) {
    crate::test_git::run(dir, args);
}

/// Seed a minimal COMMITTED git repo at `dir` — the identity-bearing fixture the global default
/// requires (a keyless config resolves globally only for a root with a derivable repo identity).
fn git_commit_all(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "seed"]);
}
