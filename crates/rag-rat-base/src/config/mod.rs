use std::path::PathBuf;

use thiserror::Error;

use crate::language::LanguageError;

mod discovery;
mod load;
mod raw;
mod types;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    Language(#[from] LanguageError),
    #[error("unknown target kind `{0}`")]
    UnknownTargetKind(String),
    #[error(
        "`database` points at the consolidated global store, but this root has no resolvable repo \
         identity (not a committed git repo). Add `[index] repo_id = \"...\"` to pin an identity, \
         or point `database` at a per-repo file (e.g. `.rag-rat/index.sqlite`)"
    )]
    GlobalPinWithoutIdentity,
    #[error(
        "unknown embedding model `{0}` (expected a registered model id — the HF path, e.g. \
         `sentence-transformers/all-MiniLM-L6-v2`, `BAAI/bge-small-en-v1.5`, \
         `jinaai/jina-embeddings-v2-base-code`, `minishlab/potion-retrieval-32M` — or `none`)"
    )]
    UnknownEmbeddingBackend(String),
    #[error(
        "[llm.embedding.remote] requires a non-empty `model` (the Ollama API model name, such as \
         `all-minilm`)"
    )]
    RemoteEmbeddingMissingModel,
    #[error("{section} `backend` must be one of `ollama`, `infinity`, or `vllm` (got `{got}`)")]
    RemoteBackendUnknown { section: &'static str, got: String },
    #[error(
        "[llm.embedding.remote] `backend = \"{backend}\"` with an ephemeral `cookbook` requires \
         an explicit `query_endpoint` — queries embed against a LOCAL server after the box is \
         torn down, and the default (a local Ollama URL) only fits `backend = \"ollama\"`. Point \
         `query_endpoint` at a local `{backend}` server serving the same model"
    )]
    RemoteQueryEndpointRequiredForBackend { backend: &'static str },
    #[error(
        "[llm.embedding.remote] requires EXACTLY ONE of `endpoint` (connect to a running server) \
         or `cookbook` (provision an ephemeral box) — set neither both nor zero"
    )]
    RemoteEmbeddingModeAmbiguous,
    #[error(
        "[llm.embedding.remote] `endpoint` must not embed credentials in the URL (no \
         `user:pass@host`) — the endpoint is persisted into the index; put any token in an env \
         var and name it via `auth_env` instead"
    )]
    RemoteEmbeddingEndpointHasCredentials,
    #[error(
        "[llm.embedding.remote] `gpu` applies only to ephemeral `cookbook` provisioning, not a \
         connect `endpoint` — remove `gpu`, or switch the remote block to a `cookbook` recipe"
    )]
    RemoteGpuRequiresCookbook,
    #[error(
        "{section} `gpu` is set but empty — give it a provider-specific value (a Modal GPU class \
         like `A10G`, or a RunPod `gpuTypeId`), or remove the key to use the recipe default"
    )]
    RemoteGpuEmpty { section: &'static str },
    #[error(
        "[llm.embedding.remote] `num_ctx` must be greater than zero when set — remove it to use \
         Ollama's default context, or set a positive context window such as 4096"
    )]
    RemoteEmbeddingInvalidNumCtx,
    #[error(
        "[llm.embedding.remote] `concurrency` is {value}, but the maximum supported value is {max}"
    )]
    RemoteEmbeddingConcurrencyTooHigh { value: u32, max: u32 },
    #[error(
        "[llm.embedding.remote] can only serve a transformer model over Ollama, but `model = \
         \"{0}\"` is not a transformer (it is a static/hash model, or `none`/disabled) — remove \
         the remote block, or select a transformer model (e.g. \
         `sentence-transformers/all-MiniLM-L6-v2`, `BAAI/bge-small-en-v1.5`, \
         `jinaai/jina-embeddings-v2-base-code`)"
    )]
    RemoteEmbeddingNonTransformerModel(String),
    #[error(
        "{section} requires a non-empty `model` (the server-side chat model name — an ollama \
         model like `qwen3:8b`, or the HuggingFace id vLLM was launched with, e.g. \
         `Qwen/Qwen3-4B-Instruct-2507`)"
    )]
    DreamRemoteMissingModel { section: &'static str },
    #[error(
        "{section} `backend = \"{backend}\"` cannot serve chat completions — a chat model needs a \
         chat-capable backend (`ollama` or `vllm`); `infinity` is embed-only. Switch the backend, \
         or point it at an ollama/vLLM server"
    )]
    DreamBackendCannotServeChat { section: &'static str, backend: String },
    #[error(
        "{section} requires EXACTLY ONE of `endpoint` (connect to a running chat server) or \
         `cookbook` (provision an ephemeral box) — set neither both nor zero"
    )]
    DreamRemoteModeAmbiguous { section: &'static str },
    #[error(
        "{section} `endpoint` must not embed credentials in the URL (no `user:pass@host`) — put \
         any token in an env var and name it via `auth_env` instead"
    )]
    DreamRemoteEndpointHasCredentials { section: &'static str },
    #[error(
        "{section} `gpu` applies only to ephemeral `cookbook` provisioning, not a connect \
         `endpoint` — remove `gpu`, or switch the remote block to a `cookbook` recipe"
    )]
    DreamRemoteGpuRequiresCookbook { section: &'static str },
    #[error(
        "{section} `provision_timeout_s = {got}` is below the `{backend}` backend's {floor}s \
         provisioning floor — the override may only LENGTHEN the boot budget (e.g. a large model \
         whose weight pull exceeds the default), never shorten it below what a cold start needs"
    )]
    DreamRemoteProvisionTimeoutBelowFloor {
        section: &'static str,
        backend: &'static str,
        got: u64,
        floor: u64,
    },
    #[error(
        "the `[local_ai]` table was renamed to `[llm]` (#317). Update your rag-rat.toml: rename \
         `[local_ai.embedding]` → `[llm.embedding]` (and any `[local_ai.embedding.remote]` / \
         `[local_ai.embedding.runtime]` → `[llm.embedding.remote]` / `[llm.embedding.runtime]`)"
    )]
    LocalAiTableRenamed,
    #[error(
        "the dream model config moved from `[dream.model]` to `[llm.dream]` / \
         `[llm.dream.remote]`. Update your rag-rat.toml: rename `[dream.model] enabled = true` → \
         `[llm.dream] enabled = true`, and put the server config (endpoint/model, or a \
         cookbook/backend/gpu for a remote GPU) under `[llm.dream.remote]`"
    )]
    DreamTableMoved,
    #[error("duplicate target name `{0}`")]
    DuplicateTarget(String),
    #[error("configured directory does not exist: {0}")]
    MissingDirectory(PathBuf),
    #[error("[log] `level` must be one of off|error|warn|info|debug|trace (got `{0}`)")]
    UnknownLogLevel(String),
    #[error("[log] `format` must be `text` or `json` (got `{0}`)")]
    UnknownLogFormat(String),
    #[error("[memory] `surface` must be `full` or `summary` (got `{0}`)")]
    UnknownMemorySurface(String),
    #[error(
        "each [[tracker]] binding requires a `provider` (`github`, `gitlab`, `bitbucket`, or \
         `jira`)"
    )]
    TrackerProviderMissing,
    #[error(
        "[[tracker]] `provider` must be one of `github`, `gitlab`, `bitbucket`, or `jira` (got \
         `{0}`)"
    )]
    UnknownTrackerProvider(String),
    #[error("[[tracker]] `provider = \"jira\"` requires an explicit `project`")]
    JiraTrackerRequiresProject,
    #[error("[[tracker]] `project = \"{project}\"` is not valid for provider `{provider}`")]
    InvalidTrackerProject { provider: &'static str, project: String },
    #[error("[[tracker]] `auth` requires exactly one of `env` or `token_command`")]
    TrackerAuthExactlyOne,
    #[error("[[tracker]] `base_url` must be an http(s) URL (got `{0}`)")]
    TrackerBaseUrlNotHttp(String),
    #[error("[[tracker]] `base_url` must not embed credentials in the URL")]
    TrackerBaseUrlHasCredentials,
    #[error(
        "[papertrail] `rate_limit_reserve` must be a finite fraction from 0.0 up to (but not \
         including) 1.0 (got `{0}`)"
    )]
    PapertrailRateLimitReserveOutOfRange(f64),
    #[error(
        "[papertrail] `{0}` must be positive — a zero cadence would silently disable automatic \
         sync"
    )]
    PapertrailIntervalZero(&'static str),
}

