use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::embedding_models::{
    Backend, EmbeddingModelSpec, FASTEMBED_MODEL_ID, MODEL2VEC_MODEL_ID, spec,
};
use crate::language::{Language, LanguageError};

#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub database: PathBuf,
    pub targets: Vec<ResolvedTarget>,
    pub llm: LlmConfig,
    pub watch: WatchConfig,
    pub log: LogConfig,
    pub version_check: VersionCheckConfig,
    pub oracle: OracleConfig,
    pub search: SearchConfig,
    pub memory: MemoryConfig,
    /// Optional `[index] repo_id` override for the consolidated global store — pins the repo's
    /// identity instead of deriving it from the root-commit hash. `None` = derive. Consumed by
    /// `crate::repo_identity::resolve_repo_identity`. An EXPLICIT `database` path never depends on
    /// it; a KEYLESS config's default does only through the identity-existence gate (a pin makes a
    /// non-git root identity-bearing, so it resolves to the global store instead of per-root).
    pub repo_id_override: Option<String>,
    /// Whether the GOVERNING config sets an explicit `[index] database` key. "Governing" is
    /// main-worktree-anchored (see `main_database_key`): in a linked worktree the MAIN checkout's
    /// config decides, exactly as it does for `repo_id` — a branch-local config can neither pin
    /// nor un-pin the repo's database. `rag-rat consolidate` keys its pinned-config refusal off
    /// this, so the refusal and `database` resolution can never disagree.
    pub database_key_pinned: bool,
    /// When `Config::load` re-anchored `[index] root` from a LINKED worktree to the MAIN checkout
    /// (so all worktrees share one base index), this holds the ORIGINAL linked-worktree root; else
    /// `None`. The pre-anchor root is otherwise lost after `anchor_root_to_main_worktree`. Used by
    /// the `index` command to warn that it is indexing the main checkout, not the named worktree
    /// (#427). Not read from TOML — populated during load.
    pub source_root_reanchored_from: Option<PathBuf>,
    /// Opt in to registering an EMPTY index (zero discovered files). Default `false`: the core
    /// registration path (`rebuild_with_progress`) refuses a first-time-empty registration with
    /// [`crate::index::EmptyIndexRefused`], so no entry point silently creates one (#427). The CLI
    /// `index --allow-empty` flag sets this `true`; every other caller (watcher, git-hook
    /// `maintenance`, MCP, init) leaves it `false`. Not read from TOML — set per invocation.
    pub allow_empty: bool,
}

/// Search-ranking knobs (`[search]`). Default OFF so the shipped fuse is byte-identical to today;
/// opt in per-repo via `rag-rat.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchConfig {
    /// Replace the binary git has-history boost with a graded recency+churn magnitude and add the
    /// generated/test demotion at the wide-pool rerank site (default false — opt in explicitly).
    /// A/B-swept on the commit-replay eval (`rag-rat eval --replay --rerank`); see
    /// [`crate::search::lexical::SearchOptions::graded_history`].
    pub graded_git_rerank: bool,
}

/// Repo-memory surfacing (`[memory]`) — how memory attachments and memory-query results render.
/// Default is `summary`: an LLM-compacted summary + verdict marker (dream-generated) instead of the
/// full body, so a drive-by attachment or a `memory_search` hit is scannable rather than an
/// 8000-char wall. `full` restores whole bodies everywhere. Either way, `memory show` /
/// `memory_show` always return the full body — that is the deliberate "expand on request" path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryConfig {
    pub surface: MemorySurface,
}

/// `[memory] surface` — the memory rendering mode. `Summary` (default) renders the dream-compacted
/// summary (when one exists for the memory's current body) plus a plain-text verdict marker,
/// falling back to the mechanical compact header (title-only) when no summary row exists; `Full`
/// keeps whole bodies. Persisted only in config, so a closed enum with a stable `as_str` round-trip
/// is enough (no DB column).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MemorySurface {
    /// Title + compacted summary + verdict marker, falling back to title-only when no summary row
    /// exists for the current body. The default — bodies are deferred to `memory show`.
    #[default]
    Summary,
    /// The full body (plus the mechanical compact header where a compact projection is emitted) —
    /// whole prose everywhere, the pre-summary behavior.
    Full,
}

impl MemorySurface {
    /// The stable config string (matches the toml `surface = "..."` value).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Summary => "summary",
        }
    }

    /// Parse a `surface = "..."` value (case-insensitive). `None` for an unrecognized value — the
    /// config layer turns that into `ConfigError::UnknownMemorySurface`.
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Self::Full),
            "summary" => Some(Self::Summary),
            _ => None,
        }
    }
}

/// Background auto-fresh oracle (`[oracle]`). Opt-in; default OFF. When `auto_run` is enabled, the
/// long-lived `rag-rat mcp` server runs the SCIP oracle for the active checkout when the index is
/// stale and quiet, heavily throttled by two gates (a long quiet-period debounce + a
/// minimum-interval floor) — see [`crate::index::oracle::auto_run_decision`]. SCIP production takes
/// minutes while edits arrive in seconds, so edge-collapsing alone would thrash; both gates are
/// required. Fail-open and detached: it never blocks a request and dies with the server process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleConfig {
    /// Run the oracle in the background on the MCP server (default false — opt in explicitly).
    pub auto_run: bool,
    /// Run only after the index has been quiet (no change) for at least this long. The debounce
    /// that keeps an active editing session from triggering a minutes-long SCIP pass on every
    /// save.
    pub auto_run_quiet_period_secs: u64,
    /// And at most once this often, regardless of churn. The minimum-interval floor.
    pub auto_run_min_interval_secs: u64,
}

impl Default for OracleConfig {
    fn default() -> Self {
        Self {
            auto_run: false,
            auto_run_quiet_period_secs: 900,
            auto_run_min_interval_secs: 21_600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionCheckConfig {
    /// Check crates.io for a newer published `rag-rat` and surface it to agents/operators (default
    /// true). Opt out with `[version_check] enabled = false` in `rag-rat.toml`. The check is
    /// best-effort, cached, and never blocks; disabling it makes no network calls at all.
    pub enabled: bool,
}

impl Default for VersionCheckConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchConfig {
    /// Run the background file watcher (default true). `RAG_RAT_NO_WATCH` overrides this off at
    /// the call site.
    pub enabled: bool,
    /// Quiet window (ms) before a debounced reindex pass.
    pub debounce_ms: u64,
    /// Hard cap (ms): force a pass after this much continuous activity, so sustained writes never
    /// starve the quiet-window debounce.
    pub max_latency_ms: u64,
    /// Periodic backstop: run a pass at least this often even with no events (0 disables). Covers
    /// event-blind filesystems (NFS, WSL2 `/mnt`) and a watcher that missed events, and bounds how
    /// long a wedged peer can leave the index stale.
    pub periodic_sweep_secs: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self { enabled: true, debounce_ms: 400, max_latency_ms: 2500, periodic_sweep_secs: 300 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "off" => Self::Off,
            "error" => Self::Error,
            "warn" => Self::Warn,
            "info" => Self::Info,
            "debug" => Self::Debug,
            "trace" => Self::Trace,
            _ => return None,
        })
    }

    /// The EnvFilter directive string for this level.
    pub fn as_filter_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

impl LogFormat {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    /// Master switch (default false). `RAG_RAT_LOG` can force-enable at init even when false.
    pub enabled: bool,
    pub level: LogLevel,
    /// Optional per-subsystem EnvFilter directives (e.g. `rag_rat_core::index::ai=debug`).
    pub filter: Option<String>,
    /// Resolved absolute log dir (finalized in `load()` to the db sibling by default).
    pub dir: PathBuf,
    pub format: LogFormat,
    /// Roll/prune a file once it exceeds this many bytes (0 disables the size check).
    pub max_file_bytes: u64,
    pub retention_days: u64,
    pub max_files: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            level: LogLevel::Info,
            filter: None,
            dir: PathBuf::from(".rag-rat/logs"),
            format: LogFormat::Text,
            max_file_bytes: 50 * 1024 * 1024,
            retention_days: 7,
            max_files: 200,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmConfig {
    pub embedding: EmbeddingConfig,
    pub dream: DreamLlmConfig,
}

/// Dream-mode model pass (`[llm.dream]`) — rag-rat's first generative-model dependency (#122),
/// serving its verdict/compaction turns over an OpenAI-compatible chat endpoint. Mirrors
/// [`EmbeddingConfig`]: an `enabled` gate plus a [`RemoteDreamConfig`] serving block (connect XOR
/// ephemeral). Default OFF, so `rag-rat dream` stays 100% deterministic unless the operator opts
/// in.
///
/// Unlike embeddings, `remote` is NOT optional: dream has no in-process backend, so an absent
/// `[llm.dream.remote]` block still resolves to a serving config — [`RemoteDreamConfig::default`],
/// a connect to a local Ollama (today's `[llm.dream.remote]` default).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DreamLlmConfig {
    /// Run the model pass at all (default false — opt in explicitly). When false, `rag-rat dream
    /// --verify` still runs the deterministic pass-0 findings; only the model turn is skipped.
    pub enabled: bool,
    /// Which chat server serves the dream turns (connect XOR ephemeral). Absent
    /// `[llm.dream.remote]` → [`RemoteDreamConfig::default`] (a local-Ollama connect).
    pub remote: RemoteDreamConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbeddingConfig {
    /// Which embedding backend to use for semantic (vector) recall. `init` picks a default based
    /// on repo size; see [`EmbeddingBackend`].
    pub backend: EmbeddingBackend,
    pub runtime: EmbeddingRuntimeConfig,
    /// Optional remote-embedding offload (`[llm.embedding.remote]`). When present, the
    /// indexer can hand embedding work to an HTTP server (Ollama's `/api/embed`) instead of
    /// running the model in-process — see [`RemoteEmbeddingConfig`]. Absent → `None` → in-process
    /// embedding only. Parsed + validated here; the dispatch that consumes it lands in #317 task
    /// 5.
    pub remote: Option<RemoteEmbeddingConfig>,
}

/// The embedding backend selector (`[llm.embedding] model = "..."`).
///
/// Resolves the toml `model = "..."` string through the [`crate::embedding_models`] registry by its
/// `model_id` (the HF path — NO aliases, #317), so ANY registered model is selectable by its full
/// name — adding a model to the registry makes it selectable here with no edit. The embeddings-off
/// choice (`none` / `off`) carries `None`. Kept a thin wrapper over a registry spec reference so
/// `config` resolves model identity without depending on `index` (`crate::embedding_models` has no
/// `index` dependency, so there is no cycle).
///
/// The common tiers, for reference:
/// - `sentence-transformers/all-MiniLM-L6-v2` (the `EmbeddingBackend::default`): MiniLM transformer
///   — best general quality, but the cold backfill is CPU-bound (~10-100 chunks/sec), so
///   impractical for very large repos.
/// - `minishlab/potion-retrieval-32M`: static token-vector lookup + mean-pool — ~100-500× faster on
///   CPU at some retrieval-quality cost (no context/word-order). The choice for huge repos that
///   still want vectors.
/// - `none`: structural + BM25 only; no dense vectors. `semantic_search` degrades to BM25. The
///   cheapest option for enormous codebases where any embedding backfill is too slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingBackend(Option<&'static EmbeddingModelSpec>);

impl EmbeddingBackend {
    /// Embeddings off — structural + BM25 only.
    pub const NONE: Self = Self(None);

    /// The FastEmbed MiniLM tier — the default backend (`init` recommends it for smaller repos).
    /// Resolved from the registry by model_id.
    pub fn fast_embed() -> Self {
        Self(spec(FASTEMBED_MODEL_ID))
    }

    /// The Model2Vec static tier (`init` recommends it for very large repos).
    pub fn model2vec() -> Self {
        Self(spec(MODEL2VEC_MODEL_ID))
    }

    /// The toml selector for this backend — the model_id (the HF path `init` renders back into
    /// `rag-rat.toml`), or `"none"` for the embeddings-off choice.
    pub fn as_str(self) -> &'static str {
        match self.0 {
            Some(spec) => spec.model_id,
            None => "none",
        }
    }

    /// The persisted embedding-model id this backend installs/activates, or `None` for the
    /// embeddings-off choice.
    pub fn model_id(self) -> Option<&'static str> {
        self.0.map(|spec| spec.model_id)
    }

    /// The registry [`Backend`] this selector resolves to (`None` for the embeddings-off choice).
    /// Lets `config` reason about a `model = "..."` selection without re-deriving the mapping the
    /// registry owns — e.g. the `[embedding.remote]` guardrail that rejects serving a
    /// non-transformer (static/hash) model over Ollama.
    pub fn registry_backend(self) -> Option<Backend> {
        self.0.map(|spec| spec.backend)
    }
}

impl Default for EmbeddingBackend {
    fn default() -> Self {
        Self::fast_embed()
    }
}

impl FromStr for EmbeddingBackend {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        // The embeddings-off keywords are case-insensitive; the MODEL selector is the HF path,
        // which is CASE-SENSITIVE (`all-MiniLM-L6-v2`, `BAAI/...`) — match it verbatim via
        // `spec`, never lowercased, or the case-sensitive model_id lookup would miss.
        match value.to_ascii_lowercase().as_str() {
            "none" | "off" | "bm25" => Ok(Self::NONE),
            _ => spec(value)
                .map(|spec| Self(Some(spec)))
                .ok_or_else(|| ConfigError::UnknownEmbeddingBackend(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingRuntimeConfig {
    pub batch_size: u32,
    pub ort_threads: Option<u32>,
    pub omp_threads: Option<u32>,
    pub max_embedding_chars: usize,
}

impl Default for EmbeddingRuntimeConfig {
    fn default() -> Self {
        Self {
            batch_size: 64,
            ort_threads: Some(4),
            omp_threads: Some(1),
            max_embedding_chars: 4000,
        }
    }
}

/// Which OpenAI-compatible embedding server serves a `[remote]` block. All three speak the SAME
/// wire API (`POST /v1/embeddings`), so the embedding client is IDENTICAL regardless of backend —
/// the selector only routes ephemeral provisioning (which cookbook container), the freshness /
/// vector- identity marker (different backends can produce slightly different vectors), the tune
/// cache key, and the `ai_models.runtime` install marker.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum RemoteBackend {
    /// ollama, via its `/v1/embeddings` OpenAI-compatibility route (the default; back-compat).
    #[default]
    Ollama,
    /// michaelfeil/infinity (`infinity_emb v2 --model-id <hf>`).
    Infinity,
    /// vLLM in embedding mode (`vllm serve <hf> --runner pooling --host 0.0.0.0`).
    Vllm,
}

impl RemoteBackend {
    /// The stable wire/DB string (matches the serde repr): the `ai_models.runtime` marker, and the
    /// backend discriminator folded into the freshness key + the tune cache key.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a config `backend = "..."` value (case-insensitive). `None` for an unknown value — the
    /// config layer turns that into `ConfigError::RemoteBackendUnknown`.
    pub fn from_db_str(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }

    /// The HTTP path (appended to the endpoint) of this backend's embeddings route. The
    /// request/response SHAPE is identical across backends (OpenAI `{model,input}` →
    /// `{data:[{embedding,index}]}`); only the PATH differs: ollama and vLLM expose the
    /// OpenAI-standard `/v1/embeddings`, while michaelfeil/infinity's `v2` server serves it at
    /// `/embeddings` (verified live against `michaelf34/infinity:latest-cpu`).
    pub fn embed_path(self) -> &'static str {
        match self {
            RemoteBackend::Ollama | RemoteBackend::Vllm => "/v1/embeddings",
            RemoteBackend::Infinity => "/embeddings",
        }
    }

    /// Whether this backend can serve CHAT completions (the dream verdict/compaction turn). Ollama
    /// (same image, pull a chat model) and vLLM (a generation runner, no `--runner pooling`) both
    /// can; michaelfeil/infinity is embeddings/rerank/classify only, so a `[llm.dream.remote]`
    /// block on `infinity` is a config error (`DreamBackendCannotServeChat`). Embeddings request
    /// the `embed` capability; dream requests `chat`.
    pub fn supports_chat(self) -> bool {
        match self {
            RemoteBackend::Ollama | RemoteBackend::Vllm => true,
            RemoteBackend::Infinity => false,
        }
    }

    /// The HTTP path (appended to the endpoint) of this backend's chat-completions route. Uniform
    /// across chat-capable backends — ollama and vLLM both expose the OpenAI-standard
    /// `/v1/chat/completions`; only the SERVING differs (see `supports_chat`), not the route.
    pub fn chat_path(self) -> &'static str {
        "/v1/chat/completions"
    }

    /// How long the ephemeral cookbook may take to provision + serve a box for this backend before
    /// rag-rat gives up — BOTH the Rust-side handshake deadline and (minus a margin) the
    /// `provision_timeout_s` the recipe budgets against.
    ///
    /// vLLM's published image (`vllm/vllm-openai`, a ~10–15 GB CUDA image) is an order of magnitude
    /// larger than ollama's or infinity's, so its cold start (image pull + GPU model load) needs a
    /// much longer ceiling: 300s times it out on Modal (the create alone can eat most of it,
    /// leaving too little for `waitUntilReady`). ollama/infinity cold-start in well under 300s.
    pub fn provision_timeout(self) -> Duration {
        match self {
            RemoteBackend::Ollama | RemoteBackend::Infinity => Duration::from_secs(300),
            RemoteBackend::Vllm => Duration::from_secs(900),
        }
    }
}

/// Remote-embedding offload (`[llm.embedding.remote]`). Hands embedding work to an
/// OpenAI-compatible HTTP server (`POST /v1/embeddings` on ollama/infinity/vLLM) instead of running
/// the model in-process — the lever for huge repos whose in-process backfill is too slow on the
/// indexing box. Optional: absent → in-process embedding only.
///
/// Carries NO `dim` field: the vector dimension comes from the registry spec of the SELECTED model
/// (`model = "sentence-transformers/all-MiniLM-L6-v2"`, dim 384) and is validated against the
/// server's first response at runtime by the embedder; duplicating it here would be redundant and
/// drift-prone. The `backend` selector (default `ollama`) picks WHICH server; the runtime is
/// implied by this block's mere PRESENCE (#317 rework).
///
/// The mode is INFERRED, not configured — there is no `mode` field (#318). EXACTLY ONE of
/// `endpoint` / `cookbook` is set:
/// - `endpoint` present → CONNECT: talk to an already-running Ollama at that URL.
/// - `cookbook` present → EPHEMERAL: the bulk-`reconcile` path provisions an on-demand box via the
///   cookbook subprocess, embeds the repo against it, then tears it down. Queries use the LOCAL box
///   at `query_endpoint` (same model → same GGUF vector space as the remote-embedded chunks).
///
/// `Serialize` lets the install step persist this verbatim into the secret-free
/// `active_embedding_remote_config` meta, so the `conn`-based `active_embedder` can reconstruct the
/// remote embedder for both chunk-embed (reconcile) and query-embed (search) without threading
/// config through the search path. SECRET-FREE: `auth_env` is the env-var NAME, never a token, so
/// the serialized JSON holds no secret. `Deserialize` is the read side of that meta round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEmbeddingConfig {
    /// The SERVER-side model name sent in the `/v1/embeddings` request body — the server's own
    /// identifier, NOT a `rag-rat` registry alias (the registry only supplies the dim parity
    /// contract). For `backend = "ollama"` this is the ollama model name (e.g. `"all-minilm"`);
    /// for infinity/vLLM it is the HuggingFace id the server was launched with (e.g.
    /// `"sentence-transformers/all-MiniLM-L6-v2"`).
    pub model: String,
    /// Which OpenAI-compatible server serves this block (`ollama` | `infinity` | `vllm`). The wire
    /// call is identical across backends; this only routes provisioning + the
    /// freshness/tune/install markers. `#[serde(default)]` → `Ollama` so config JSON persisted
    /// before this field (and any omitted TOML) still deserializes as the pre-existing ollama
    /// behavior.
    #[serde(default)]
    pub backend: RemoteBackend,
    /// CONNECT: base URL of an already-running Ollama (e.g. `"http://localhost:11434"`);
    /// `/api/embed` is appended by the embedder. Mutually exclusive with `cookbook`.
    pub endpoint: Option<String>,
    /// EPHEMERAL: the cookbook recipe rag-rat spawns to provision an on-demand box — an npm
    /// package spec (`"@rag-rat/cookbook/modal"`, run via `npx -y`) or a recipe file path
    /// (`.mjs`/`.js` → `node`, `.ts` → `npx tsx`). Mutually exclusive with `endpoint`.
    pub cookbook: Option<String>,
    /// EPHEMERAL: the LOCAL server used for QUERY embedding after the box is torn down (queries
    /// embed the same model on the same `backend` → identical vector space). Defaults to
    /// `http://localhost:11434` (local Ollama) ONLY for `backend = ollama`; a non-ollama backend
    /// must set this explicitly (its route/model differ, so the Ollama default would silently
    /// break query embedding — see `ConfigError::RemoteQueryEndpointRequiredForBackend`).
    /// Ignored in connect mode.
    pub query_endpoint: Option<String>,
    /// Name of the environment variable holding the bearer token, if the server needs auth. Local
    /// Ollama needs none, so this is optional; the embedder reads the var once at construction.
    pub auth_env: Option<String>,
    /// EPHEMERAL-only: the GPU the cookbook recipe should provision for the on-demand box. The
    /// value is PROVIDER-specific and validated by the provider at provision time, not here —
    /// Modal wants a GPU class (`A10G`/`T4`/`A100`), RunPod a `gpuTypeId`. `None` lets the
    /// recipe pick its default (Modal defaults to CPU; RunPod to a cheap on-demand GPU). Set
    /// together with a connect `endpoint` it is a config error (`RemoteGpuRequiresCookbook`).
    ///
    /// `#[serde(default)]`: this field post-dates the meta round-trip, so a config JSON persisted
    /// by an older binary (no `gpu` key) must still deserialize (→ `None`) instead of
    /// erroring.
    #[serde(default)]
    pub gpu: Option<String>,
    /// Optional Ollama context window (`options.num_ctx`) for `/api/embed` requests. Some local
    /// embedding GGUFs default too small for rag-rat's code chunks; this lets config raise the
    /// server context without shrinking chunk size or batch throughput.
    ///
    /// `#[serde(default)]`: this field post-dates the meta round-trip, so a config JSON persisted
    /// by an older binary (no `num_ctx` key) must still deserialize (→ `None`) instead of
    /// erroring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    /// How many texts to send per `/api/embed` request.
    pub batch_size: u32,
    /// How many `/api/embed` requests the remote embedder may keep in flight.
    ///
    /// `#[serde(default)]`: this field post-dates the meta round-trip. Older persisted remote meta
    /// has no mode-aware TOML context, so deserialize it as single-flight instead of making an
    /// existing connect endpoint fan out 32 requests after upgrade.
    #[serde(default = "legacy_remote_embedding_concurrency")]
    pub concurrency: u32,
    /// Maximum total input characters to include in one `/api/embed` request.
    ///
    /// `#[serde(default)]`: this field post-dates the meta round-trip, so a config JSON persisted
    /// by an older binary (no `max_batch_chars` key) must still deserialize with the default
    /// instead of erroring.
    #[serde(default = "default_remote_embedding_max_batch_chars")]
    pub max_batch_chars: usize,
    /// Per-request HTTP timeout, in seconds.
    pub request_timeout_s: u64,
}

/// The default LOCAL Ollama URL for ephemeral query embedding when `query_endpoint` is omitted.
pub const DEFAULT_QUERY_ENDPOINT: &str = "http://localhost:11434";
/// Upper bound a user may set for `[remote] concurrency`. Headroom for GPU backends
/// (infinity/vLLM), which do real dynamic-batched inference and keep scaling with client fan-out
/// well past 128 (an L4 serving all-MiniLM via infinity was still climbing at 128 with zero
/// failures). ollama is server-bound (a higher value doesn't help it), so this is opt-in per
/// config; the defaults stay low. The auto-tuner sweeps within whatever cap the user sets and
/// clamps the knee to it.
pub const MAX_REMOTE_EMBEDDING_CONCURRENCY: u32 = 512;
const DEFAULT_REMOTE_EMBEDDING_CONCURRENCY: u32 = 32;
const LEGACY_REMOTE_EMBEDDING_CONCURRENCY: u32 = 1;
const DEFAULT_REMOTE_EMBEDDING_MAX_BATCH_CHARS: usize = 384_000;

fn legacy_remote_embedding_concurrency() -> u32 {
    LEGACY_REMOTE_EMBEDDING_CONCURRENCY
}

fn default_remote_embedding_max_batch_chars() -> usize {
    DEFAULT_REMOTE_EMBEDDING_MAX_BATCH_CHARS
}

impl Default for RemoteEmbeddingConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            backend: RemoteBackend::Ollama,
            endpoint: None,
            cookbook: None,
            query_endpoint: None,
            auth_env: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: DEFAULT_REMOTE_EMBEDDING_CONCURRENCY,
            max_batch_chars: DEFAULT_REMOTE_EMBEDDING_MAX_BATCH_CHARS,
            request_timeout_s: 60,
        }
    }
}

