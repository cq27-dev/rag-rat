use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{
    ConfigError, DEFAULT_QUERY_ENDPOINT, DreamLlmConfig, EmbeddingBackend, EmbeddingConfig,
    EmbeddingRuntimeConfig, LlmConfig, LogConfig, LogFormat, LogLevel,
    MAX_REMOTE_EMBEDDING_CONCURRENCY, MemoryConfig, MemorySurface, OracleConfig, PapertrailConfig,
    RemoteBackend, RemoteDreamConfig, RemoteEmbeddingConfig, SearchConfig, Tracker, TrackerAuth,
    TrackerConfig, VersionCheckConfig, WatchConfig,
};
use crate::embedding_models::Backend;

#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct RawConfig {
    #[serde(default)]
    pub(crate) index: RawIndex,
    #[serde(default)]
    pub(crate) llm: RawLlm,
    /// Presence-capture for the OLD `[local_ai]` table (renamed to `[llm]` in #317). Serde would
    /// otherwise SILENTLY DROP this now-unknown table, loading every embedding setting as a
    /// default (re-enabling FastEmbed, dropping a configured remote/runtime) on upgrade. We
    /// capture it so `load` can reject it loudly with a migration instruction instead of
    /// misconfiguring silently.
    #[serde(default)]
    pub(crate) local_ai: Option<toml::Value>,
    /// Presence-capture for the OLD top-level `[dream]` table (the dream model config moved to
    /// `[llm.dream]` / `[llm.dream.remote]`). Serde would otherwise SILENTLY DROP it, so an
    /// upgrade from `[dream.model] enabled = true` would load `[llm.dream] enabled = false`
    /// and run the deterministic passes only, never the model. Captured so `load` rejects it
    /// loudly with a migration instruction instead of silently downgrading.
    #[serde(default)]
    pub(crate) dream: Option<toml::Value>,
    #[serde(default)]
    pub(crate) watch: RawWatch,
    #[serde(default)]
    pub(crate) log: RawLog,
    #[serde(default)]
    pub(crate) version_check: RawVersionCheck,
    #[serde(default)]
    pub(crate) oracle: RawOracle,
    #[serde(default)]
    pub(crate) search: RawSearch,
    #[serde(default)]
    pub(crate) memory: RawMemory,
    #[serde(default, rename = "tracker")]
    pub(crate) tracker: Vec<RawTracker>,
    /// Papertrail transport and automatic synchronization settings.
    #[serde(default)]
    pub(crate) papertrail: Option<RawPapertrail>,
    #[serde(default)]
    pub(crate) target_bindings: BTreeMap<String, Vec<String>>,
    #[serde(default, rename = "target")]
    pub(crate) target: Vec<RawTarget>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPapertrail {
    pub(crate) probe_interval_secs: Option<u64>,
    pub(crate) sync_min_interval_secs: Option<u64>,
    pub(crate) full_sync_interval_secs: Option<u64>,
    pub(crate) rate_limit_reserve: Option<f64>,
}

impl TryFrom<RawPapertrail> for PapertrailConfig {
    type Error = ConfigError;