pub use discovery::{
    default_database_path, default_legacy_database_path, discover_config_path,
    linked_worktree_main_root, nearest_config_at_or_above, worktree_root,
};
pub(crate) use discovery::{main_worktree_root, normalize_existing_dir, resolve_default_database};
#[cfg(test)]
pub(crate) use load::{anchor_root_to_main_worktree, resolve_targets};
pub use raw::endpoint_authority_has_userinfo;
pub(crate) use raw::{RawConfig, RawTarget, resolve_relative_cookbook_path};
#[cfg(test)]
pub(crate) use raw::{RawMemory, RawOracle, RawSearch, RawVersionCheck, RawWatch};
pub use types::{
    Config, DEFAULT_QUERY_ENDPOINT, DistillLlmConfig, DreamLlmConfig, EmbeddingBackend,
    EmbeddingConfig, EmbeddingRuntimeConfig, LlmConfig, LogConfig, LogFormat, LogLevel,
    MAX_REMOTE_EMBEDDING_CONCURRENCY, MemoryConfig, MemorySurface, OracleConfig, PapertrailConfig,
    RemoteBackend, RemoteDreamConfig, RemoteEmbeddingConfig, ResolvedTarget, SearchConfig,
    TargetKind, Tracker, TrackerAuth, TrackerConfig, VersionCheckConfig, WatchConfig,
};

#[cfg(test)]
mod tests;