impl RemoteEmbeddingConfig {
    /// Default for a missing TOML `concurrency` field after the mode has been inferred.
    ///
    /// Connect-mode configs may point at ordinary Ollama servers that were not started with
    /// matching `OLLAMA_NUM_PARALLEL`, so omitted connect concurrency stays single-flight. Cookbook
    /// configs are provisioned by rag-rat recipes that align server-side parallelism, so omitted
    /// ephemeral configs use the new parallel default.
    pub fn omitted_concurrency_default(is_connect: bool) -> u32 {
        if is_connect {
            LEGACY_REMOTE_EMBEDDING_CONCURRENCY
        } else {
            DEFAULT_REMOTE_EMBEDDING_CONCURRENCY
        }
    }

    /// Runtime-safe concurrency for deserialized/persisted configs. TOML parsing rejects explicit
    /// values above [`MAX_REMOTE_EMBEDDING_CONCURRENCY`], but older or manually-edited DB metadata
    /// can bypass that validation.
    pub fn bounded_concurrency(&self) -> u32 {
        Self::bounded_concurrency_value(self.concurrency)
    }

    pub fn bounded_concurrency_value(value: u32) -> u32 {
        value.clamp(1, MAX_REMOTE_EMBEDDING_CONCURRENCY)
    }

    /// CONNECT mode: an already-running server at `endpoint`. Exactly one of connect/ephemeral
    /// holds (config validation guarantees it), so `is_connect() == !is_ephemeral()`.
    pub fn is_connect(&self) -> bool {
        self.endpoint.is_some()
    }

    /// EPHEMERAL mode: provision an on-demand box via `cookbook` for the bulk reconcile.
    pub fn is_ephemeral(&self) -> bool {
        self.cookbook.is_some()
    }
}

/// Remote-serving config for the dream model pass (`[llm.dream.remote]`). The CHAT-flavored mirror
/// of [`RemoteEmbeddingConfig`]: it hands the verdict/compaction turn to an OpenAI-compatible chat
/// server (`POST /v1/chat/completions` on ollama/vLLM) instead of running a model in-process.
///
/// Chat is one-turn and budget-capped, so this carries NONE of the embedding block's batching knobs
/// (`query_endpoint`, `num_ctx`, `batch_size`, `concurrency`, `max_batch_chars`) — only the serving
/// selector plus a request timeout.
///
/// Like embeddings, the mode is INFERRED, not configured — EXACTLY ONE of `endpoint` / `cookbook`
/// is set:
/// - `endpoint` present → CONNECT: talk to an already-running chat server at that URL.
/// - `cookbook` present → EPHEMERAL: the dream command provisions an on-demand box via the cookbook
///   subprocess, runs the pass against it, then tears it down.
///
/// NOT `Serialize`/`Deserialize`: dream config does not round-trip through the index meta (unlike
/// the embedder, which reconstructs itself from persisted config), matching the old
/// `DreamModelConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDreamConfig {
    /// Which OpenAI-compatible server serves this block (`ollama` | `vllm`). `infinity` is
    /// embed-only and rejected at parse time (`DreamBackendCannotServeChat`). Omitted → `Ollama`.
    pub backend: RemoteBackend,
    /// CONNECT: base URL of an already-running chat server (e.g. `"http://localhost:11434"`);
    /// `/v1/chat/completions` is appended by the verdict client. Mutually exclusive with
    /// `cookbook`.
    pub endpoint: Option<String>,
    /// EPHEMERAL: the cookbook recipe rag-rat spawns to provision an on-demand box — an npm
    /// package spec (`"@rag-rat/cookbook modal"`, run via `npx -y`) or a recipe file path.
    /// Mutually exclusive with `endpoint`.
    pub cookbook: Option<String>,
    /// The SERVER-side chat model sent in the request body — the server's own identifier, NOT a
    /// rag-rat registry alias. For `backend = "ollama"` the ollama model name (e.g. `"qwen3:8b"`);
    /// for vLLM the HuggingFace id the server was launched with (e.g. `"Qwen/Qwen3-4B-Instruct"`).
    pub model: String,
    /// EPHEMERAL-only: the GPU the cookbook recipe should provision. PROVIDER-specific and
    /// validated by the provider at provision time, not here. Set together with a connect
    /// `endpoint` it is a config error (`DreamRemoteGpuRequiresCookbook`).
    pub gpu: Option<String>,
    /// Name of the environment variable holding the bearer token, if the server needs auth. Local
    /// Ollama needs none, so this is optional; the verdict client reads the var once at
    /// construction.
    pub auth_env: Option<String>,
    /// Per-request HTTP timeout, in seconds. A dense evidence pack against a remote model can take
    /// a while, so the default is generous.
    pub request_timeout_s: u64,
}

impl Default for RemoteDreamConfig {
    /// A local-Ollama CONNECT — byte-for-byte the pre-migration `[dream.model]` default, so an
    /// absent `[llm.dream.remote]` block preserves today's behavior.
    fn default() -> Self {
        Self {
            backend: RemoteBackend::Ollama,
            endpoint: Some("http://localhost:11434".to_string()),
            cookbook: None,
            model: "qwen3:4b-instruct".to_string(),
            gpu: None,
            auth_env: None,
            request_timeout_s: 300,
        }
    }
}

impl RemoteDreamConfig {
    /// CONNECT mode: an already-running server at `endpoint`. Exactly one of connect/ephemeral
    /// holds (config validation guarantees it), so `is_connect() == !is_ephemeral()`.
    pub fn is_connect(&self) -> bool {
        self.endpoint.is_some()
    }

    /// EPHEMERAL mode: provision an on-demand box via `cookbook` for the dream pass.
    pub fn is_ephemeral(&self) -> bool {
        self.cookbook.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub name: String,
    pub language: Language,
    pub directories: Vec<PathBuf>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub kind: TargetKind,
}

impl ResolvedTarget {
    /// Indexing precedence when more than one target claims the same file (lower sorts first /
    /// wins). Two levels: (1) kind — generated/tests/docs before source, the existing order;
    /// (2) within a kind, a language that claims an ambiguous extension as an upgrade
    /// ([`Language::upgrades_ambiguous_extension`]) wins it — so a `.h` covered by both a `c` and a
    /// `cpp` binding indexes as C++ (the deliberate intent), not C (alphabetical accident). Both
    /// the full-rebuild walk (first-claimer-wins) and the incremental per-path resolver use
    /// this.
    pub fn index_precedence(&self) -> (u8, u8) {
        let kind_rank = match self.kind {
            TargetKind::Generated => 0,
            TargetKind::Tests => 1,
            TargetKind::Docs => 2,
            TargetKind::Source => 3,
        };
        let upgrade_rank = u8::from(!self.language.upgrades_ambiguous_extension());
        (kind_rank, upgrade_rank)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Source,
    Generated,
    Docs,
    Tests,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Generated => "generated",
            Self::Docs => "docs",
            Self::Tests => "tests",
        }
    }
}

impl FromStr for TargetKind {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "source" => Ok(Self::Source),
            "generated" => Ok(Self::Generated),
            "docs" => Ok(Self::Docs),
            "tests" | "test" => Ok(Self::Tests),
            other => Err(ConfigError::UnknownTargetKind(other.to_string())),
        }
    }
}

impl Config {
    /// The deduplicated set of target directories (relative to [`Config::root`]) across all
    /// targets, in stable order. Used to scope `.gitignore` nested-discovery to the indexed trees
    /// (see [`crate::index::ignore_rules::IgnoreMatcher::compile`]) instead of recursing the whole
    /// root into unindexed siblings.
    pub fn target_directories(&self) -> Vec<PathBuf> {
        let mut seen = BTreeSet::new();
        let mut dirs = Vec::new();
        for target in &self.targets {
            for dir in &target.directories {
                if seen.insert(dir.clone()) {
                    dirs.push(dir.clone());
                }
            }
        }
        dirs
    }

    /// A copy of this (BASE) config whose `targets` are re-resolved from a LINKED worktree's own
    /// `rag-rat.toml` (at `<linked_worktree_root>/rag-rat.toml`), so an overlay refresh indexes
    /// that branch with its OWN target set — not the sweeping process's. The shared `root`
    /// (anchored to main), `database`, and the rest are kept: the overlay still resolves
    /// against the one base index, but the delta + ignore filtering see the branch's targets.
    /// Returns the config unchanged when the linked worktree has no readable/valid
    /// `rag-rat.toml`, or when its targets don't validate against the linked checkout — so a
    /// malformed branch config degrades to base targets rather than dropping the worktree. The
    /// branch targets are root-relative, so they apply to the shared `root` directly (#219
    /// review).
    ///
    /// Used by `refresh_worktree_overlays`: the main watcher/maintenance process refreshes every
    /// linked worktree, but a worktree whose branch ADDS a target (e.g. `extra/`) would otherwise
    /// be filtered against the sweeper's targets — pruning overlay rows a branch-launched hook
    /// indexed.
    pub fn for_linked_worktree_overlay(&self, linked_path: &Path) -> Self {
        let linked_targets = (|| {
            // `linked_path` may be the checkout root, a subdir of it, or the git dir (a hook); the
            // branch `rag-rat.toml` lives at the WORKDIR top, so resolve the workdir first.
            let workdir = crate::index::discover_repo(linked_path)
                .ok()
                .and_then(|repo| repo.workdir().map(Path::to_path_buf))
                .unwrap_or_else(|| linked_path.to_path_buf());
            let text = fs::read_to_string(workdir.join("rag-rat.toml")).ok()?;
            let raw: RawConfig = toml::from_str(&text).ok()?;
            // Targets are relative to the config's `[index].root` (a subdir layout puts the toml at
            // the worktree top with `root = "<subdir>"`); resolve + validate them there so the
            // stored, root-relative directories match the base config's spelling exactly.
            let target_root = workdir.join(raw.index.root.as_deref().unwrap_or("."));
            resolve_targets(&target_root, raw.target_bindings, raw.target).ok()
        })();
        match linked_targets {
            Some(targets) => Self { targets, ..self.clone() },
            None => self.clone(),
        }
    }

