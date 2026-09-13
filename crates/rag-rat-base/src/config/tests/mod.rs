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
/// One uniquely owned scratch directory, removed on drop — panics and early returns included.
/// Wraps [`crate::test_scratch::ScratchDir`] and derefs to `PathBuf` so fixtures use it exactly
/// like the bare temp path it replaces; the directory is created up front and fixtures nest
/// repos and worktree destinations under it. Bind it for the whole test — a temporary drops the
/// guard at the statement boundary and deletes the directory mid-test.
#[derive(Debug)]
struct ScratchRoot {
    path: PathBuf,
    _scratch: crate::test_scratch::ScratchDir,
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

/// A fresh scratch directory under the shared, self-healing test-scratch namespace (see
/// [`crate::test_scratch`]), keyed by `tag`, the process id, and a process-wide counter.
fn scratch(tag: &str) -> ScratchRoot {
    let guard = crate::test_scratch::ScratchDir::new(tag);
    ScratchRoot { path: guard.path().to_path_buf(), _scratch: guard }
}