    fn try_from(raw: RawPapertrail) -> Result<Self, Self::Error> {
        let default = PapertrailConfig::default();
        let probe_interval_secs = raw.probe_interval_secs.unwrap_or(default.probe_interval_secs);
        let sync_min_interval_secs =
            raw.sync_min_interval_secs.unwrap_or(default.sync_min_interval_secs);
        let full_sync_interval_secs =
            raw.full_sync_interval_secs.unwrap_or(default.full_sync_interval_secs);
        let rate_limit_reserve = raw.rate_limit_reserve.unwrap_or(default.rate_limit_reserve);
        if !rate_limit_reserve.is_finite() || !(0.0..1.0).contains(&rate_limit_reserve) {
            return Err(ConfigError::PapertrailRateLimitReserveOutOfRange(rate_limit_reserve));
        }
        Ok(PapertrailConfig {
            probe_interval_secs,
            sync_min_interval_secs,
            full_sync_interval_secs,
            rate_limit_reserve,
        })
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawTracker {
    provider: Option<String>,
    project: Option<String>,
    remote: Option<String>,
    base_url: Option<String>,
    auth: Option<RawTrackerAuth>,
    tags: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
struct RawTrackerAuth {
    env: Option<String>,
    token_command: Option<String>,
}

impl TryFrom<RawTracker> for TrackerConfig {
    type Error = ConfigError;
    fn try_from(raw: RawTracker) -> Result<Self, Self::Error> {
        let trimmed = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let provider = match trimmed(raw.provider) {
            None => return Err(ConfigError::TrackerProviderMissing),
            Some(p) => Tracker::parse_config(&p).ok_or(ConfigError::UnknownTrackerProvider(p))?,
        };
        let project = trimmed(raw.project);
        if provider == Tracker::Jira && project.is_none() {
            return Err(ConfigError::JiraTrackerRequiresProject);
        }
        if let Some(project) = project.as_deref()
            && !valid_tracker_project(provider, project)
        {
            return Err(ConfigError::InvalidTrackerProject {
                provider: provider.as_db_str(),
                project: project.to_string(),
            });
        }
        let base_url = match trimmed(raw.base_url) {
            None => None,
            Some(url) => {
                let Some((scheme, rest)) = url.split_once("://") else {
                    return Err(ConfigError::TrackerBaseUrlNotHttp(url));
                };
                let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
                let suffix = &rest[authority.len()..];
                if !matches!(scheme, "http" | "https")
                    || authority.is_empty()
                    || authority.starts_with(':')
                    || authority.chars().any(char::is_whitespace)
                    || !suffix.chars().all(|ch| ch == '/')
                {
                    return Err(ConfigError::TrackerBaseUrlNotHttp(url));
                }
                if endpoint_authority_has_userinfo(&url) {
                    return Err(ConfigError::TrackerBaseUrlHasCredentials);
                }
                Some(url.trim_end_matches('/').to_string())
            },
        };
        let auth = raw.auth.map(TrackerAuth::try_from).transpose()?;
        let tags = raw
            .tags
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        Ok(Self {
            provider,
            project,
            remote: trimmed(raw.remote).unwrap_or_else(|| "origin".to_string()),
            base_url,
            auth,
            tags,
        })
    }
}

fn valid_tracker_project(provider: Tracker, project: &str) -> bool {
    let parts = project.split('/').collect::<Vec<_>>();
    let valid_code_host_segment = |part: &&str| {
        !part.is_empty()
            && !matches!(*part, "." | "..")
            && part.chars().all(|ch| {
                !ch.is_control() && !ch.is_whitespace() && !matches!(ch, '?' | '#' | '%' | '\\')
            })
    };
    match provider {
        Tracker::Github | Tracker::Bitbucket =>
            parts.len() == 2 && parts.iter().all(valid_code_host_segment),
        Tracker::Gitlab => parts.len() >= 2 && parts.iter().all(valid_code_host_segment),
        Tracker::Jira =>
            project.chars().count() >= 2
                && project.chars().next().is_some_and(|first| first.is_ascii_uppercase())
                && project.chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit()),
    }
}

impl TryFrom<RawTrackerAuth> for TrackerAuth {
    type Error = ConfigError;
    fn try_from(raw: RawTrackerAuth) -> Result<Self, Self::Error> {
        let trimmed = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        match (trimmed(raw.env), trimmed(raw.token_command)) {
            (Some(v), None) => Ok(Self::Env(v)),
            (None, Some(v)) => Ok(Self::TokenCommand(v)),
            _ => Err(ConfigError::TrackerAuthExactlyOne),
        }
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawWatch {
    enabled: Option<bool>,
    debounce_ms: Option<u64>,
    max_latency_ms: Option<u64>,
    periodic_sweep_secs: Option<u64>,
}

impl From<RawWatch> for WatchConfig {
    fn from(raw: RawWatch) -> Self {
        let default = WatchConfig::default();
        Self {
            enabled: raw.enabled.unwrap_or(default.enabled),
            debounce_ms: raw.debounce_ms.unwrap_or(default.debounce_ms),
            max_latency_ms: raw.max_latency_ms.unwrap_or(default.max_latency_ms),
            periodic_sweep_secs: raw.periodic_sweep_secs.unwrap_or(default.periodic_sweep_secs),
        }
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawLog {
    enabled: Option<bool>,
    level: Option<String>,
    filter: Option<String>,
    dir: Option<String>,
    format: Option<String>,
    max_file_bytes: Option<u64>,
    retention_days: Option<u64>,
    max_files: Option<u64>,
}

impl TryFrom<RawLog> for LogConfig {
    type Error = ConfigError;

    fn try_from(raw: RawLog) -> Result<Self, Self::Error> {
        let d = LogConfig::default();
        let level = match raw.level {
            Some(s) => LogLevel::parse(&s).ok_or(ConfigError::UnknownLogLevel(s))?,
            None => d.level,
        };
        let format = match raw.format {
            Some(s) => LogFormat::parse(&s).ok_or(ConfigError::UnknownLogFormat(s))?,
            None => d.format,
        };
        Ok(Self {
            enabled: raw.enabled.unwrap_or(d.enabled),
            level,
            filter: raw.filter.filter(|s| !s.trim().is_empty()),
            // `dir` is finalized in `load()` (needs the db path); raw value passed through as-is.
            dir: raw.dir.map(PathBuf::from).unwrap_or_default(),
            format,
            max_file_bytes: raw.max_file_bytes.unwrap_or(d.max_file_bytes),
            retention_days: raw.retention_days.unwrap_or(d.retention_days),
            max_files: raw.max_files.unwrap_or(d.max_files),
        })
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawVersionCheck {
    enabled: Option<bool>,
}

impl From<RawVersionCheck> for VersionCheckConfig {
    fn from(raw: RawVersionCheck) -> Self {
        Self { enabled: raw.enabled.unwrap_or(VersionCheckConfig::default().enabled) }
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawOracle {
    auto_run: Option<bool>,
    auto_run_quiet_period_secs: Option<u64>,
    auto_run_min_interval_secs: Option<u64>,
}

impl From<RawOracle> for OracleConfig {
    fn from(raw: RawOracle) -> Self {
        let default = OracleConfig::default();
        Self {
            auto_run: raw.auto_run.unwrap_or(default.auto_run),
            auto_run_quiet_period_secs: raw
                .auto_run_quiet_period_secs
                .unwrap_or(default.auto_run_quiet_period_secs),
            auto_run_min_interval_secs: raw
                .auto_run_min_interval_secs
                .unwrap_or(default.auto_run_min_interval_secs),
        }
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawSearch {
    graded_git_rerank: Option<bool>,
}

impl From<RawSearch> for SearchConfig {
    fn from(raw: RawSearch) -> Self {
        Self {
            graded_git_rerank: raw
                .graded_git_rerank
                .unwrap_or(SearchConfig::default().graded_git_rerank),
        }
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawMemory {
    surface: Option<String>,
}

impl TryFrom<RawMemory> for MemoryConfig {
    type Error = ConfigError;

    fn try_from(raw: RawMemory) -> Result<Self, Self::Error> {
        let surface = match raw.surface {
            Some(s) => MemorySurface::parse(&s).ok_or(ConfigError::UnknownMemorySurface(s))?,
            None => MemorySurface::default(),
        };
        Ok(Self { surface })
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawIndex {
    pub(crate) root: Option<String>,
    pub(crate) database: Option<String>,
    /// `[index] repo_id` — pins the repo's identity for the consolidated global store instead of
    /// deriving it from the root-commit hash. Set it for a fork that must NOT share memories with
    /// its upstream, or a repo with no commits yet. Parsed here; consumed by
    /// `resolve_repo_identity` in a later workstream (no effect on path resolution).
    pub(crate) repo_id: Option<String>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawLlm {
    #[serde(default)]
    embedding: RawEmbedding,
    #[serde(default)]
    pub(crate) dream: RawDreamLlm,
}

impl TryFrom<RawLlm> for LlmConfig {
    type Error = ConfigError;

    fn try_from(raw: RawLlm) -> Result<Self, Self::Error> {
        Ok(Self {
            embedding: EmbeddingConfig::try_from(raw.embedding)?,
            dream: DreamLlmConfig::try_from(raw.dream)?,
        })
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawDreamLlm {
    enabled: Option<bool>,
    /// `[llm.dream.remote]` — absent → [`RemoteDreamConfig::default`] (a local-Ollama connect).
    /// Unlike embeddings there is no in-process fallback, so a missing block still yields a
    /// serving config rather than `None`.
    remote: Option<RawRemoteDream>,
}

impl TryFrom<RawDreamLlm> for DreamLlmConfig {
    type Error = ConfigError;

    fn try_from(raw: RawDreamLlm) -> Result<Self, Self::Error> {
        let remote = match raw.remote {
            Some(remote) => RemoteDreamConfig::try_from(remote)?,
            None => RemoteDreamConfig::default(),
        };
        Ok(Self { enabled: raw.enabled.unwrap_or_default(), remote })
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawRemoteDream {
    backend: Option<String>,
    endpoint: Option<String>,
    cookbook: Option<String>,
    model: Option<String>,
    gpu: Option<String>,
    auth_env: Option<String>,
    request_timeout_s: Option<u64>,
}

impl TryFrom<RawRemoteDream> for RemoteDreamConfig {
    type Error = ConfigError;

    fn try_from(raw: RawRemoteDream) -> Result<Self, Self::Error> {
        let default = RemoteDreamConfig::default();
        let trimmed = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        // The SERVER-side chat model name — required, non-empty. For ollama the ollama model name;
        // for vLLM the HF id the server was launched with.
        let model = raw.model.unwrap_or_default();
        let model = model.trim();
        if model.is_empty() {
            return Err(ConfigError::DreamRemoteMissingModel);
        }
        // Which OpenAI-compatible server serves this block. Omitted → `ollama`.
        let backend = match trimmed(raw.backend) {
            None => RemoteBackend::default(),
            Some(raw_backend) => RemoteBackend::from_db_str(&raw_backend)
                .ok_or(ConfigError::RemoteBackendUnknown(raw_backend))?,
        };
        // Dream requests the CHAT capability; `infinity` is embed-only. Reject it at parse time so
        // a misrouted backend never reaches the verdict client.
        if !backend.supports_chat() {
            return Err(ConfigError::DreamBackendCannotServeChat(backend.as_db_str().to_string()));
        }
        // The MODE is INFERRED from which URL field is set (mirrors embeddings): EXACTLY ONE of
        // `endpoint` (connect) / `cookbook` (ephemeral). Both → ambiguous; neither → no server.
        let endpoint = trimmed(raw.endpoint);
        let cookbook = trimmed(raw.cookbook);
        match (endpoint.is_some(), cookbook.is_some()) {
            (true, true) | (false, false) => {
                return Err(ConfigError::DreamRemoteModeAmbiguous);
            },
            _ => {},
        }
        // SECRET HYGIENE: reject a `user:pass@host` endpoint and direct the user to `auth_env`
        // instead. Checked against the URL authority only, so an `@` in a path/query is fine.
        // `cookbook` is a recipe spec, not a URL, so it is not checked.
        if let Some(url) = endpoint.as_deref()
            && endpoint_authority_has_userinfo(url)
        {
            return Err(ConfigError::DreamRemoteEndpointHasCredentials);
        }
        // EPHEMERAL-only: the GPU to provision. A PRESENT-but-empty value is a config error
        // (clearer than silently dropping a meant-to-be-set key). Set with a connect
        // `endpoint` it is meaningless → rejected. The VALUE is provider-specific and
        // validated at provision time.
        let gpu = match raw.gpu {
            Some(g) => {
                let g = g.trim();
                if g.is_empty() {
                    return Err(ConfigError::RemoteGpuEmpty);
                }
                if endpoint.is_some() {
                    return Err(ConfigError::DreamRemoteGpuRequiresCookbook);
                }
                Some(g.to_string())
            },
            None => None,
        };
        // Optional — local Ollama needs no auth; trim if present.
        let auth_env = trimmed(raw.auth_env);
        Ok(Self {
            backend,
            endpoint,
            cookbook,
            model: model.to_string(),
            gpu,
            auth_env,
            request_timeout_s: raw.request_timeout_s.unwrap_or(default.request_timeout_s),
        })
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawEmbedding {
    /// `model = "<model_id>" | "none"` — the embedding model selector, the registry model_id (the
    /// HF path, e.g. `sentence-transformers/all-MiniLM-L6-v2`; no aliases, #317).
    model: Option<String>,
    #[serde(default)]
    runtime: RawEmbeddingRuntime,
    /// `[llm.embedding.remote]` — absent → no remote offload (`remote: None`).
    remote: Option<RawRemoteEmbedding>,
}

impl TryFrom<RawEmbedding> for EmbeddingConfig {
    type Error = ConfigError;

    fn try_from(raw: RawEmbedding) -> Result<Self, Self::Error> {
        let backend = match raw.model.as_deref() {
            Some(value) => value.parse()?,
            None => EmbeddingBackend::default(),
        };
        let remote = raw.remote.map(RemoteEmbeddingConfig::try_from).transpose()?;
        // #317 rework: the `model = "..."` selector names the MODEL; a `[remote]` block serves THAT
        // model over Ollama. The two no longer have to "agree on a backend" — the old
        // model="ollama" coupling (`RemoteEmbeddingBackendMismatch` /
        // `RemoteEmbeddingMissingConfig`) is gone. The only coherence left: Ollama can only
        // serve TRANSFORMER models, so a `[remote]` block on a static/hash model is a
        // misconfiguration the dim probe couldn't usefully explain — reject it here with a
        // clear message.
        // A `[remote]` block REQUIRES a transformer (FastEmbed) selected model — Ollama can only
        // serve those. Reject when the selected backend is NOT FastEmbed: that covers static/hash
        // (`registry_backend()` is `Some(Hash | Model2Vec)`) AND `model = "none"` (embeddings
        // disabled → `registry_backend()` is `None`), which would otherwise slip through and leave
        // a remote block that never installs or provisions anything.
        if remote.is_some() && !matches!(backend.registry_backend(), Some(Backend::FastEmbed)) {
            return Err(ConfigError::RemoteEmbeddingNonTransformerModel(
                backend.as_str().to_string(),
            ));
        }
        Ok(Self { backend, runtime: raw.runtime.into(), remote })
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawRemoteEmbedding {
    model: Option<String>,
    backend: Option<String>,
    endpoint: Option<String>,
    cookbook: Option<String>,
    query_endpoint: Option<String>,
    auth_env: Option<String>,
    gpu: Option<String>,
    num_ctx: Option<u32>,
    batch_size: Option<u32>,
    concurrency: Option<u32>,
    max_batch_chars: Option<usize>,
    request_timeout_s: Option<u64>,
}

/// Whether the URL's authority embeds userinfo (`user[:pass]@host`). Parses the authority as
/// everything after `scheme://` up to the next `/`, `?`, or `#`, and reports an `@` in it. A bare
/// host, an `@` in the path/query, or a URL with no scheme separator all return false. Used to keep
/// credentials out of the endpoint string before it is persisted into the index meta.
pub fn endpoint_authority_has_userinfo(endpoint: &str) -> bool {
    let after_scheme = endpoint.split_once("://").map_or(endpoint, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or(after_scheme);
    authority.contains('@')
}

/// If the cookbook spec's FIRST token is a RELATIVE recipe PATH, return the spec with that token
/// resolved against `config_dir` (the `rag-rat.toml` directory). Returns `None` — leave the spec
/// unchanged — for an npm-package spec (`@scope/pkg`, a bare name) or an ALREADY-ABSOLUTE path; the
/// recipe runner (`node`/`npx`) would otherwise resolve a relative path against the process CWD
/// (wherever reconcile/the watcher runs), giving ENOENT from a subdir or a daemon (R6).
///
/// "Path-shaped" = starts with `./` or `../`, OR ends in a recipe extension (`.mjs`/`.js`/`.ts`/
/// `.mts`). Only the first whitespace token (the path) is rewritten; provider subcommand/args after
/// it are preserved verbatim.
pub(crate) fn resolve_relative_cookbook_path(cookbook: &str, config_dir: &Path) -> Option<String> {
    let mut tokens = cookbook.split_whitespace();
    let first = tokens.next()?;
    let lower = first.to_ascii_lowercase();
    let is_path_shaped = first.starts_with("./")
        || first.starts_with("../")
        || lower.ends_with(".mjs")
        || lower.ends_with(".js")
        || lower.ends_with(".ts")
        || lower.ends_with(".mts");
    if !is_path_shaped || Path::new(first).is_absolute() {
        return None; // npm spec or already absolute → leave verbatim
    }
    let resolved = config_dir.join(first);
    // NATIVE separator, no slash rewrite. This string is a single `argv` entry for `node`/`npx tsx
    // <path>` (spawned directly, not through a shell), and both accept the platform-native
    // separator — `\` on Windows, `/` on Unix — so the join output is already correct as-is.
    // Rewriting `\`→`/` would be wrong two ways: on Windows it corrupts a verbatim
    // extended-length prefix (canonicalize can yield `\\?\C:\repo`, where the suffix MUST stay
    // backslash-delimited), and on Unix it would mangle a literal backslash in a filename.
    // `to_string_lossy` touches neither.
    let mut out = resolved.to_string_lossy().into_owned();
    for arg in tokens {
        out.push(' ');
        out.push_str(arg);
    }
    Some(out)
}

impl TryFrom<RawRemoteEmbedding> for RemoteEmbeddingConfig {
    type Error = ConfigError;

    fn try_from(raw: RawRemoteEmbedding) -> Result<Self, Self::Error> {
        let default = RemoteEmbeddingConfig::default();
        let trimmed = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        // The SERVER-side model name — required, non-empty. For ollama the ollama model name; for
        // infinity/vLLM the HF id the server was launched with.
        let model = raw.model.unwrap_or_default();
        let model = model.trim();
        if model.is_empty() {
            return Err(ConfigError::RemoteEmbeddingMissingModel);
        }
        // Which OpenAI-compatible server serves this block. Omitted → `ollama` (back-compat).
        let backend = match trimmed(raw.backend) {
            None => RemoteBackend::default(),
            Some(raw_backend) => RemoteBackend::from_db_str(&raw_backend)
                .ok_or(ConfigError::RemoteBackendUnknown(raw_backend))?,
        };
        // The MODE is INFERRED from which URL field is set (#318): EXACTLY ONE of `endpoint`
        // (connect) / `cookbook` (ephemeral). Both → ambiguous; neither → no server to reach.
        let endpoint = trimmed(raw.endpoint);
        let cookbook = trimmed(raw.cookbook);
        match (endpoint.is_some(), cookbook.is_some()) {
            (true, true) | (false, false) => {
                return Err(ConfigError::RemoteEmbeddingModeAmbiguous);
            },
            _ => {},
        }
        // SECRET HYGIENE: `endpoint`/`query_endpoint` are persisted WHOLE into the (secret-free)
        // index meta. A URL with userinfo (`https://user:token@host`) would copy that credential
        // into the SQLite index — reject it and direct the user to `auth_env`. Checked against the
        // URL authority only (between `scheme://` and the next `/`), so an `@` in a path/query is
        // fine. `cookbook` is a recipe spec, not a URL, so it is not checked.
        // EPHEMERAL: the LOCAL query box. Ephemeral chunks embed on the provisioned box, but that
        // box is torn down after reconcile, so QUERIES embed against `query_endpoint` (a local
        // server running the same model) — and `query_embed_config` PRESERVES the backend when it
        // rewrites the ephemeral config to a connect-shaped query config. `DEFAULT_QUERY_ENDPOINT`
        // is a local OLLAMA URL, so it only fits `backend = ollama`. For a non-ollama backend the
        // default would build a query embedder posting the wrong route (`/embeddings`) / an HF
        // model id at local Ollama → every query embed fails → silent, permanent BM25
        // fallback. So require an explicit `query_endpoint` there. Ignored for connect
        // (queries hit the connect endpoint).
        let query_endpoint = if cookbook.is_some() {
            match trimmed(raw.query_endpoint) {
                Some(qe) => Some(qe),
                None if backend == RemoteBackend::Ollama =>
                    Some(DEFAULT_QUERY_ENDPOINT.to_string()),
                None => {
                    return Err(ConfigError::RemoteQueryEndpointRequiredForBackend {
                        backend: backend.as_db_str(),
                    });
                },
            }
        } else {
            None
        };
        for url in [endpoint.as_deref(), query_endpoint.as_deref()].into_iter().flatten() {
            if endpoint_authority_has_userinfo(url) {
                return Err(ConfigError::RemoteEmbeddingEndpointHasCredentials);
            }
        }
        // Optional — local Ollama needs no auth; trim if present.
        let auth_env = trimmed(raw.auth_env);
        // EPHEMERAL-only: the GPU to provision. A PRESENT-but-empty/whitespace value is a config
        // error (clearer than silently dropping a meant-to-be-set key). Set together with a connect
        // `endpoint` it is meaningless — reject it rather than ignore it. The VALUE is
        // provider-specific (Modal GPU class / RunPod gpuTypeId); we do NOT validate it against an
        // allow-list — the provider does so at provision time.
        let gpu = match raw.gpu {
            Some(g) => {
                let g = g.trim();
                if g.is_empty() {
                    return Err(ConfigError::RemoteGpuEmpty);
                }
                if endpoint.is_some() {
                    return Err(ConfigError::RemoteGpuRequiresCookbook);
                }
                Some(g.to_string())
            },
            None => None,
        };
        if matches!(raw.num_ctx, Some(0)) {
            return Err(ConfigError::RemoteEmbeddingInvalidNumCtx);
        }
        let is_connect = endpoint.is_some();
        let concurrency = raw
            .concurrency
            .unwrap_or_else(|| RemoteEmbeddingConfig::omitted_concurrency_default(is_connect))
            .max(1);
        if concurrency > MAX_REMOTE_EMBEDDING_CONCURRENCY {
            return Err(ConfigError::RemoteEmbeddingConcurrencyTooHigh {
                value: concurrency,
                max: MAX_REMOTE_EMBEDDING_CONCURRENCY,
            });
        }
        Ok(Self {
            model: model.to_string(),
            backend,
            endpoint,
            cookbook,
            query_endpoint,
            auth_env,
            gpu,
            num_ctx: raw.num_ctx,
            batch_size: raw.batch_size.unwrap_or(default.batch_size),
            concurrency,
            max_batch_chars: raw.max_batch_chars.unwrap_or(default.max_batch_chars).max(1),
            request_timeout_s: raw.request_timeout_s.unwrap_or(default.request_timeout_s),
        })
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct RawEmbeddingRuntime {
    batch_size: Option<u32>,
    ort_threads: Option<u32>,
    omp_threads: Option<u32>,
    max_embedding_chars: Option<usize>,
}

impl From<RawEmbeddingRuntime> for EmbeddingRuntimeConfig {
    fn from(raw: RawEmbeddingRuntime) -> Self {
        let default = EmbeddingRuntimeConfig::default();
        Self {
            batch_size: raw.batch_size.unwrap_or(default.batch_size),
            ort_threads: raw.ort_threads.or(default.ort_threads),
            omp_threads: raw.omp_threads.or(default.omp_threads),
            max_embedding_chars: raw.max_embedding_chars.unwrap_or(default.max_embedding_chars),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct RawTarget {
    pub(crate) name: String,
    pub(crate) language: String,
    pub(crate) directories: Vec<String>,
    pub(crate) kind: Option<String>,
    pub(crate) include: Option<Vec<String>>,
    pub(crate) exclude: Option<Vec<String>>,
}