    /// Load a config, resolving WHICH `rag-rat.toml` GOVERNS at one seam: in a linked git
    /// worktree, the MAIN worktree's config file is authoritative for the WHOLE config — identity,
    /// database location, targets, models, everything. A branch-local `rag-rat.toml` (an older
    /// branch, a divergent checkout) cannot fork any of it; when its content differs from main's
    /// it is ignored with a one-line warning naming the ignored file. This subsumes the historical
    /// per-key anchoring (root #218/#219, targets #219, `repo_id` #413, `database` A7) — the
    /// question "which config governs this repo" is answered once, so new keys are main-anchored
    /// by default and cannot re-open the split-brain class.
    ///
    /// DECLARED WORKTREE-LOCAL ALLOW-LIST (the only keys that legitimately vary per worktree):
    ///  * `target_bindings` / `[[target]]` — FOR THE OVERLAY INDEX ONLY, read by
    ///    [`Config::for_linked_worktree_overlay`] from the linked checkout's own file, because a
    ///    branch may add/remove source dirs and its overlay must index its own file set (#219). The
    ///    BASE config's targets remain main-anchored here.
    ///
    /// Everything else, present and future, resolves from the governing (main) config.
    ///
    /// EDGE POSTURES:
    ///  * Main worktree resolvable but CONFIG-LESS → the local config governs, with a warning (the
    ///    repo's config belongs in main; until it exists there, the branch copy is all we have —
    ///    best-effort, mirroring the old per-key fallbacks).
    ///  * Main config exists but FAILS to parse/read → the error PROPAGATES: loading from any
    ///    worktree must behave like loading from main, errors included (silently falling back to
    ///    the branch config would fork the repo exactly when main is briefly broken).
    ///  * No resolvable main (bare-repo hubs, pruned main, custom GIT_DIR, non-git roots) →
    ///    `main_worktree_root` is `None`, the local config governs unchanged — there is no
    ///    designated main to defer to.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)?;
        // Parse the LOCAL file but DO NOT fail yet: when a main config governs, the branch-local
        // file's contents are irrelevant by design — a parse/validation failure there must fold
        // into the divergence warning, never block every command from the linked checkout (Codex
        // batch 8, finding 2). Wherever the local config GOVERNS, the error is fatal as always.
        // The `[local_ai]` / `[dream]` rejections are part of local validity — see the
        // `RawConfig` presence-capture fields.
        let local_parse: Result<RawConfig, ConfigError> =
            toml::from_str::<RawConfig>(&text).map_err(ConfigError::from).and_then(|raw| {
                if raw.local_ai.is_some() {
                    Err(ConfigError::LocalAiTableRenamed)
                } else if raw.dream.is_some() {
                    Err(ConfigError::DreamTableMoved)
                } else {
                    Ok(raw)
                }
            });
        let local_config_dir = path.parent().unwrap_or_else(|| Path::new("."));
        // The topology subject must be a discoverable directory: a RELATIVE config path like
        // `rag-rat.toml` has the EMPTY path as its parent (`Path::parent` yields `Some("")`, not
        // `None`), which git discovery cannot open — it means the process cwd.
        let local_checkout =
            if local_config_dir.as_os_str().is_empty() { Path::new(".") } else { local_config_dir };

        // Best-effort resolution of what the LOCAL checkout's own `[index] root` names, taken
        // BEFORE the governing seam below picks a winner — used only to detect + report a re-anchor
        // to the caller (#427), never to decide anything (the seam is the sole source of truth for
        // governance). `None` on any parse/resolution failure; that is not a second error path,
        // just a diagnostic that stays silent when it cannot be computed.
        let local_root_named: Option<PathBuf> = local_parse.as_ref().ok().and_then(|local_raw| {
            normalize_existing_dir(
                &local_config_dir
                    .join(local_raw.index.root.clone().unwrap_or_else(|| ".".to_string())),
            )
            .ok()
        });

        // THE GOVERNING SEAM (see the doc comment). Linked-ness comes from git TOPOLOGY — the
        // checkout holding the config file vs the repo's designated main worktree
        // ([`linked_worktree_main_root`]) — and governance is UNCONDITIONAL on that predicate.
        // It must never hang off a root-anchoring proxy: a branch-only `[index] root` makes
        // `anchor_root_to_main_worktree` return the local root unchanged, and an equality trigger
        // would then let the branch config govern database/identity/models — the exact
        // split-brain the seam exists to prevent (Codex batch 8, finding 3). Anchoring outcomes
        // affect ROOT resolution only, never who governs.
        let (mut raw, config_dir, root, target_validation_root) =
            match linked_worktree_main_root(local_checkout) {
                Some(main_top) => match governing_main_config(&main_top)? {
                    Some((main_raw, main_config_dir)) => {
                        let divergent = match &local_parse {
                            Ok(local_raw) => *local_raw != main_raw,
                            Err(_) => true,
                        };
                        if divergent {
                            let ignored =
                                path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
                            let invalid_note =
                                if local_parse.is_err() { " (also invalid)" } else { "" };
                            eprintln!(
                                "rag-rat: ignoring branch config{invalid_note} {} — in a linked \
                                 worktree the main worktree's config governs ({}); edit that file \
                                 instead",
                                ignored.display(),
                                main_config_dir.join("rag-rat.toml").display(),
                            );
                        }
                        // Re-derive root from MAIN's own config, exactly as loading it directly
                        // would (its root is already the main worktree — anchoring is identity).
                        let main_root =
                            normalize_existing_dir(&main_config_dir.join(
                                main_raw.index.root.clone().unwrap_or_else(|| ".".to_string()),
                            ))?;
                        (main_raw, main_config_dir, main_root.clone(), main_root)
                    },
                    None => {
                        // Config-less main: the LOCAL config governs (best-effort, loudly), so
                        // its validity is fatal exactly as in a non-linked checkout.
                        let local_raw = local_parse?;
                        eprintln!(
                            "rag-rat: the main worktree has no rag-rat.toml; using {} until one \
                             exists there (the repo's config belongs in the main worktree)",
                            path.display(),
                        );
                        // Root stays anchored so the shared index still keys off the main
                        // checkout; targets validate against the local checkout where they
                        // exist (#219).
                        let local_root = normalize_existing_dir(&local_config_dir.join(
                            local_raw.index.root.clone().unwrap_or_else(|| ".".to_string()),
                        ))?;
                        let anchored_root = anchor_root_to_main_worktree(&local_root);
                        (local_raw, local_config_dir.to_path_buf(), anchored_root, local_root)
                    },
                },
                None => {
                    // The config's own checkout is the main worktree (or there is no designated
                    // main): the local config governs and its validity is fatal. Root anchoring
                    // still applies for the exotic `[index] root` pointing into a linked
                    // checkout (#218/#219) — an anchoring concern, not a governance one.
                    let local_raw = local_parse?;
                    let local_root = normalize_existing_dir(
                        &local_config_dir
                            .join(local_raw.index.root.clone().unwrap_or_else(|| ".".to_string())),
                    )?;
                    let anchored_root = anchor_root_to_main_worktree(&local_root);
                    (local_raw, local_config_dir.to_path_buf(), anchored_root, local_root)
                },
            };

        // #427: `root` may have ended up different from what the LOCAL checkout's own `[index]
        // root` names — either because a linked worktree's config was overridden wholesale by
        // MAIN's (the common case, above), or because `anchor_root_to_main_worktree` rebased an
        // exotic branch-only root (#218/#219). Either way, report the pre-anchor value so `index`
        // can warn the operator they're indexing the main checkout, not the worktree they named.
        let source_root_reanchored_from = local_root_named.filter(|named| *named != root);

        // The database path (A7 default flip): an explicit `database` key is honored as-is — the
        // deprecated per-repo deployment, that repo stays un-consolidated and never syncs. ABSENT,
        // the default is the CONSOLIDATED GLOBAL store, EXCEPT a pre-existing legacy
        // `.rag-rat/index.sqlite` is kept (with a deprecation nudge toward `rag-rat consolidate`).
        // Relative explicit paths (and the legacy path) resolve against the MAIN worktree TOP —
        // NOT `root`, which may be a subdirectory — so every worktree of a repo AND any
        // `root="<subdir>"` config land on the SAME index.
        let db_base = main_worktree_root(&root).unwrap_or_else(|| root.clone());
        let repo_id_override =
            raw.index.repo_id.take().map(|id| id.trim().to_string()).filter(|id| !id.is_empty());
        let governing_database_key = raw.index.database.take();
        let database_key_pinned = governing_database_key.is_some();
        let database = match governing_database_key {
            Some(db) if Path::new(&db).is_absolute() => PathBuf::from(db),
            Some(db) => db_base.join(db),
            // The keyless default probes the repo IDENTITY (root + the governing `[index]
            // repo_id` pin): only an identity-BEARING root may land in the shared global store —
            // see `default_database_with_disposition`.
            None => resolve_default_database(&db_base, &root, repo_id_override.as_deref()),
        };
        // The identity gate's SECOND entrance (Codex batch 8, finding 5): an explicit pin AT the
        // consolidated global store bypasses the keyless identity gate above, and an
        // identity-less root (non-git, unborn HEAD) opening the shared store would fall through
        // to adoption's sole-repo fallback — scoping this project onto whichever SIBLING repo
        // sorts first. Refuse at resolution with the remedy. (The structural backstop for every
        // other shared-path pin shape lives in `adopt_repo_from_config`: an identity-less open
        // never sole-picks on a multi-repo database.)
        if database_key_pinned
            && Some(database.as_path()) == crate::data_dir::global_database_path().as_deref()
            && !crate::repo_identity::identity_is_resolvable(&root, repo_id_override.as_deref())
        {
            return Err(ConfigError::GlobalPinWithoutIdentity);
        }
        // Targets resolve from the GOVERNING config; validation runs against the checkout that
        // config describes (main when main governs; the local checkout on the config-less-main
        // fallback, tolerating branch-only dirs — #219). `ResolvedTarget.directories` are
        // root-relative, so the stored targets are checkout-independent either way. A linked
        // branch's own target set is NOT lost: the overlay refresh reads the branch config via
        // `for_linked_worktree_overlay` and indexes the branch with it (#219).
        let targets = resolve_targets(&target_validation_root, raw.target_bindings, raw.target)?;
        let mut llm = LlmConfig::try_from(raw.llm)?;
        // Resolve a RELATIVE cookbook recipe PATH against the GOVERNING config dir, not the
        // process CWD (R6): the recipe is handed to `node`/`npx`, which resolve it against
        // wherever reconcile/the watcher runs — ENOENT from a subdir or a daemon.
        if let Some(remote) = llm.embedding.remote.as_mut()
            && let Some(cookbook) = remote.cookbook.as_ref()
            && let Some(resolved) = resolve_relative_cookbook_path(cookbook, &config_dir)
        {
            remote.cookbook = Some(resolved);
        }
        // Same relative-cookbook resolution for the dream remote — its recipe is handed to
        // `node`/`npx` too, so a relative path must resolve against the config dir, not the process
        // CWD. `remote` is not optional for dream (a local-Ollama connect default), so only the
        // ephemeral case has a cookbook to rewrite.
        if let Some(cookbook) = llm.dream.remote.cookbook.as_ref()
            && let Some(resolved) = resolve_relative_cookbook_path(cookbook, &config_dir)
        {
            llm.dream.remote.cookbook = Some(resolved);
        }
        let watch = raw.watch.into();
        let version_check = raw.version_check.into();
        let oracle = raw.oracle.into();
        let search = raw.search.into();
        let memory = MemoryConfig::try_from(raw.memory)?;
        let mut log = LogConfig::try_from(raw.log)?;
        // Finalize `dir`: empty (unset) → sibling of the db (`<db_parent>/logs`); a set value is
        // resolved relative to the GOVERNING config dir (absolute honored).
        log.dir = if log.dir.as_os_str().is_empty() {
            database.parent().map(|p| p.join("logs")).unwrap_or_else(|| PathBuf::from("logs"))
        } else if log.dir.is_absolute() {
            log.dir.clone()
        } else {
            config_dir.join(&log.dir)
        };

        Ok(Self {
            root,
            database,
            targets,
            llm,
            watch,
            version_check,
            oracle,
            search,
            memory,
            log,
            repo_id_override,
            database_key_pinned,
            source_root_reanchored_from,
            allow_empty: false,
        })
    }
}

