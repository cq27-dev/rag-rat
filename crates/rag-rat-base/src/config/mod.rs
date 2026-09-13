mod discovery;
mod error;
mod globs;
mod load;
mod raw;
mod types;

pub use discovery::{
    default_database_path, default_legacy_database_path, discover_config_path, git_common_dir,
    linked_worktree_main_root, nearest_config_at_or_above, worktree_root,
};
pub(crate) use discovery::{main_worktree_root, normalize_existing_dir, resolve_default_database};
pub use error::ConfigError;
#[cfg(test)]
pub(crate) use load::{anchor_root_to_main_worktree, resolve_targets};
pub(crate) use raw::{RawConfig, RawTarget, resolve_relative_cookbook_path};
#[cfg(test)]
pub(crate) use raw::{RawMemory, RawOracle, RawSearch, RawSync, RawVersionCheck, RawWatch};
pub use raw::{endpoint_authority_has_userinfo, valid_tracker_base_url, valid_tracker_project};
pub use types::{
    Config, DEFAULT_DISCOVERY_NODE, DEFAULT_QUERY_ENDPOINT, DEFAULT_SYNC_RELAY, DistillLlmConfig,
    DreamLlmConfig, EmbeddingBackend, EmbeddingConfig, EmbeddingRuntimeConfig, LlmConfig,
    LogConfig, LogFormat, LogLevel, MAX_REMOTE_EMBEDDING_CONCURRENCY, MemoryConfig, MemorySurface,
    OracleConfig, OracleLiveConfig, PapertrailConfig, RemoteBackend, RemoteDreamConfig,
    RemoteEmbeddingConfig, ResolvedTarget, SearchConfig, SyncConfig, TargetKind, Tracker,
    TrackerAuth, TrackerConfig, VersionCheckConfig, WatchConfig,
};

#[cfg(test)]
mod tests;
