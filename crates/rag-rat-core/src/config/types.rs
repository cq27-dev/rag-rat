use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::ConfigError;
use crate::embedding_models::{
    Backend, EmbeddingModelSpec, FASTEMBED_MODEL_ID, MODEL2VEC_MODEL_ID, spec,
};
use crate::language::Language;

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
    pub(crate) fn parse(s: &str) -> Option<Self> {
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
    pub(crate) fn parse(s: &str) -> Option<Self> {
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
    pub(crate) fn parse(s: &str) -> Option<Self> {
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