/// The MAIN worktree's parsed config, when one exists — the governing side of `Config::load`'s
/// seam. `main_top` is the already-derived main worktree top ([`linked_worktree_main_root`]).
/// `Ok(None)` = main has NO `rag-rat.toml` (the local-governs fallback); a main config that
/// exists but cannot be read or parsed PROPAGATES its error (loading from a linked worktree must
/// behave like loading from main, errors included). Returns the main worktree TOP as the
/// governing config dir.
fn governing_main_config(main_top: &Path) -> Result<Option<(RawConfig, PathBuf)>, ConfigError> {
    let main_top = main_top.to_path_buf();
    let main_config_path = main_top.join("rag-rat.toml");
    let text = match fs::read_to_string(&main_config_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let raw: RawConfig = toml::from_str(&text)?;
    if raw.local_ai.is_some() {
        return Err(ConfigError::LocalAiTableRenamed);
    }
    if raw.dream.is_some() {
        return Err(ConfigError::DreamTableMoved);
    }
    Ok(Some((raw, main_top)))
}

fn resolve_targets(
    root: &Path,
    simple: BTreeMap<String, Vec<String>>,
    expanded: Vec<RawTarget>,
) -> Result<Vec<ResolvedTarget>, ConfigError> {
    let mut names = BTreeSet::new();
    let mut targets = Vec::new();

    for (language_name, directories) in simple {
        let language = Language::from_str(&language_name)?;
        let kind =
            if language == Language::Markdown { TargetKind::Docs } else { TargetKind::Source };
        let name = language.as_str().to_string();
        push_target(root, &mut names, &mut targets, ResolvedTarget {
            include: language.default_include_globs(),
            exclude: Vec::new(),
            name,
            language,
            directories: directories.into_iter().map(PathBuf::from).collect(),
            kind,
        })?;
    }

    for target in expanded {
        let language = Language::from_str(&target.language)?;
        let kind = target
            .kind
            .as_deref()
            .map(TargetKind::from_str)
            .transpose()?
            .unwrap_or(TargetKind::Source);
        push_target(root, &mut names, &mut targets, ResolvedTarget {
            name: target.name,
            language,
            directories: target.directories.into_iter().map(PathBuf::from).collect(),
            include: target.include.unwrap_or_else(|| language.default_include_globs()),
            exclude: target.exclude.unwrap_or_default(),
            kind,
        })?;
    }

    Ok(targets)
}

fn push_target(
    root: &Path,
    names: &mut BTreeSet<String>,
    targets: &mut Vec<ResolvedTarget>,
    target: ResolvedTarget,
) -> Result<(), ConfigError> {
    if !names.insert(target.name.clone()) {
        return Err(ConfigError::DuplicateTarget(target.name));
    }
    for directory in &target.directories {
        let full_path = root.join(directory);
        if !full_path.is_dir() {
            return Err(ConfigError::MissingDirectory(directory.clone()));
        }
    }
    targets.push(target);
    Ok(())
}

/// Re-anchor a **linked** worktree's `root` to the equivalent path under the **main** worktree, so
/// every worktree of a repo resolves to one root + one shared index — while PRESERVING any
/// subdirectory the config root points at (a `root="<subdir>"` rebases to `<main>/<subdir>`, not
/// the repo top). The main worktree (and non-git dirs) resolve to themselves. Collapsing a subdir
/// root to the repo top changed the indexed file set and could fail config load when a target dir
/// exists only under the subdir (#219 review).
fn anchor_root_to_main_worktree(root: &Path) -> PathBuf {
    let Ok(repo) = crate::index::discover_repo(root) else {
        return root.to_path_buf();
    };
    let (Some(workdir), Some(main_root)) = (repo.workdir(), main_worktree_root(root)) else {
        return root.to_path_buf();
    };
    let workdir = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    if main_root == workdir {
        return root.to_path_buf(); // already the main worktree — keep the configured (sub)root
    }
    // Linked worktree: rebase root's in-worktree subpath under the main worktree top. `root` is
    // canonicalized by `normalize_existing_dir`, so it strips cleanly against the canonical
    // workdir; `root == workdir` (a `root="."` config) yields an empty suffix → the main
    // worktree top.
    let anchored = match root.strip_prefix(&workdir) {
        Ok(rel) => main_root.join(rel),
        Err(_) => main_root,
    };
    // The anchored path must EXIST in the main checkout. When the linked branch sets `[index].root`
    // to a directory that lives only on the branch (not in main), `main_root.join(rel)` points at a
    // missing path, which would break the later `discover_repo` / database-base / base-indexing
    // calls that read `Config.root`. Keep the linked checkout's (validated, existing) root in that
    // case — the overlay still serves the branch; the base just can't anchor there (#219 review).
    if anchored.is_dir() { anchored } else { root.to_path_buf() }
}

/// The main worktree root, derived from the git common dir (`<main>/.git`). Returns `None` outside
/// a standard git repo (bare repo, custom `GIT_DIR`, git unavailable) so resolution falls back to
/// `root` — never guess.
/// The DEFAULT config path for the checkout containing `dir` — the DISCOVERY side of the
/// governing seam. [`Config::load`]'s seam decides which config WINS once a file is loaded; this
/// decides where the CLI LOOKS when no explicit `--config` was given. Without it, a linked
/// worktree with no branch-local `rag-rat.toml` (the state `init`'s refusal deliberately leaves)
/// dies at the existence check before the seam ever runs (Codex batch 9). Resolution:
///  * a local `rag-rat.toml` exists → use it. In a linked worktree the seam then governs from main
///    AND emits the divergence warning — routing discovery straight to main when a branch file
///    exists would silently skip that warning, so the file's presence keeps the load going through
///    the seam (governance is identical either way).
///  * no local file, linked worktree → the MAIN worktree's `rag-rat.toml` (whether or not it exists
///    yet — a missing-config hint should name the path where the config BELONGS).
///  * no local file, main/non-git → the nearest `rag-rat.toml` in an ANCESTOR directory, so a
///    launch from a SUBDIRECTORY of a rag-rat repo still finds the repo's config; failing that, the
///    local path (the hint names it).
///
/// An EXPLICIT `--config` path never routes through this — a user override is taken literally.
pub fn discover_config_path(dir: &Path) -> PathBuf {
    let local = dir.join("rag-rat.toml");
    if local.exists() {
        return local;
    }
    match linked_worktree_main_root(dir) {
        // Linked worktree: the MAIN checkout's config governs UNCONDITIONALLY — return its path
        // even when the file is missing. This is the governing seam; the ancestor walk
        // below must never preempt it (a subdir of the MAIN worktree is deliberately NOT
        // classified as linked, so it falls into the `None` arm and the walk finds main's
        // config there).
        Some(main_top) => main_top.join("rag-rat.toml"),
        None => nearest_config_at_or_above(dir).unwrap_or(local),
    }
}

/// Walk upward from `dir` to the nearest directory (at or above `dir`) holding a `rag-rat.toml`,
/// returning that file's path. `None` ⇒ no rag-rat repo at or above `dir`. The single upward-walk
/// primitive: `discover_config_path`'s non-worktree arm uses it so a subdirectory launch inside a
/// repo finds the repo's config, and the Claude-hook cwd→config resolver
/// (`claude_hook::find_config`) loads the returned path. Unbounded (to the filesystem root),
/// matching a repo that has no other marker than its `rag-rat.toml`.
pub fn nearest_config_at_or_above(dir: &Path) -> Option<PathBuf> {
    // Resolve to an ABSOLUTE path first. A relative `dir` such as `.` has `parent() == Some("")`
    // then `None`, so the ancestor walk would only ever inspect `.` and never climb the real
    // filesystem tree (the callers pass `Path::new(".")` for cwd). `canonicalize` also collapses
    // `..`/symlinks so the climb follows the true directory chain. If it fails (a non-existent
    // `dir`), fall back to walking `dir` as given rather than aborting discovery.
    let absolute = dir.canonicalize().ok();
    let mut current = absolute.as_deref().or(Some(dir));
    while let Some(cur) = current {
        let candidate = cur.join("rag-rat.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        current = cur.parent();
    }
    None
}

/// The MAIN worktree top for `root` when `root` sits in a LINKED git worktree — `None` when the
/// checkout containing `root` IS the main worktree (or the layout has no designated main:
/// bare-repo hubs, custom `GIT_DIR`, non-git dirs). This is THE linked-ness predicate — derived
/// from git topology (the discovered checkout's WORKDIR vs the common dir's main), never from a
/// path-equality proxy: comparing `root` itself to main falsely classifies a SUBDIRECTORY of the
/// main worktree as linked, and root-anchoring success is defeated by a branch-only `[index]
/// root` (Codex batch 8, findings 1+3). Both `Config::load`'s governing seam and the CLI's
/// `init` refusal resolve linked-ness through this one helper.
pub fn linked_worktree_main_root(root: &Path) -> Option<PathBuf> {
    let repo = crate::index::discover_repo(root).ok()?;
    let main = main_worktree_root(root)?;
    let workdir = repo.workdir()?;
    let workdir = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    (main != workdir).then_some(main)
}

fn main_worktree_root(root: &Path) -> Option<PathBuf> {
    let repo = crate::index::discover_repo(root).ok()?;
    let common_dir = repo.common_dir().canonicalize().ok()?;
    // Only the standard `<main>/.git` layout maps cleanly to a main worktree root.
    if common_dir.file_name()?.to_str()? != ".git" {
        return None;
    }
    let main_root = common_dir.parent()?.to_path_buf();
    main_root.is_dir().then_some(main_root)
}

/// The database path a `rag-rat.toml` WITHOUT a `database` key resolves to (A7), pure — no logging,
/// no filesystem writes. The default is the CONSOLIDATED GLOBAL store (`data_dir()/rag-rat.sqlite`)
/// — one database per machine — with two exceptions that keep an upgrade safe:
///  1. A pre-existing legacy `<main_worktree>/.rag-rat/index.sqlite` is honored, so a repo indexed
///     before the flip never silently abandons its authored memories; once `rag-rat consolidate`
///     imports and renames the file away, resolution falls through to the global path.
///  2. When no data dir resolves at all (no `HOME`/XDG on this platform), fall back to the legacy
///     per-repo path rather than failing — the pre-A7 behavior.
///
/// `db_base` is the directory a relative/legacy path anchors to (the main worktree top — see
/// `Config::load`). Public so the init wizard can display where a keyless config will land without
/// loading one.
pub fn default_database_path(
    db_base: &Path,
    identity_root: &Path,
    repo_id_override: Option<&str>,
) -> PathBuf {
    let (path, _) = default_database_with_disposition(db_base, identity_root, repo_id_override);
    path
}

/// How a keyless config's default database resolved — the load path warns per variant.
enum DefaultDatabaseDisposition {
    /// The root has NO derivable repo identity (non-git dir, unborn `git init`, no pin): the
    /// per-root legacy path, exactly the pre-flip posture.
    IdentityLess,
    /// The legacy per-repo file is in use (awaiting `rag-rat consolidate`).
    Legacy,
    /// The global store (or the no-data-dir legacy fallback path, which does not exist on disk).
    Global,
    /// The global store, with a STRAY legacy file present DESPITE the `.imported` marker — an old
    /// binary, a backup restore, or a stray process re-created it after consolidation.
    GlobalWithStrayLegacy,
}

/// [`default_database_path`] plus the [`DefaultDatabaseDisposition`] the resolution took.
///
/// IDENTITY GATE (first, before everything): the global default REQUIRES a resolvable repo
/// identity (`repo_identity::identity_is_resolvable` — a pin, or a git repo with a born HEAD).
/// An identity-less root stays on its per-root `.rag-rat/index.sqlite` exactly as pre-flip:
/// in the shared global store every such root would pool under the ONE `__unassigned__`
/// placeholder scope — two fresh non-git projects would see and overwrite each other — and an
/// unborn repo would strand its placeholder rows the moment its first commit mints a real id.
/// Per-root, the placeholder stays a single-repo-DB concept with its existing adoption flow
/// (first commit → the placeholder adopts in the per-root DB → consolidate when ready).
///
/// The `.imported` marker is a STAY-GLOBAL LATCH: once `rag-rat consolidate` has renamed the legacy
/// file away, a keyless repo resolves to the global store even if a legacy `index.sqlite`
/// REAPPEARS beside the marker (an old binary, a restored backup, a stray process) — otherwise the
/// stray would silently divert the repo off the store its memories were imported into.
fn default_database_with_disposition(
    db_base: &Path,
    identity_root: &Path,
    repo_id_override: Option<&str>,
) -> (PathBuf, DefaultDatabaseDisposition) {
    let legacy = db_base.join(".rag-rat/index.sqlite");
    if !crate::repo_identity::identity_is_resolvable(identity_root, repo_id_override) {
        return (legacy, DefaultDatabaseDisposition::IdentityLess);
    }
    let marker = db_base.join(".rag-rat/index.sqlite.imported");
    let global = crate::data_dir::global_database_path();
    if marker.exists()
        && let Some(global) = global
    {
        let disposition = if legacy.exists() {
            DefaultDatabaseDisposition::GlobalWithStrayLegacy
        } else {
            DefaultDatabaseDisposition::Global
        };
        return (global, disposition);
    }
    if legacy.exists() {
        return (legacy, DefaultDatabaseDisposition::Legacy);
    }
    (global.unwrap_or(legacy), DefaultDatabaseDisposition::Global)
}

/// The load-time wrapper around [`default_database_path`]: same resolution, plus a one-line notice
/// per disposition — the deprecation nudge toward `rag-rat consolidate` while the legacy file is
/// what keeps the repo off the global store, and a stray-file warning when a legacy file reappears
/// after consolidation (it is ignored, never silently adopted). An identity-less root is SILENT:
/// it is the pre-flip posture, and `rag-rat consolidate` refuses identity-less repos, so a nudge
/// would dead-end.
fn resolve_default_database(
    db_base: &Path,
    identity_root: &Path,
    repo_id_override: Option<&str>,
) -> PathBuf {
    let (path, disposition) =
        default_database_with_disposition(db_base, identity_root, repo_id_override);
    match disposition {
        DefaultDatabaseDisposition::Legacy => tracing::warn!(
            path = %path.display(),
            "using the legacy per-repo index at `.rag-rat/index.sqlite`; the default database is now \
             the consolidated global store. Run `rag-rat consolidate` to import this repo's memories \
             and switch to it."
        ),
        DefaultDatabaseDisposition::GlobalWithStrayLegacy => tracing::warn!(
            "a stray `.rag-rat/index.sqlite` exists beside the `.imported` consolidation marker; \
             ignoring it and staying on the consolidated global store. Delete the stray file (its \
             contents were NOT imported)."
        ),
        DefaultDatabaseDisposition::Global | DefaultDatabaseDisposition::IdentityLess => {},
    }
    path
}

/// The DEFAULT legacy per-repo path a KEYLESS config at `root` would consult
/// (`<main_worktree_top>/.rag-rat/index.sqlite`) — what `rag-rat consolidate` compares a pinned
/// `database` path against to pick the right remedy: a pin AT this path just needs the key
/// removed, while a CUSTOM pin must also move its file here first (keyless resolution never looks
/// anywhere else, so removing the key alone would strand the custom file unimported).
pub fn default_legacy_database_path(root: &Path) -> PathBuf {
    main_worktree_root(root).unwrap_or_else(|| root.to_path_buf()).join(".rag-rat/index.sqlite")
}

fn normalize_existing_dir(path: &Path) -> Result<PathBuf, ConfigError> {
    let absolute =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let canonical = absolute.canonicalize()?;
    if !canonical.is_dir() {
        return Err(ConfigError::MissingDirectory(canonical));
    }
    Ok(canonical)
}

#[derive(Debug, Deserialize, PartialEq)]
struct RawConfig {
    #[serde(default)]
    index: RawIndex,
    #[serde(default)]
    llm: RawLlm,
    /// Presence-capture for the OLD `[local_ai]` table (renamed to `[llm]` in #317). Serde would
    /// otherwise SILENTLY DROP this now-unknown table, loading every embedding setting as a
    /// default (re-enabling FastEmbed, dropping a configured remote/runtime) on upgrade. We
    /// capture it so `load` can reject it loudly with a migration instruction instead of
    /// misconfiguring silently.
    #[serde(default)]
    local_ai: Option<toml::Value>,
    /// Presence-capture for the OLD top-level `[dream]` table (the dream model config moved to
    /// `[llm.dream]` / `[llm.dream.remote]`). Serde would otherwise SILENTLY DROP it, so an
    /// upgrade from `[dream.model] enabled = true` would load `[llm.dream] enabled = false`
    /// and run the deterministic passes only, never the model. Captured so `load` rejects it
    /// loudly with a migration instruction instead of silently downgrading.
    #[serde(default)]
    dream: Option<toml::Value>,
    #[serde(default)]
    watch: RawWatch,
    #[serde(default)]
    log: RawLog,
    #[serde(default)]
    version_check: RawVersionCheck,
    #[serde(default)]
    oracle: RawOracle,
    #[serde(default)]
    search: RawSearch,
    #[serde(default)]
    memory: RawMemory,
    #[serde(default)]
    target_bindings: BTreeMap<String, Vec<String>>,
    #[serde(default, rename = "target")]
    target: Vec<RawTarget>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
struct RawWatch {
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
struct RawLog {
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
struct RawVersionCheck {
    enabled: Option<bool>,
}

impl From<RawVersionCheck> for VersionCheckConfig {
    fn from(raw: RawVersionCheck) -> Self {
        Self { enabled: raw.enabled.unwrap_or(VersionCheckConfig::default().enabled) }
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
struct RawOracle {
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
struct RawSearch {
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
struct RawMemory {
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
struct RawIndex {
    root: Option<String>,
    database: Option<String>,
    /// `[index] repo_id` — pins the repo's identity for the consolidated global store instead of
    /// deriving it from the root-commit hash. Set it for a fork that must NOT share memories with
    /// its upstream, or a repo with no commits yet. Parsed here; consumed by
    /// `resolve_repo_identity` in a later workstream (no effect on path resolution).
    repo_id: Option<String>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
struct RawLlm {
    #[serde(default)]
    embedding: RawEmbedding,
    #[serde(default)]
    dream: RawDreamLlm,
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
struct RawDreamLlm {
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
struct RawRemoteDream {
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
struct RawEmbedding {
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
struct RawRemoteEmbedding {
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
fn resolve_relative_cookbook_path(cookbook: &str, config_dir: &Path) -> Option<String> {
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
struct RawEmbeddingRuntime {
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
struct RawTarget {
    name: String,
    language: String,
    directories: Vec<String>,
    kind: Option<String>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
}

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
    #[error(
        "[llm.embedding.remote] `backend` must be one of `ollama`, `infinity`, or `vllm` (got \
         `{0}`)"
    )]
    RemoteBackendUnknown(String),
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
        "[llm.embedding.remote] `gpu` is set but empty — give it a provider-specific value (a \
         Modal GPU class like `A10G`, or a RunPod `gpuTypeId`), or remove the key to use the \
         recipe default"
    )]
    RemoteGpuEmpty,
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
        "[llm.dream.remote] requires a non-empty `model` (the server-side chat model name — an \
         ollama model like `qwen3:8b`, or the HuggingFace id vLLM was launched with, e.g. \
         `Qwen/Qwen3-4B-Instruct-2507`)"
    )]
    DreamRemoteMissingModel,
    #[error(
        "[llm.dream.remote] `backend = \"{0}\"` cannot serve chat completions — dream needs a \
         chat-capable backend (`ollama` or `vllm`); `infinity` is embed-only. Switch the backend, \
         or point the dream model at an ollama/vLLM server"
    )]
    DreamBackendCannotServeChat(String),
    #[error(
        "[llm.dream.remote] requires EXACTLY ONE of `endpoint` (connect to a running chat server) \
         or `cookbook` (provision an ephemeral box) — set neither both nor zero"
    )]
    DreamRemoteModeAmbiguous,
    #[error(
        "[llm.dream.remote] `endpoint` must not embed credentials in the URL (no \
         `user:pass@host`) — put any token in an env var and name it via `auth_env` instead"
    )]
    DreamRemoteEndpointHasCredentials,
    #[error(
        "[llm.dream.remote] `gpu` applies only to ephemeral `cookbook` provisioning, not a \
         connect `endpoint` — remove `gpu`, or switch the remote block to a `cookbook` recipe"
    )]
    DreamRemoteGpuRequiresCookbook,
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
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    // Test-only: forward-slashes a real filesystem path so it can be embedded in a
    // double-quoted TOML string value without Windows `\U`/`\R` invalid-escape parse
    // errors.
    use path_slash::PathExt;

    use super::*;

    static CFG_TEMP: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn config_load_resolves_main_and_linked_worktrees_to_one_database() {
        // The actual guarantee (review item 1): Config::load from the main worktree and from a
        // linked worktree of the same repo produce the *same* database path — not two DBs.
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-cfgload-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n[target_bindings]\nrust \
             = [\"src\"]\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

        let from_main = Config::load(main.join("rag-rat.toml")).unwrap();
        let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
        assert_eq!(
            from_main.database, from_linked.database,
            "main and linked worktrees must share one index database",
        );
        assert_eq!(from_main.database, main.canonicalize().unwrap().join(".rag-rat/index.sqlite"));
        // AND the `root` anchors to the main worktree from either launch point — so every process
        // uses the same base commit for the shared index, instead of a worktree-launched one
        // rooting at the worktree (a different base → conflicting overlay writes /
        // readable-vs-tombstone races) (#218/#219).
        assert_eq!(from_main.root, from_linked.root, "main and linked configs resolve to one root");
        assert_eq!(
            from_linked.root,
            main.canonicalize().unwrap(),
            "a linked worktree's config root anchors to the main worktree",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repo_id_override_is_parsed_and_does_not_change_the_database_path() {
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-repoid-{}-{id}", std::process::id()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            tmp.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\nrepo_id = \"  pinned-id  \
             \"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();

        let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
        assert_eq!(
            config.repo_id_override.as_deref(),
            Some("pinned-id"),
            "the [index] repo_id override is parsed and trimmed",
        );
        // Parse-only: the override must NOT influence path resolution — the explicit database stays
        // at the per-repo path beside `root`.
        assert_eq!(config.database, config.root.join(".rag-rat/index.sqlite"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Seed a minimal COMMITTED git repo at `dir` — the identity-bearing fixture the global
    /// default requires (a keyless config resolves globally only for a root with a derivable repo
    /// identity).
    fn git_commit_all(dir: &Path) {
        let git = |args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@e"]);
        git(&["config", "user.name", "t"]);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "seed"]);
    }

    /// A7 default flip: a keyless config in an IDENTITY-BEARING repo (a committed git root) with
    /// no legacy `.rag-rat/index.sqlite` resolves to the consolidated GLOBAL store. Compared
    /// against `global_database_path()` in the CURRENT environment (no env mutation ⇒ no
    /// cross-test race); `Config::load` only RESOLVES the path, it never opens or creates the DB,
    /// so this never touches a developer's real global store.
    #[test]
    fn config_load_without_a_database_key_resolves_to_the_global_database() {
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-globaldb-{}-{id}", std::process::id()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            tmp.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        git_commit_all(&tmp);

        let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
        let expected = crate::data_dir::global_database_path()
            .expect("a data dir resolves in the test environment (HOME is set)");
        assert_eq!(
            config.database, expected,
            "a keyless config defaults to the consolidated global database",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The GOVERNING SEAM: in a linked worktree the MAIN config governs the WHOLE config, not a
    /// per-key subset — a divergent branch-local file cannot fork the embedding model (or any
    /// other key) even though no per-key anchoring was ever written for `[llm]`. The two loads
    /// must produce the SAME resolved `Config`.
    #[test]
    fn config_load_in_a_linked_worktree_is_governed_wholesale_by_the_main_config() {
        let git = |dir: &Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-wholecfg-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \"main.sqlite\"\n[watch]\ndebounce_ms = \
             1111\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@e"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

        // The branch config diverges on a key with NO historical per-key anchoring: `[watch]`.
        std::fs::write(
            linked.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \"branch.sqlite\"\n[watch]\ndebounce_ms = \
             9999\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        let from_main = Config::load(main.join("rag-rat.toml")).unwrap();
        let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
        assert_eq!(
            from_linked.watch.debounce_ms, from_main.watch.debounce_ms,
            "the divergent branch config is IGNORED wholesale — keys with no per-key anchoring \
             history included",
        );
        assert_eq!(from_linked.watch.debounce_ms, 1111, "main's value, not the branch's 9999");
        assert_eq!(from_linked.database, from_main.database);
        assert_eq!(from_linked.root, from_main.root);
        assert_eq!(from_linked.targets, from_main.targets);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Config-less-main fallback posture: main is resolvable but has NO `rag-rat.toml`, so the
    /// linked worktree's local config governs best-effort (with a warning) — root still anchors
    /// to main so the shared index keys off one base checkout.
    #[test]
    fn config_load_falls_back_to_the_local_config_when_main_has_none() {
        let git = |dir: &Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("ragrat-nomaincfg-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@e"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

        // Only the LINKED checkout has a config (e.g. authored on a branch, not yet merged).
        std::fs::write(
            linked.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \"branch.sqlite\"\n[target_bindings]\nrust = \
             [\"src\"]\n",
        )
        .unwrap();
        let cfg = Config::load(linked.join("rag-rat.toml")).unwrap();
        let canonical_main = main.canonicalize().unwrap();
        assert_eq!(cfg.root, canonical_main, "root anchors to main even on the fallback");
        assert_eq!(
            cfg.database,
            canonical_main.join("branch.sqlite"),
            "the local key governs (resolved against the main top) until main gains a config",
        );
        assert!(cfg.database_key_pinned);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The DISCOVERY resolver matrix (Codex batch 9): local file wins wherever it exists (the
    /// seam then governs + warns), a linked checkout without one resolves to MAIN's path (even
    /// when that file doesn't exist yet — hints must name where the config belongs), and
    /// main/non-git checkouts stay local.
    #[test]
    fn discover_config_path_resolves_the_governing_checkout() {
        let git = |dir: &Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-discover-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@e"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
        let main_c = main.canonicalize().unwrap();

        // Linked, no local file, main config not yet written: MAIN's path (where it belongs).
        assert_eq!(discover_config_path(&linked), main_c.join("rag-rat.toml"));
        // Main checkout: always local, present or not.
        assert_eq!(discover_config_path(&main), main.join("rag-rat.toml"));
        // Linked WITH a local (divergent) file: the local path — the load then routes through
        // the governing seam, which warns; discovery must not silently skip that.
        std::fs::write(linked.join("rag-rat.toml"), "[index]\nroot = \".\"\n").unwrap();
        assert_eq!(discover_config_path(&linked), linked.join("rag-rat.toml"));
        // Non-git: local.
        let plain = tmp.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(discover_config_path(&plain), plain.join("rag-rat.toml"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The ANCESTOR-WALK arm (non-worktree): a launch from a SUBDIRECTORY of a rag-rat repo
    /// resolves to the repo root's `rag-rat.toml` instead of dying at the local existence
    /// check, while a genuinely config-less tree still yields the local (non-existent) path for
    /// the hint. Also guards the relative-path footgun the walk fixed:
    /// `nearest_config_at_or_above` must resolve a `.`-style dir to ABSOLUTE before climbing —
    /// a relative `parent()` is `Some("")` then `None`, so the walk would never leave the
    /// starting dir.
    #[test]
    fn discover_config_path_walks_up_to_a_parent_repo_config() {
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-walkup-{}-{id}", std::process::id()));
        let repo = tmp.join("repo");
        let nested = repo.join("crates").join("cli").join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(repo.join("rag-rat.toml"), "[index]\nroot = \".\"\n").unwrap();

        // The walk returns a canonical absolute path (a found file), so compare canonically — temp
        // roots can be symlinked (macOS `/tmp` → `/private/tmp`).
        let want = repo.join("rag-rat.toml").canonicalize().unwrap();
        assert_eq!(
            discover_config_path(&nested).canonicalize().unwrap(),
            want,
            "subdir → repo cfg"
        );
        assert_eq!(discover_config_path(&repo).canonicalize().unwrap(), want, "repo root → local");

        // A config-less tree with NO ancestor config: the local (non-existent) path, unchanged —
        // the not-found fallback returns the original `dir/rag-rat.toml` for the hint, uncanonical.
        let bare = tmp.join("bare").join("deep");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(discover_config_path(&bare), bare.join("rag-rat.toml"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The linked-ness PRIMITIVE (Codex batch 8, findings 1+3): topology-derived — the discovered
    /// checkout's workdir vs the designated main — so a SUBDIRECTORY of the main worktree is NOT
    /// linked (pre-fix, `init` from `main/src` falsely refused), while any path inside a linked
    /// checkout (its top OR a subdir) is.
    #[test]
    fn linked_worktree_main_root_derives_linkedness_from_topology() {
        let git = |dir: &Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-linkpred-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@e"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
        let main_c = main.canonicalize().unwrap();

        assert_eq!(linked_worktree_main_root(&main), None, "the main worktree is not linked");
        assert_eq!(
            linked_worktree_main_root(&main.join("src")),
            None,
            "a SUBDIRECTORY of main is main — not linked (the false-refusal bug)",
        );
        assert_eq!(linked_worktree_main_root(&linked), Some(main_c.clone()));
        assert_eq!(
            linked_worktree_main_root(&linked.join("src")),
            Some(main_c),
            "a subdir of a linked checkout is still linked",
        );
        let plain = tmp.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(linked_worktree_main_root(&plain), None, "non-git has no designated main");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Validation ORDERING (Codex batch 8, finding 2): the governing config is chosen FIRST; hard
    /// validation applies only to the config actually used. A branch-local file that fails to
    /// parse (or trips the `[local_ai]` rejection) in a linked worktree folds into the divergence
    /// warning — it must never make every command from the linked checkout fatal, because its
    /// contents are irrelevant by design when main governs.
    #[test]
    fn config_load_ignores_an_invalid_branch_config_when_main_governs() {
        let git = |dir: &Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-brokecfg-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \"main.sqlite\"\n[target_bindings]\nrust = \
             [\"src\"]\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@e"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

        // Unparseable garbage on the branch: main still governs.
        std::fs::write(linked.join("rag-rat.toml"), "this is [not toml").unwrap();
        let cfg = Config::load(linked.join("rag-rat.toml"))
            .expect("a broken branch config is ignored when main governs");
        let main_c = main.canonicalize().unwrap();
        assert_eq!(cfg.database, main_c.join("main.sqlite"));

        // The deprecated `[local_ai]` table on the branch: same posture (it is a VALIDATION
        // failure, not a parse failure — both fold into the warning).
        std::fs::write(
            linked.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[local_ai]\nmodel = \"x\"\n[target_bindings]\nrust = \
             [\"src\"]\n",
        )
        .unwrap();
        let cfg = Config::load(linked.join("rag-rat.toml")).unwrap();
        assert_eq!(cfg.database, main_c.join("main.sqlite"));

        // In the checkout that GOVERNS (main), the same brokenness stays fatal.
        std::fs::write(main.join("rag-rat.toml"), "this is [not toml").unwrap();
        assert!(
            Config::load(main.join("rag-rat.toml")).is_err(),
            "the governing config's validation is fatal as always",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The seam's trigger is TOPOLOGY, not the root-anchoring proxy (Codex batch 8, finding 3): a
    /// branch-only `[index] root` makes `anchor_root_to_main_worktree` keep the local root (the
    /// dir doesn't exist in main), which under the old `anchored != local` trigger concluded
    /// "not linked" and let the branch config govern database/watch/models — the exact
    /// split-brain the seam prevents. Governance must be unconditional on linked-ness.
    #[test]
    fn config_load_governs_from_main_even_when_a_branch_only_root_defeats_anchoring() {
        let git = |dir: &Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("ragrat-branchroot-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \"main.sqlite\"\n[watch]\ndebounce_ms = \
             1111\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@e"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

        // The branch config points `[index] root` at a dir that exists ONLY on the branch —
        // anchoring keeps the local root (missing in main), defeating the old equality proxy.
        std::fs::create_dir_all(linked.join("branch_only/src")).unwrap();
        std::fs::write(linked.join("branch_only/src/lib.rs"), "pub fn b() {}\n").unwrap();
        assert!(!main.join("branch_only").exists(), "main never had this dir");
        std::fs::write(
            linked.join("rag-rat.toml"),
            "[index]\nroot = \"branch_only\"\ndatabase = \"branch.sqlite\"\n[watch]\ndebounce_ms \
             = 9999\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        let cfg = Config::load(linked.join("rag-rat.toml")).unwrap();
        let main_c = main.canonicalize().unwrap();
        assert_eq!(
            cfg.database,
            main_c.join("main.sqlite"),
            "main's database governs — the branch-only root cannot defeat the seam",
        );
        assert_eq!(cfg.watch.debounce_ms, 1111, "main's watch config governs too");
        assert_eq!(cfg.root, main_c, "root comes from MAIN's config when main governs");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The identity gate's SECOND entrance (Codex batch 8, finding 5a): an EXPLICIT pin at the
    /// consolidated global store from an identity-less root is refused at resolution — the
    /// keyless gate never sees a pinned config, and letting it open the shared store would land
    /// this project on adoption's sole-repo pick (a SIBLING repo). A `repo_id` pin restores the
    /// identity and lifts the refusal. Compares against `global_database_path()` in the CURRENT
    /// environment (no env mutation ⇒ parallel-safe); `load` only resolves, never writes there.
    #[test]
    fn config_load_refuses_an_identity_less_pin_at_the_global_store() {
        let Some(global) = crate::data_dir::global_database_path() else {
            return; // no resolvable data dir on this platform — the gate cannot trigger
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-globpin-{}-{id}", std::process::id()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        let config_path = tmp.join("rag-rat.toml");
        std::fs::write(
            &config_path,
            format!(
                "[index]\nroot = \".\"\ndatabase = \"{}\"\n[target_bindings]\nrust = [\"src\"]\n",
                // Forward-slash (path-slash): a Windows `C:\…` path has invalid TOML escapes
                // (`\U`, …); `/` is TOML-safe and `Path` treats the separators as equivalent
                // there.
                global.to_slash_lossy()
            ),
        )
        .unwrap();
        let err = Config::load(&config_path).expect_err("identity-less global pin is refused");
        assert!(
            matches!(err, ConfigError::GlobalPinWithoutIdentity),
            "the refusal names the remedy: {err}",
        );

        // A `repo_id` pin IS a resolvable identity — the same config with one loads fine.
        std::fs::write(
            &config_path,
            format!(
                "[index]\nroot = \".\"\nrepo_id = \"pinned-project\"\ndatabase = \
                 \"{}\"\n[target_bindings]\nrust = [\"src\"]\n",
                // Forward-slash (path-slash): a Windows `C:\…` path has invalid TOML escapes
                // (`\U`, …); `/` is TOML-safe and `Path` treats the separators as equivalent
                // there.
                global.to_slash_lossy()
            ),
        )
        .unwrap();
        let cfg = Config::load(&config_path).expect("a repo_id pin lifts the refusal");
        assert_eq!(cfg.database, global);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The `database` decision is MAIN-WORKTREE-ANCHORED (Codex batch 7): a linked worktree's
    /// branch-local config can neither UN-PIN (a branch toml omitting the key while main pins —
    /// pre-fix that split the repo across the global store and main's per-repo file) nor RE-PIN
    /// (a branch adding its own key) the repo's database. Main's config is authoritative, exactly
    /// as it is for `repo_id`.
    #[test]
    fn config_load_anchors_the_database_key_to_the_main_worktree() {
        let git = |dir: &Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-dbanchor-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        // MAIN pins an explicit per-repo database.
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \"custom/pinned.sqlite\"\n[target_bindings]\nrust \
             = [\"src\"]\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@e"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

        // The BRANCH config omits the key (a branch predating the pin): pre-fix the keyless
        // default resolved the linked checkout to the GLOBAL store — a different DB than main's.
        std::fs::write(
            linked.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        let from_main = Config::load(main.join("rag-rat.toml")).unwrap();
        let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
        assert_eq!(
            from_linked.database, from_main.database,
            "a branch omitting the key must not divert the linked worktree off main's pin",
        );
        assert!(from_linked.database_key_pinned, "the GOVERNING (main) key decision travels too");

        // The BRANCH config pinning its OWN key: main (keyless here) stays authoritative — a
        // branch cannot fork the repo onto a private database.
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        std::fs::write(
            linked.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \"branch/fork.sqlite\"\n[target_bindings]\nrust = \
             [\"src\"]\n",
        )
        .unwrap();
        let from_main = Config::load(main.join("rag-rat.toml")).unwrap();
        let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
        assert_eq!(
            from_linked.database, from_main.database,
            "a branch-local pin must not fork the repo onto its own database",
        );
        assert!(!from_linked.database_key_pinned, "main keyless ⇒ governing decision is keyless");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A7 legacy interplay: a keyless config in a repo that ALREADY has a `.rag-rat/index.sqlite`
    /// (indexed before the flip, or a fresh `rag-rat init` over an old checkout) keeps resolving to
    /// that legacy file — never silently abandoning its memories — until `rag-rat consolidate`
    /// imports and renames it, after which resolution falls through to the global store.
    #[test]
    fn config_load_without_a_database_key_prefers_an_existing_legacy_index() {
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-legacydb-{}-{id}", std::process::id()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::create_dir_all(tmp.join(".rag-rat")).unwrap();
        std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(tmp.join(".rag-rat/index.sqlite"), b"legacy").unwrap();
        std::fs::write(
            tmp.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        git_commit_all(&tmp);

        let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
        assert_eq!(
            config.database,
            tmp.canonicalize().unwrap().join(".rag-rat/index.sqlite"),
            "a pre-existing legacy index wins over the global default until consolidated",
        );

        // Once consolidated (the legacy file renamed away), the same config resolves globally.
        std::fs::rename(
            tmp.join(".rag-rat/index.sqlite"),
            tmp.join(".rag-rat/index.sqlite.imported"),
        )
        .unwrap();
        let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
        assert_eq!(
            config.database,
            crate::data_dir::global_database_path().expect("data dir resolves"),
            "after consolidation the keyless config falls through to the global store",
        );

        // The `.imported` marker is a STAY-GLOBAL LATCH: a stray legacy file REAPPEARING beside it
        // (an old binary, a restored backup) must not silently divert the repo off the global
        // store its memories were imported into.
        std::fs::write(tmp.join(".rag-rat/index.sqlite"), b"stray").unwrap();
        let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
        assert_eq!(
            config.database,
            crate::data_dir::global_database_path().expect("data dir resolves"),
            "a stray legacy file beside the .imported marker is ignored, not adopted",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The IDENTITY GATE on the global default: a keyless config at a root with NO derivable repo
    /// identity (non-git, or a `git init` with an unborn HEAD) stays on its PER-ROOT legacy path —
    /// in the shared global store every identity-less root would pool under the one
    /// `__unassigned__` placeholder scope, so two fresh non-git projects would see and overwrite
    /// each other's rows, and an unborn repo would strand its placeholder rows once its first
    /// commit mints a real id. Two identity-less roots therefore NEVER share a database.
    #[test]
    fn config_load_without_a_database_key_stays_per_root_for_identity_less_roots() {
        let keyless_config = |tag: &str| {
            let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
            let tmp = std::env::temp_dir()
                .join(format!("ragrat-noident-{tag}-{}-{id}", std::process::id()));
            std::fs::create_dir_all(tmp.join("src")).unwrap();
            std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
            std::fs::write(
                tmp.join("rag-rat.toml"),
                "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
            )
            .unwrap();
            tmp
        };

        // Two NON-GIT roots: each resolves to its OWN per-root legacy path — never the shared
        // global store, and never each other's.
        let a = keyless_config("a");
        let b = keyless_config("b");
        let config_a = Config::load(a.join("rag-rat.toml")).unwrap();
        let config_b = Config::load(b.join("rag-rat.toml")).unwrap();
        assert_eq!(
            config_a.database,
            a.canonicalize().unwrap().join(".rag-rat/index.sqlite"),
            "an identity-less root stays on its per-root legacy path",
        );
        assert_eq!(
            config_b.database,
            b.canonicalize().unwrap().join(".rag-rat/index.sqlite"),
            "each identity-less root gets its own database",
        );
        assert_ne!(config_a.database, config_b.database, "identity-less roots never share scope");

        // An UNBORN repo (`git init`, no commit yet) is identity-less too: it lands per-root, so
        // its placeholder rows adopt IN THAT DB when the first commit mints a real id (the
        // existing single-repo adoption flow), instead of stranding in the global store.
        let unborn = keyless_config("unborn");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&unborn)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        git(&["init", "-q"]);
        let config = Config::load(unborn.join("rag-rat.toml")).unwrap();
        assert_eq!(
            config.database,
            unborn.canonicalize().unwrap().join(".rag-rat/index.sqlite"),
            "an unborn repo stays per-root until its first commit mints an identity",
        );
        // A `[index] repo_id` pin IS an identity: the same root then resolves globally.
        std::fs::write(
            unborn.join("rag-rat.toml"),
            "[index]\nroot = \".\"\nrepo_id = \"pinned-project\"\n[target_bindings]\nrust = \
             [\"src\"]\n",
        )
        .unwrap();
        let config = Config::load(unborn.join("rag-rat.toml")).unwrap();
        assert_eq!(
            config.database,
            crate::data_dir::global_database_path().expect("data dir resolves"),
            "a pinned repo_id makes the root identity-bearing, so the global default applies",
        );

        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
        let _ = std::fs::remove_dir_all(&unborn);
    }

    #[test]
    fn repo_id_override_absent_is_none() {
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("ragrat-repoid-none-{}-{id}", std::process::id()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            tmp.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();

        let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
        assert_eq!(config.repo_id_override, None, "no [index] repo_id → None");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn config_load_in_a_linked_worktree_uses_main_base_targets_not_the_branch() {
        // #219 review: a linked branch can point `rag-rat.toml` at a target dir that exists ONLY in
        // that branch. `Config::load` anchors `root` to the main worktree for the shared base
        // index. Two things must hold: (1) loading the branch config must NOT fail with
        // `MissingDirectory` (the branch-only dir is validated against the linked checkout where it
        // lives); (2) the stored BASE `targets` must come from MAIN's `rag-rat.toml`, not the
        // branch's — otherwise base discovery walks main with the branch target set and tombstones
        // any main file outside it. The branch's extra target is served via the overlay, not the
        // base config.
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("ragrat-cfgbranch-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        // Main's config indexes only `src`.
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n[target_bindings]\nrust \
             = [\"src\"]\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);

        // A branch adds a NEW target dir `extra` and a config that indexes it — committed only on
        // the branch, checked out in the linked worktree.
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
        std::fs::create_dir_all(linked.join("extra")).unwrap();
        std::fs::write(linked.join("extra/more.rs"), "pub fn b() {}\n").unwrap();
        std::fs::write(
            linked.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\", \"extra\"]\n",
        )
        .unwrap();
        git(&linked, &["add", "-A"]);
        git(&linked, &["commit", "-qm", "branch adds extra"]);

        // `extra` does not exist in the main checkout, so validating against main would fail —
        // loading still succeeds because the branch-only dir is validated against the linked
        // checkout where it lives.
        assert!(!main.join("extra").exists(), "the branch-only dir must be absent from main");
        let from_linked = Config::load(linked.join("rag-rat.toml"))
            .expect("loading the branch config in the linked worktree must not fail (req 1)");
        // root still anchors to main (one shared base index).
        assert_eq!(
            from_linked.root,
            main.canonicalize().unwrap(),
            "root anchors to the main worktree for the shared base index",
        );
        // The stored BASE targets come from MAIN's config (`src` only), NOT the branch's
        // (`src` + `extra`): base discovery must not walk main with the branch's target set (req
        // 2).
        let dirs = from_linked.target_directories();
        assert!(dirs.contains(&PathBuf::from("src")), "main's `src` target is the base: {dirs:?}");
        assert!(
            !dirs.contains(&PathBuf::from("extra")),
            "the branch-only target must NOT be a base target (it can't tombstone main): {dirs:?}",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn config_load_in_a_linked_worktree_keeps_main_targets_when_the_branch_narrows_them() {
        // #219 review (3440746682): a linked branch's `rag-rat.toml` that NARROWS the target set
        // (drops a dir that still exists on main) must NOT carry that narrowed set into the BASE
        // config. The base config drives discovery over the anchored (main) root; with the branch's
        // narrowed targets, main-only files would be classified `deleted` and tombstoned in the
        // base scope — hiding committed files from main queries. The stored base targets
        // must be MAIN's.
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("ragrat-cfgnarrow-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::create_dir_all(main.join("extra")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(main.join("extra/more.rs"), "pub fn b() {}\n").unwrap();
        // Main indexes BOTH `src` and `extra`.
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\", \"extra\"]\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);

        // The branch NARROWS to `src` only (drops `extra`), committed on the branch.
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
        std::fs::write(
            linked.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        git(&linked, &["add", "-A"]);
        git(&linked, &["commit", "-qm", "branch narrows to src"]);

        let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
        let dirs = from_linked.target_directories();
        // Both of main's targets survive in the base config, so base discovery still walks `extra`
        // on main and never tombstones `extra/more.rs`.
        assert!(dirs.contains(&PathBuf::from("src")), "base keeps main's `src`: {dirs:?}");
        assert!(
            dirs.contains(&PathBuf::from("extra")),
            "base keeps main's `extra` even though the branch dropped it: {dirs:?}",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn config_load_anchors_repo_id_override_to_main_when_the_branch_diverges() {
        // FINDING 4: repo IDENTITY is per-repo, so the `[index] repo_id` override is read from the
        // MAIN worktree's config, NOT the launching (branch-local) one. A linked worktree that pins
        // a DIFFERENT id must still resolve MAIN's — otherwise identity splits by which checkout
        // launched. This mirrors the root/database/targets anchoring above.
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("ragrat-repoid-anchor-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        // Main pins a canonical repo_id.
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\nrepo_id = \"canonical-id\"\n[target_bindings]\nrust = \
             [\"src\"]\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);

        // The branch pins a DIVERGENT id, committed on the branch and checked out in the worktree.
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
        std::fs::write(
            linked.join("rag-rat.toml"),
            "[index]\nroot = \".\"\nrepo_id = \"branch-divergent-id\"\n[target_bindings]\nrust = \
             [\"src\"]\n",
        )
        .unwrap();
        git(&linked, &["add", "-A"]);
        git(&linked, &["commit", "-qm", "branch pins a different repo_id"]);

        let from_main = Config::load(main.join("rag-rat.toml")).unwrap();
        let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
        assert_eq!(
            from_main.repo_id_override.as_deref(),
            Some("canonical-id"),
            "the main checkout resolves its own override",
        );
        assert_eq!(
            from_linked.repo_id_override.as_deref(),
            Some("canonical-id"),
            "a linked worktree resolves MAIN's repo_id override, not its own branch-local pin",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn config_load_anchors_repo_id_override_to_main_when_main_omits_it() {
        // The strong form of FINDING 4: MAIN omits `[index] repo_id` (identity derives from the
        // root commit), but the branch pins one. The anchored value is MAIN's absence →
        // None, so identity stays derived and launch-point-independent; the branch pin is
        // NOT honored for the shared identity (honoring it would make identity depend on
        // which worktree launched).
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir()
            .join(format!("ragrat-repoid-mainomit-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        // Main OMITS repo_id.
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n[target_bindings]\nrust \
             = [\"src\"]\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);

        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
        std::fs::write(
            linked.join("rag-rat.toml"),
            "[index]\nroot = \".\"\nrepo_id = \"branch-only-id\"\n[target_bindings]\nrust = \
             [\"src\"]\n",
        )
        .unwrap();
        git(&linked, &["add", "-A"]);
        git(&linked, &["commit", "-qm", "branch pins a repo_id main lacks"]);

        let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
        assert_eq!(
            from_linked.repo_id_override, None,
            "main omits the override, so the anchored identity derives — the branch pin is ignored",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #427: a linked worktree's `[index] root` resolves to itself locally, but `Config::load`
    /// re-anchors it to MAIN so every worktree of a repo shares one base index. The PRE-anchor
    /// value (the worktree the operator actually named) would otherwise be lost after anchoring —
    /// capture it so the `index` command can warn instead of silently indexing a different
    /// checkout than the one named.
    #[test]
    fn load_records_the_pre_anchor_root_for_a_linked_worktree() {
        let git = |dir: &Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-reanchor-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            main.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@e"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
        std::fs::write(linked.join("rag-rat.toml"), "[index]\nroot = \".\"\n").unwrap();

        let main_c = main.canonicalize().unwrap();
        let linked_c = linked.canonicalize().unwrap();
        let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
        assert_eq!(from_linked.root, main_c, "root anchors to main (existing behavior)");
        assert_eq!(
            from_linked.source_root_reanchored_from.as_deref(),
            Some(linked_c.as_path()),
            "the pre-anchor (named) linked-worktree root is captured",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The counterpart to the above: loading from a plain (non-worktree) repo redirects nothing,
    /// so the field stays `None`.
    #[test]
    fn load_leaves_reanchor_none_for_the_main_worktree() {
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("ragrat-reanchor-none-{}-{id}", std::process::id()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            tmp.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        git_commit_all(&tmp);

        let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
        assert!(
            config.source_root_reanchored_from.is_none(),
            "no worktree redirection happened, so the field stays None",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cpp_target_renders_h_in_its_default_globs_but_c_keeps_h_too() {
        // The simple-binding glob render goes through `default_include_globs`, so a `cpp` binding
        // includes `**/*.h` (the header-resolution fix) while `c` keeps it as well.
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("ragrat-prec-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nc = [\".\"]\ncpp = [\".\"]\n",
        )
        .unwrap();
        let config = Config::load(root.join("rag-rat.toml")).unwrap();
        let cpp = config.targets.iter().find(|t| t.language == Language::Cpp).unwrap();
        assert!(cpp.include.contains(&"**/*.h".to_string()), "cpp globs: {:?}", cpp.include);
        // cpp must sort ahead of c so it wins the ambiguous `.h` (index_precedence).
        assert!(
            cpp.index_precedence()
                < config
                    .targets
                    .iter()
                    .find(|t| t.language == Language::C)
                    .unwrap()
                    .index_precedence(),
            "cpp must outrank c for the shared .h header"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn anchor_root_preserves_subdir_and_redirects_linked_to_main() {
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-cfg-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("seed.txt"), "x").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
        std::fs::create_dir_all(linked.join("src")).unwrap();

        let main_c = main.canonicalize().unwrap();
        let linked_c = linked.canonicalize().unwrap();

        // Main worktree (any root) resolves to itself.
        assert_eq!(anchor_root_to_main_worktree(&main_c), main_c);
        // A SUBDIR root on the main worktree is PRESERVED (not collapsed to the repo top) — the
        // #219-review regression: collapsing changed the indexed file set + failed config load.
        assert_eq!(anchor_root_to_main_worktree(&main_c.join("src")), main_c.join("src"));
        // Linked worktree, root=".", redirects to the main worktree → one shared base.
        assert_eq!(anchor_root_to_main_worktree(&linked_c), main_c);
        // Linked worktree SUBDIR root rebases under the main worktree, subdir preserved.
        assert_eq!(anchor_root_to_main_worktree(&linked_c.join("src")), main_c.join("src"));

        // A non-git directory falls back to itself.
        let plain = tmp.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let plain_c = plain.canonicalize().unwrap();
        assert_eq!(anchor_root_to_main_worktree(&plain_c), plain_c);

        // A linked-worktree subdir root that does NOT exist in main must NOT anchor to a missing
        // `main/<rel>` path (#219 review): the branch created `branch_only/`, which main never had.
        // The anchored `main/branch_only` doesn't exist, so resolution keeps the linked checkout's
        // (existing) root — otherwise `Config.root` would point outside any discoverable repo path.
        let branch_only = linked.join("branch_only");
        std::fs::create_dir_all(&branch_only).unwrap();
        let branch_only_c = branch_only.canonicalize().unwrap();
        assert!(!main_c.join("branch_only").exists(), "main never had this dir");
        assert_eq!(
            anchor_root_to_main_worktree(&branch_only_c),
            branch_only_c,
            "a branch-only root that's missing in main keeps the linked checkout's root",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parses_simple_and_expanded_targets() {
        let root = std::env::current_dir().unwrap();
        let simple = BTreeMap::from([("rust".to_string(), vec![".".to_string()])]);
        let expanded = vec![RawTarget {
            name: "generated-ts".to_string(),
            language: "typescript".to_string(),
            directories: vec![".".to_string()],
            kind: Some("generated".to_string()),
            include: Some(vec!["**/*.ts".to_string()]),
            exclude: Some(vec!["**/*.map".to_string()]),
        }];

        let targets = resolve_targets(&root, simple, expanded).unwrap();

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].language, Language::Rust);
        assert_eq!(targets[1].kind, TargetKind::Generated);
    }

    #[test]
    fn embedding_runtime_defaults_match_local_profile() {
        let runtime = EmbeddingRuntimeConfig::default();

        assert_eq!(runtime.batch_size, 64);
        assert_eq!(runtime.ort_threads, Some(4));
        assert_eq!(runtime.omp_threads, Some(1));
        assert_eq!(runtime.max_embedding_chars, 4000);
    }

    #[test]
    fn parses_embedding_runtime_overrides() {
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."
            database = ".rag-rat/index.sqlite"

            [llm.embedding.runtime]
            batch_size = 128
            ort_threads = 2
            omp_threads = 1
            max_embedding_chars = 5000
            "#,
        )
        .unwrap();

        let llm = LlmConfig::try_from(raw.llm).unwrap();

        assert_eq!(llm.embedding.runtime, EmbeddingRuntimeConfig {
            batch_size: 128,
            ort_threads: Some(2),
            omp_threads: Some(1),
            max_embedding_chars: 5000,
        });
    }

    #[test]
    fn remote_embedding_absent_is_none() {
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"
            "#,
        )
        .unwrap();
        let llm = LlmConfig::try_from(raw.llm).unwrap();
        assert_eq!(llm.embedding.remote, None, "no [remote] block → remote: None");
    }

    #[test]
    fn remote_embedding_connect_happy_path_applies_defaults() {
        // CONNECT is inferred from `endpoint` being set (#318) — no `mode` field. The selector
        // names a real MODEL; the [remote] block serves it via Ollama.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            "#,
        )
        .unwrap();
        let llm = LlmConfig::try_from(raw.llm).unwrap();
        assert_eq!(
            llm.embedding.remote,
            Some(RemoteEmbeddingConfig {
                model: "all-minilm".to_string(),
                backend: RemoteBackend::Ollama,
                endpoint: Some("http://localhost:11434".to_string()),
                cookbook: None,
                query_endpoint: None, // connect mode: no local query box
                auth_env: None,
                gpu: None,
                num_ctx: None,
                // defaults applied when omitted
                batch_size: 256,
                concurrency: 1,
                max_batch_chars: 384_000,
                request_timeout_s: 60,
            })
        );
        let remote = llm.embedding.remote.as_ref().unwrap();
        assert!(remote.is_connect() && !remote.is_ephemeral());
        // The selector still resolves to the LOCAL fastembed model — the [remote] block overrides
        // the RUNTIME, not the model identity.
        assert_eq!(
            llm.embedding.backend.model_id(),
            Some(crate::embedding_models::FASTEMBED_MODEL_ID)
        );
    }

    #[test]
    fn remote_embedding_ephemeral_infers_mode_and_defaults_query_endpoint() {
        // EPHEMERAL is inferred from `cookbook` being set; `query_endpoint` defaults to the local
        // Ollama when omitted (queries embed the same model → same vector space as remote chunks).
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "@rag-rat/cookbook/modal"
            "#,
        )
        .unwrap();
        let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
        assert!(remote.is_ephemeral() && !remote.is_connect());
        assert_eq!(remote.cookbook.as_deref(), Some("@rag-rat/cookbook/modal"));
        assert_eq!(remote.endpoint, None);
        assert_eq!(remote.query_endpoint.as_deref(), Some(DEFAULT_QUERY_ENDPOINT));
        assert_eq!(remote.concurrency, 32);
    }

    #[test]
    fn remote_embedding_ephemeral_honors_explicit_query_endpoint() {
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "./recipe.mjs"
            query_endpoint = "http://127.0.0.1:11999"
            "#,
        )
        .unwrap();
        let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
        assert_eq!(remote.query_endpoint.as_deref(), Some("http://127.0.0.1:11999"));
    }

    #[test]
    fn ephemeral_non_ollama_backend_requires_an_explicit_query_endpoint() {
        // The DEFAULT_QUERY_ENDPOINT is a local OLLAMA URL; it only fits `backend = ollama`. A
        // non-ollama ephemeral backend that omits `query_endpoint` must be REJECTED (not silently
        // defaulted), or after teardown queries embed against local Ollama with the wrong route /
        // model → silent BM25 fallback. See `RemoteQueryEndpointRequiredForBackend`.
        let build = |backend: &str, query_line: &str| {
            let raw: RawConfig = toml::from_str(&format!(
                r#"
                [index]
                root = "."

                [llm.embedding]
                model = "sentence-transformers/all-MiniLM-L6-v2"

                [llm.embedding.remote]
                model = "sentence-transformers/all-MiniLM-L6-v2"
                backend = "{backend}"
                cookbook = "@rag-rat/cookbook modal"
                {query_line}
                "#,
            ))
            .unwrap();
            LlmConfig::try_from(raw.llm).map(|l| l.embedding.remote.unwrap())
        };

        for backend in ["infinity", "vllm"] {
            let err = build(backend, "").unwrap_err();
            assert!(
                matches!(
                    err,
                    ConfigError::RemoteQueryEndpointRequiredForBackend { backend: b } if b == backend
                ),
                "{backend} ephemeral without query_endpoint → \
                 RemoteQueryEndpointRequiredForBackend, got {err:?}",
            );
            // An explicit query_endpoint is accepted, and the backend is preserved for the query
            // path.
            let remote = build(backend, r#"query_endpoint = "http://127.0.0.1:7997""#).unwrap();
            assert_eq!(remote.query_endpoint.as_deref(), Some("http://127.0.0.1:7997"));
            assert_eq!(remote.backend.as_db_str(), backend);
        }
        // ollama still defaults (its default IS a local Ollama).
        assert_eq!(
            build("ollama", "").unwrap().query_endpoint.as_deref(),
            Some(DEFAULT_QUERY_ENDPOINT),
        );
    }

    #[test]
    fn remote_embedding_ephemeral_gpu_is_parsed_and_trimmed() {
        // EPHEMERAL: `gpu` picks the GPU the cookbook recipe provisions. The value is
        // provider-specific (Modal class / RunPod gpuTypeId) and NOT validated against an
        // allow-list here — only trimmed.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "@rag-rat/cookbook/modal"
            gpu = "  A10G  "
            "#,
        )
        .unwrap();
        let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
        assert_eq!(remote.gpu.as_deref(), Some("A10G"));
    }

    #[test]
    fn remote_embedding_gpu_with_connect_endpoint_is_rejected() {
        // `gpu` only applies to ephemeral `cookbook` provisioning. Set alongside a connect
        // `endpoint` it is meaningless → rejected (not silently ignored).
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            gpu = "A10G"
            "#,
        )
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteGpuRequiresCookbook),
            "gpu + endpoint → RemoteGpuRequiresCookbook, got {err:?}",
        );
    }

    #[test]
    fn remote_embedding_empty_gpu_is_rejected() {
        // A present-but-empty/whitespace `gpu` is a config error — clearer than silently dropping a
        // key the user meant to set. (Omitting `gpu` entirely is fine: the recipe uses its
        // default.)
        for value in ["\"\"", "\"   \""] {
            let raw: RawConfig = toml::from_str(&format!(
                r#"
                [index]
                root = "."

                [llm.embedding]
                model = "sentence-transformers/all-MiniLM-L6-v2"

                [llm.embedding.remote]
                model = "all-minilm"
                cookbook = "@rag-rat/cookbook/modal"
                gpu = {value}
                "#,
            ))
            .unwrap();
            let err = LlmConfig::try_from(raw.llm).unwrap_err();
            assert!(
                matches!(err, ConfigError::RemoteGpuEmpty),
                "gpu={value} → RemoteGpuEmpty, got {err:?}",
            );
        }
    }

    #[test]
    fn remote_embedding_overrides_batch_and_timeout() {
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            auth_env = "OLLAMA_TOKEN"
            num_ctx = 4096
            batch_size = 512
            concurrency = 16
            max_batch_chars = 128000
            request_timeout_s = 120
            "#,
        )
        .unwrap();
        let llm = LlmConfig::try_from(raw.llm).unwrap();
        assert_eq!(
            llm.embedding.remote,
            Some(RemoteEmbeddingConfig {
                model: "all-minilm".to_string(),
                backend: RemoteBackend::Ollama,
                endpoint: Some("http://localhost:11434".to_string()),
                cookbook: None,
                query_endpoint: None,
                auth_env: Some("OLLAMA_TOKEN".to_string()),
                gpu: None,
                num_ctx: Some(4096),
                batch_size: 512,
                concurrency: 16,
                max_batch_chars: 128_000,
                request_timeout_s: 120,
            })
        );
    }

    #[test]
    fn remote_embedding_zero_concurrency_and_char_budget_are_clamped() {
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            concurrency = 0
            max_batch_chars = 0
            "#,
        )
        .unwrap();
        let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
        assert_eq!(remote.concurrency, 1);
        assert_eq!(remote.max_batch_chars, 1);
    }

    #[test]
    fn remote_embedding_rejects_oversized_concurrency() {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            concurrency = {}
            "#,
            MAX_REMOTE_EMBEDDING_CONCURRENCY + 1
        ))
        .unwrap();

        let err = LlmConfig::try_from(raw.llm).expect_err("oversized concurrency should reject");
        assert!(matches!(
            err,
            ConfigError::RemoteEmbeddingConcurrencyTooHigh {
                value,
                max: MAX_REMOTE_EMBEDDING_CONCURRENCY
            } if value == MAX_REMOTE_EMBEDDING_CONCURRENCY + 1
        ));
    }

    #[test]
    fn older_remote_embedding_meta_json_deserializes_with_legacy_safe_defaults() {
        let json = r#"{
            "model": "all-minilm",
            "endpoint": "http://localhost:11434",
            "cookbook": null,
            "query_endpoint": null,
            "auth_env": null,
            "gpu": null,
            "num_ctx": null,
            "batch_size": 256,
            "request_timeout_s": 60
        }"#;
        let remote: RemoteEmbeddingConfig = serde_json::from_str(json).unwrap();
        assert_eq!(remote.concurrency, 1);
        assert_eq!(remote.max_batch_chars, 384_000);
    }

    #[test]
    fn remote_embedding_requires_exactly_one_of_endpoint_or_cookbook() {
        // Neither → no server to reach; both → ambiguous mode. Both reject with the exactly-one
        // rule.
        let neither = r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            "#;
        let both = r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            cookbook = "@rag-rat/cookbook/modal"
            "#;
        for (label, toml_str) in [("neither", neither), ("both", both)] {
            let raw: RawConfig = toml::from_str(toml_str).unwrap();
            let err = LlmConfig::try_from(raw.llm).unwrap_err();
            assert!(
                matches!(err, ConfigError::RemoteEmbeddingModeAmbiguous),
                "{label} endpoint/cookbook → RemoteEmbeddingModeAmbiguous, got {err:?}",
            );
        }
    }

    #[test]
    fn remote_embedding_endpoint_with_credentials_is_rejected() {
        // The endpoint is persisted whole into the index meta, so a `user:token@host` URL would
        // copy the credential into the index. Reject it and direct the user to `auth_env`.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "https://user:token@host:11434"
            "#,
        )
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteEmbeddingEndpointHasCredentials),
            "endpoint with userinfo → RemoteEmbeddingEndpointHasCredentials, got {err:?}",
        );
    }

    #[test]
    fn remote_embedding_endpoint_without_credentials_is_accepted() {
        // A plain host and a loopback endpoint both pass the userinfo guard (and an `@` in a path
        // is not userinfo).
        for endpoint in [
            "https://host:11434",
            "http://127.0.0.1:11434",
            "http://localhost:11434/v1/embeddings?user=a@b",
        ] {
            let raw: RawConfig = toml::from_str(&format!(
                "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \
                 \"sentence-transformers/all-MiniLM-L6-v2\"\n\n[llm.embedding.remote]\nmodel = \
                 \"all-minilm\"\nendpoint = \"{endpoint}\"\n"
            ))
            .unwrap();
            let remote = LlmConfig::try_from(raw.llm)
                .unwrap_or_else(|e| panic!("`{endpoint}` must be accepted: {e:?}"))
                .embedding
                .remote
                .expect("remote block present");
            assert_eq!(remote.endpoint.as_deref(), Some(endpoint));
        }
    }

    #[test]
    fn endpoint_authority_has_userinfo_classifies_urls() {
        assert!(endpoint_authority_has_userinfo("https://user:token@host:11434"));
        assert!(endpoint_authority_has_userinfo("http://u@127.0.0.1"));
        assert!(!endpoint_authority_has_userinfo("https://host:11434"));
        assert!(!endpoint_authority_has_userinfo("http://127.0.0.1:11434"));
        // An `@` in the PATH/query is not userinfo.
        assert!(!endpoint_authority_has_userinfo("http://host:11434/path?x=a@b"));
    }

    #[test]
    fn resolve_relative_cookbook_path_anchors_relative_recipe_paths_to_config_dir() {
        let dir = Path::new("/repo/sub");

        // A path-shaped spec resolves its FIRST token against `config_dir` and preserves any
        // trailing provider args verbatim. The resolved token carries the platform-NATIVE
        // separator (`\` on Windows), so assert it as a `Path`, not a `String`: `Path`
        // equality is separator-agnostic on Windows and normalizes a mid-path `.` on every
        // OS, so one assertion holds cross-platform without hardcoding a separator
        // rendering.
        let anchored = |spec: &str| -> (PathBuf, String) {
            let out = resolve_relative_cookbook_path(spec, dir).expect("path-shaped spec resolves");
            match out.split_once(' ') {
                Some((path, rest)) => (PathBuf::from(path), rest.to_string()),
                None => (PathBuf::from(out), String::new()),
            }
        };

        let (path, rest) = anchored("./recipes/x.mts");
        assert_eq!(path, dir.join("./recipes/x.mts"));
        assert_eq!(rest, "");

        let (path, rest) = anchored("../cookbook.mjs modal");
        assert_eq!(path, dir.join("../cookbook.mjs"));
        assert_eq!(rest, "modal");

        // A bare relative `.ts`/`.mts`/`.js` path (no `./`) is still path-shaped → resolved.
        let (path, rest) = anchored("recipe.mts");
        assert_eq!(path, dir.join("recipe.mts"));
        assert_eq!(rest, "");

        // npm package specs and a bare token are LEFT VERBATIM (None).
        assert_eq!(resolve_relative_cookbook_path("@rag-rat/cookbook modal", dir), None);
        assert_eq!(resolve_relative_cookbook_path("some-pkg", dir), None);
        // An ALREADY-ABSOLUTE recipe path is left verbatim (None). Use a platform-absolute path: a
        // bare `/abs/...` is NOT absolute on Windows (no drive), so it wouldn't reach the
        // absolute-bailout branch there.
        #[cfg(windows)]
        let abs_recipe = r"C:\abs\recipe.mjs runpod";
        #[cfg(not(windows))]
        let abs_recipe = "/abs/recipe.mjs runpod";
        assert_eq!(resolve_relative_cookbook_path(abs_recipe, dir), None);

        // Drive-agnostic on Windows: a NON-C drive anchors the same way (the `E:` prefix survives
        // untouched). An absolute `E:\…` recipe is still left verbatim.
        #[cfg(windows)]
        {
            let out = resolve_relative_cookbook_path("./r/x.mts", Path::new(r"E:\proj"))
                .expect("relative recipe on a non-C drive resolves");
            assert_eq!(PathBuf::from(out), Path::new(r"E:\proj").join("./r/x.mts"));
            assert_eq!(resolve_relative_cookbook_path(r"E:\abs\recipe.mjs", dir), None);
        }
    }

    #[test]
    fn remote_embedding_query_endpoint_with_credentials_is_rejected() {
        // The query_endpoint is persisted too, so userinfo in it is rejected the same as
        // `endpoint`.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "@rag-rat/cookbook/modal"
            query_endpoint = "http://user:tok@127.0.0.1:11434"
            "#,
        )
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteEmbeddingEndpointHasCredentials),
            "query_endpoint with userinfo → RemoteEmbeddingEndpointHasCredentials, got {err:?}",
        );
    }

    #[test]
    fn remote_embedding_missing_model_is_rejected() {
        // The two `model` keys are distinct: `[llm.embedding] model` is the registry SELECTOR;
        // `[remote] model` is the Ollama API model name — it's the latter that's required here.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            endpoint = "http://localhost:11434"
            "#,
        )
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteEmbeddingMissingModel),
            "omitted [remote] model → RemoteEmbeddingMissingModel, got {err:?}",
        );

        // A whitespace-only `[remote] model` trims to empty and is rejected the same way.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "   "
            endpoint = "http://localhost:11434"
            "#,
        )
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteEmbeddingMissingModel),
            "whitespace-only [remote] model → RemoteEmbeddingMissingModel, got {err:?}",
        );
    }

    #[test]
    fn remote_backend_parses_defaults_to_ollama_and_rejects_unknown() {
        let parse = |backend_line: &str| -> Result<RemoteEmbeddingConfig, ConfigError> {
            let raw: RawConfig = toml::from_str(&format!(
                r#"
                [index]
                root = "."

                [llm.embedding]
                model = "sentence-transformers/all-MiniLM-L6-v2"

                [llm.embedding.remote]
                model = "all-minilm"
                endpoint = "http://localhost:11434"
                {backend_line}
                "#
            ))
            .unwrap();
            LlmConfig::try_from(raw.llm).map(|llm| llm.embedding.remote.unwrap())
        };
        // Omitted → ollama (back-compat with pre-selector configs).
        assert_eq!(parse("").unwrap().backend, RemoteBackend::Ollama);
        // Explicit, case-insensitive.
        assert_eq!(parse(r#"backend = "infinity""#).unwrap().backend, RemoteBackend::Infinity);
        assert_eq!(parse(r#"backend = "VLLM""#).unwrap().backend, RemoteBackend::Vllm);
        // Unknown → a clear config error naming the bad value.
        let err = parse(r#"backend = "tgi""#).unwrap_err();
        assert!(matches!(&err, ConfigError::RemoteBackendUnknown(v) if v == "tgi"), "got {err:?}");
    }

    #[test]
    fn remote_backend_db_str_round_trips_and_matches_serde() {
        for b in [RemoteBackend::Ollama, RemoteBackend::Infinity, RemoteBackend::Vllm] {
            assert_eq!(RemoteBackend::from_db_str(b.as_db_str()), Some(b));
            // The serde repr (persisted into the index meta) MUST equal `as_db_str` (the runtime
            // marker + freshness/tune-key discriminator) so the two representations never drift.
            let json = serde_json::to_string(&b).unwrap();
            assert_eq!(json, format!("\"{}\"", b.as_db_str()));
        }
        assert_eq!(RemoteBackend::from_db_str("nope"), None);
    }

    #[test]
    fn remote_backend_embed_path_is_per_backend() {
        // ollama + vLLM expose the OpenAI-standard route; infinity's v2 server serves `/embeddings`
        // (verified live). Same request/response shape — only the path differs.
        assert_eq!(RemoteBackend::Ollama.embed_path(), "/v1/embeddings");
        assert_eq!(RemoteBackend::Vllm.embed_path(), "/v1/embeddings");
        assert_eq!(RemoteBackend::Infinity.embed_path(), "/embeddings");
    }

    #[test]
    fn remote_backend_provision_timeout_is_longer_for_vllm() {
        // vLLM's ~10-15 GB image needs a longer cold-start ceiling than ollama/infinity, or it
        // times out on Modal. ollama/infinity share the shorter default.
        assert_eq!(
            RemoteBackend::Ollama.provision_timeout(),
            RemoteBackend::Infinity.provision_timeout()
        );
        assert!(
            RemoteBackend::Vllm.provision_timeout() > RemoteBackend::Infinity.provision_timeout(),
            "vLLM must get a longer provisioning ceiling than infinity",
        );
    }

    #[test]
    fn remote_block_on_a_non_transformer_model_is_rejected() {
        // #317 rework guardrail: Ollama can only serve transformer models. A [remote] block on the
        // static model2vec, the hash model, or `none` (embeddings disabled) is a misconfiguration —
        // reject at parse with a clear message rather than leaving a remote block that never
        // installs/provisions anything. Selectors are the HF-path model_ids now.
        for model in ["minishlab/potion-retrieval-32M", "embedding-hash", "none"] {
            let raw: RawConfig = toml::from_str(&format!(
                "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \
                 \"{model}\"\n\n[llm.embedding.remote]\nmodel = \"all-minilm\"\nendpoint = \
                 \"http://localhost:11434\"\n"
            ))
            .unwrap();
            let err = LlmConfig::try_from(raw.llm).unwrap_err();
            assert!(
                matches!(err, ConfigError::RemoteEmbeddingNonTransformerModel(_)),
                "remote block + {model} → RemoteEmbeddingNonTransformerModel, got {err:?}",
            );
        }
    }

    #[test]
    fn the_renamed_local_ai_table_is_rejected_with_a_migration_message() {
        // #317 renamed [local_ai] → [llm]. An old config's [local_ai] table must error LOUDLY:
        // serde would otherwise silently DROP it, reverting embedding settings to defaults on
        // upgrade. The error fires in Config::load before any directory resolution.
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-localai-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[local_ai.embedding]\nmodel = \"none\"\n",
        )
        .unwrap();
        let err = Config::load(tmp.join("rag-rat.toml")).unwrap_err();
        assert!(
            matches!(err, ConfigError::LocalAiTableRenamed),
            "[local_ai] table → LocalAiTableRenamed, got {err:?}",
        );
    }

    #[test]
    fn the_legacy_dream_table_is_rejected_with_a_migration_message() {
        // The dream model config moved from [dream.model] → [llm.dream]. An old config's top-level
        // [dream] table must error LOUDLY: serde would otherwise silently DROP it, so an upgrade
        // from `[dream.model] enabled = true` would run the deterministic passes only (never the
        // model). Fires in Config::load before any directory resolution.
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-dream-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[dream.model]\nenabled = true\n",
        )
        .unwrap();
        let err = Config::load(tmp.join("rag-rat.toml")).unwrap_err();
        assert!(
            matches!(err, ConfigError::DreamTableMoved),
            "[dream] table → DreamTableMoved, got {err:?}",
        );
    }

    #[test]
    fn remote_block_with_a_transformer_model_is_accepted() {
        // The inverse of the guardrail: the FastEmbed (transformer) HF-path models accept a
        // [remote] block.
        for model in [
            "sentence-transformers/all-MiniLM-L6-v2",
            "BAAI/bge-small-en-v1.5",
            "jinaai/jina-embeddings-v2-base-code",
        ] {
            let raw: RawConfig = toml::from_str(&format!(
                "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \
                 \"{model}\"\n\n[llm.embedding.remote]\nmodel = \"all-minilm\"\nendpoint = \
                 \"http://localhost:11434\"\n"
            ))
            .unwrap();
            assert!(
                LlmConfig::try_from(raw.llm).is_ok(),
                "remote block + {model} (transformer) must be accepted",
            );
        }
    }

    #[test]
    fn watch_config_defaults_on_and_parses_overrides() {
        let default: WatchConfig = RawWatch::default().into();
        assert!(default.enabled, "watcher is on by default");
        assert_eq!(default.debounce_ms, 400);
        assert_eq!(default.max_latency_ms, 2500);
        assert_eq!(default.periodic_sweep_secs, 300);

        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [watch]
            enabled = false
            debounce_ms = 750
            max_latency_ms = 4000
            periodic_sweep_secs = 0
            "#,
        )
        .unwrap();
        let watch: WatchConfig = raw.watch.into();
        assert_eq!(watch, WatchConfig {
            enabled: false,
            debounce_ms: 750,
            max_latency_ms: 4000,
            periodic_sweep_secs: 0,
        });
    }

    #[test]
    fn version_check_defaults_on_and_parses_opt_out() {
        let default: VersionCheckConfig = RawVersionCheck::default().into();
        assert!(default.enabled, "version check is opted in by default");

        let raw: RawConfig =
            toml::from_str("[index]\nroot = \".\"\n\n[version_check]\nenabled = false\n").unwrap();
        let version_check: VersionCheckConfig = raw.version_check.into();
        assert!(!version_check.enabled, "[version_check] enabled = false opts out");
    }

    #[test]
    fn search_defaults_off_and_parses_opt_in() {
        let default: SearchConfig = RawSearch::default().into();
        assert!(!default.graded_git_rerank, "graded git rerank is OFF by default");

        let raw: RawConfig =
            toml::from_str("[index]\nroot = \".\"\n\n[search]\ngraded_git_rerank = true\n")
                .unwrap();
        let search: SearchConfig = raw.search.into();
        assert!(search.graded_git_rerank, "[search] graded_git_rerank = true opts in");
    }

    #[test]
    fn dream_absent_defaults_to_off_and_local_ollama_connect() {
        // No `[llm.dream]` at all → disabled, with a local-Ollama CONNECT serving default
        // (byte-for-byte the pre-migration `[dream.model]` default).
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"
            "#,
        )
        .unwrap();
        let dream = LlmConfig::try_from(raw.llm).unwrap().dream;
        assert!(!dream.enabled, "the model pass is OFF by default");
        assert_eq!(dream.remote, RemoteDreamConfig::default());
        assert_eq!(dream.remote.backend, RemoteBackend::Ollama);
        assert_eq!(dream.remote.endpoint.as_deref(), Some("http://localhost:11434"));
        assert_eq!(dream.remote.model, "qwen3:4b-instruct");
        assert_eq!(dream.remote.request_timeout_s, 300);
        assert!(dream.remote.is_connect() && !dream.remote.is_ephemeral());
    }

    #[test]
    fn dream_enabled_flag_without_remote_block_keeps_default_serving() {
        // `[llm.dream] enabled = true` with no `[llm.dream.remote]` still resolves to the default
        // (a local-Ollama connect) — dream has no in-process backend, so `remote` is never `None`.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.dream]
            enabled = true
            "#,
        )
        .unwrap();
        let dream = LlmConfig::try_from(raw.llm).unwrap().dream;
        assert!(dream.enabled, "[llm.dream] enabled = true opts in");
        assert_eq!(dream.remote, RemoteDreamConfig::default());
    }

    #[test]
    fn dream_remote_connect_happy_path_applies_defaults() {
        // CONNECT is inferred from `endpoint` being set.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.dream]
            enabled = true

            [llm.dream.remote]
            backend = "ollama"
            endpoint = "http://ollama.local:11434"
            model = "qwen3:8b"
            auth_env = "OLLAMA_TOKEN"
            request_timeout_s = 60
            "#,
        )
        .unwrap();
        let dream = LlmConfig::try_from(raw.llm).unwrap().dream;
        assert!(dream.enabled);
        assert_eq!(dream.remote, RemoteDreamConfig {
            backend: RemoteBackend::Ollama,
            endpoint: Some("http://ollama.local:11434".to_string()),
            cookbook: None,
            model: "qwen3:8b".to_string(),
            gpu: None,
            auth_env: Some("OLLAMA_TOKEN".to_string()),
            request_timeout_s: 60,
        });
        assert!(dream.remote.is_connect() && !dream.remote.is_ephemeral());
    }

    #[test]
    fn dream_remote_ephemeral_infers_mode_and_parses_gpu() {
        // EPHEMERAL is inferred from `cookbook` being set; a vLLM backend serves chat, and `gpu` is
        // trimmed (not validated here). No `query_endpoint`/batching knobs exist for dream.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.dream]
            enabled = true

            [llm.dream.remote]
            backend = "vllm"
            cookbook = "@rag-rat/cookbook modal"
            gpu = "  A10G  "
            model = "Qwen/Qwen3-4B-Instruct-2507"
            request_timeout_s = 900
            "#,
        )
        .unwrap();
        let remote = LlmConfig::try_from(raw.llm).unwrap().dream.remote;
        assert!(remote.is_ephemeral() && !remote.is_connect());
        assert_eq!(remote.backend, RemoteBackend::Vllm);
        assert_eq!(remote.cookbook.as_deref(), Some("@rag-rat/cookbook modal"));
        assert_eq!(remote.gpu.as_deref(), Some("A10G"), "gpu is trimmed");
        assert_eq!(remote.model, "Qwen/Qwen3-4B-Instruct-2507");
        assert_eq!(remote.request_timeout_s, 900);
    }

    #[test]
    fn dream_remote_requires_a_non_empty_model() {
        for model_line in ["", "model = \"  \""] {
            let raw: RawConfig = toml::from_str(&format!(
                r#"
                [index]
                root = "."

                [llm.dream.remote]
                endpoint = "http://localhost:11434"
                {model_line}
                "#,
            ))
            .unwrap();
            let err = LlmConfig::try_from(raw.llm).unwrap_err();
            assert!(
                matches!(err, ConfigError::DreamRemoteMissingModel),
                "model={model_line:?} → DreamRemoteMissingModel, got {err:?}",
            );
        }
    }

    #[test]
    fn dream_remote_infinity_backend_cannot_serve_chat() {
        // `infinity` is embed-only; a dream remote on it is rejected at parse time.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.dream.remote]
            backend = "infinity"
            endpoint = "http://localhost:7997"
            model = "some-model"
            "#,
        )
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(&err, ConfigError::DreamBackendCannotServeChat(b) if b.as_str() == "infinity"),
            "infinity → DreamBackendCannotServeChat, got {err:?}",
        );
    }

    #[test]
    fn dream_remote_requires_exactly_one_of_endpoint_or_cookbook() {
        // Neither → no server to reach; both → ambiguous mode. Both reject with the exactly-one
        // rule.
        let neither = r#"
            [index]
            root = "."

            [llm.dream.remote]
            model = "qwen3:8b"
            "#;
        let both = r#"
            [index]
            root = "."

            [llm.dream.remote]
            model = "qwen3:8b"
            endpoint = "http://localhost:11434"
            cookbook = "@rag-rat/cookbook modal"
            "#;
        for (label, toml_str) in [("neither", neither), ("both", both)] {
            let raw: RawConfig = toml::from_str(toml_str).unwrap();
            let err = LlmConfig::try_from(raw.llm).unwrap_err();
            assert!(
                matches!(err, ConfigError::DreamRemoteModeAmbiguous),
                "{label} endpoint/cookbook → DreamRemoteModeAmbiguous, got {err:?}",
            );
        }
    }

    #[test]
    fn dream_remote_gpu_with_connect_endpoint_is_rejected() {
        // `gpu` only applies to ephemeral `cookbook` provisioning. Set alongside a connect
        // `endpoint` it is meaningless → rejected.
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.dream.remote]
            endpoint = "http://localhost:11434"
            model = "qwen3:8b"
            gpu = "A10G"
            "#,
        )
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::DreamRemoteGpuRequiresCookbook),
            "gpu + endpoint → DreamRemoteGpuRequiresCookbook, got {err:?}",
        );
    }

    #[test]
    fn dream_remote_empty_gpu_is_rejected() {
        for value in ["\"\"", "\"   \""] {
            let raw: RawConfig = toml::from_str(&format!(
                r#"
                [index]
                root = "."

                [llm.dream.remote]
                backend = "vllm"
                cookbook = "@rag-rat/cookbook modal"
                model = "Qwen/Qwen3-4B-Instruct-2507"
                gpu = {value}
                "#,
            ))
            .unwrap();
            let err = LlmConfig::try_from(raw.llm).unwrap_err();
            assert!(
                matches!(err, ConfigError::RemoteGpuEmpty),
                "gpu={value} → RemoteGpuEmpty, got {err:?}",
            );
        }
    }

    #[test]
    fn dream_remote_endpoint_with_credentials_is_rejected() {
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.dream.remote]
            endpoint = "https://user:token@host:11434"
            model = "qwen3:8b"
            "#,
        )
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::DreamRemoteEndpointHasCredentials),
            "endpoint with userinfo → DreamRemoteEndpointHasCredentials, got {err:?}",
        );
    }

    #[test]
    fn dream_remote_unknown_backend_is_rejected() {
        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [llm.dream.remote]
            backend = "tgi"
            endpoint = "http://localhost:11434"
            model = "qwen3:8b"
            "#,
        )
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(&err, ConfigError::RemoteBackendUnknown(b) if b.as_str() == "tgi"),
            "unknown backend → RemoteBackendUnknown, got {err:?}",
        );
    }

    #[test]
    fn remote_backend_chat_capability_and_path() {
        assert!(RemoteBackend::Ollama.supports_chat());
        assert!(RemoteBackend::Vllm.supports_chat());
        assert!(!RemoteBackend::Infinity.supports_chat(), "infinity is embed-only");
        // The chat route is uniform across chat-capable backends; only serving differs.
        assert_eq!(RemoteBackend::Ollama.chat_path(), "/v1/chat/completions");
        assert_eq!(RemoteBackend::Vllm.chat_path(), "/v1/chat/completions");
    }

    #[test]
    fn memory_surface_defaults_summary_and_parses_full_and_rejects_unknown() {
        let default: MemoryConfig = RawMemory::default().try_into().unwrap();
        assert_eq!(
            default.surface,
            MemorySurface::Summary,
            "the memory surface is `summary` by default (bodies deferred to `memory show`)"
        );

        let raw: RawConfig =
            toml::from_str("[index]\nroot = \".\"\n\n[memory]\nsurface = \"full\"\n").unwrap();
        let memory: MemoryConfig = raw.memory.try_into().unwrap();
        assert_eq!(
            memory.surface,
            MemorySurface::Full,
            "surface = \"full\" opts back to whole bodies"
        );

        // Case-insensitive, and a round-trip through `as_str`.
        assert_eq!(MemorySurface::parse("SUMMARY"), Some(MemorySurface::Summary));
        assert_eq!(MemorySurface::parse("FULL"), Some(MemorySurface::Full));
        assert_eq!(MemorySurface::Summary.as_str(), "summary");
        assert_eq!(MemorySurface::Full.as_str(), "full");

        let bad: RawConfig =
            toml::from_str("[index]\nroot = \".\"\n\n[memory]\nsurface = \"digest\"\n").unwrap();
        assert!(matches!(
            MemoryConfig::try_from(bad.memory),
            Err(ConfigError::UnknownMemorySurface(_))
        ));
    }

    #[test]
    fn oracle_defaults_off_and_parses_overrides() {
        let default: OracleConfig = RawOracle::default().into();
        assert!(!default.auto_run, "background oracle is OFF by default");
        assert_eq!(default.auto_run_quiet_period_secs, 900);
        assert_eq!(default.auto_run_min_interval_secs, 21_600);

        let raw: RawConfig = toml::from_str(
            r#"
            [index]
            root = "."

            [oracle]
            auto_run = true
            auto_run_quiet_period_secs = 60
            auto_run_min_interval_secs = 3600
            "#,
        )
        .unwrap();
        let oracle: OracleConfig = raw.oracle.into();
        assert_eq!(oracle, OracleConfig {
            auto_run: true,
            auto_run_quiet_period_secs: 60,
            auto_run_min_interval_secs: 3600,
        });
    }

    #[test]
    fn rejects_unknown_language() {
        let root = std::env::current_dir().unwrap();
        let simple = BTreeMap::from([("cobol".to_string(), vec![".".to_string()])]);

        let err = resolve_targets(&root, simple, Vec::new()).unwrap_err();

        assert!(err.to_string().contains("unknown language"));
    }

    #[test]
    fn log_config_defaults_off() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let log: LogConfig = raw.log.try_into().unwrap();
        assert!(!log.enabled);
        assert_eq!(log.level, LogLevel::Info);
        assert_eq!(log.format, LogFormat::Text);
        assert_eq!(log.retention_days, 7);
        assert_eq!(log.max_files, 200);
    }

    #[test]
    fn log_config_parses_and_rejects_unknown_level_and_format() {
        let raw: RawConfig = toml::from_str(
            "[log]\nenabled=true\nlevel=\"debug\"\nformat=\"json\"\nfilter=\"\
             rag_rat_core::index::ai=trace\"\nmax_files=10",
        )
        .unwrap();
        let log: LogConfig = raw.log.try_into().unwrap();
        assert!(log.enabled);
        assert_eq!(log.level, LogLevel::Debug);
        assert_eq!(log.format, LogFormat::Json);
        assert_eq!(log.filter.as_deref(), Some("rag_rat_core::index::ai=trace"));
        assert_eq!(log.max_files, 10);

        let bad_level: RawConfig = toml::from_str("[log]\nlevel=\"loud\"").unwrap();
        assert!(matches!(LogConfig::try_from(bad_level.log), Err(ConfigError::UnknownLogLevel(_))));
        let bad_fmt: RawConfig = toml::from_str("[log]\nformat=\"xml\"").unwrap();
        assert!(matches!(LogConfig::try_from(bad_fmt.log), Err(ConfigError::UnknownLogFormat(_))));
    }

    #[test]
    fn log_dir_defaults_to_db_sibling_and_custom_is_config_relative() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rag-rat.toml"), "[log]\nenabled=true\n").unwrap();
        let cfg = Config::load(dir.path().join("rag-rat.toml")).unwrap();
        assert_eq!(cfg.log.dir, cfg.database.parent().unwrap().join("logs"));
    }
}
