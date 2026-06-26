use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

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
    pub version_check: VersionCheckConfig,
    pub oracle: OracleConfig,
    pub search: SearchConfig,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmConfig {
    pub embedding: EmbeddingConfig,
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

/// Remote-embedding offload (`[llm.embedding.remote]`). Hands embedding work to an HTTP
/// server (Ollama's `/api/embed`) instead of running the model in-process — the lever for huge
/// repos whose in-process backfill is too slow on the indexing box. Optional: absent → in-process
/// embedding only.
///
/// Deliberately carries NO `dim` or `backend` field. The vector dimension comes from the registry
/// spec of the SELECTED model (`model = "sentence-transformers/all-MiniLM-L6-v2"`, dim 384) and is
/// validated against the server's first response at runtime by the embedder; the runtime is implied
/// by this block's mere PRESENCE (#317 rework). Duplicating either here would be redundant and
/// drift-prone.
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
    /// The Ollama API model name sent to `/api/embed` (e.g. `"all-minilm"`). This is the server's
    /// own model identifier, NOT a `rag-rat` registry alias — the registry only supplies the dim
    /// parity contract.
    pub model: String,
    /// CONNECT: base URL of an already-running Ollama (e.g. `"http://localhost:11434"`);
    /// `/api/embed` is appended by the embedder. Mutually exclusive with `cookbook`.
    pub endpoint: Option<String>,
    /// EPHEMERAL: the cookbook recipe rag-rat spawns to provision an on-demand box — an npm
    /// package spec (`"@rag-rat/cookbook/modal"`, run via `npx -y`) or a recipe file path
    /// (`.mjs`/`.js` → `node`, `.ts` → `npx tsx`). Mutually exclusive with `endpoint`.
    pub cookbook: Option<String>,
    /// EPHEMERAL: the LOCAL Ollama used for QUERY embedding (queries embed the same model as the
    /// remote-embedded chunks → identical vector space). Defaults to `http://localhost:11434` when
    /// ephemeral and omitted. Ignored in connect mode.
    pub query_endpoint: Option<String>,
    /// Name of the environment variable holding the bearer token, if the server needs auth. Local
    /// Ollama needs none, so this is optional; the embedder reads the var once at construction.
    pub auth_env: Option<String>,
    /// How many texts to send per `/api/embed` request.
    pub batch_size: u32,
    /// Per-request HTTP timeout, in seconds.
    pub request_timeout_s: u64,
}

/// The default LOCAL Ollama URL for ephemeral query embedding when `query_endpoint` is omitted.
pub const DEFAULT_QUERY_ENDPOINT: &str = "http://localhost:11434";

impl Default for RemoteEmbeddingConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            endpoint: None,
            cookbook: None,
            query_endpoint: None,
            auth_env: None,
            batch_size: 256,
            request_timeout_s: 60,
        }
    }
}

impl RemoteEmbeddingConfig {
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

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&text)?;
        // Reject the renamed `[local_ai]` table loudly (#317) — a silently-ignored old table would
        // drop the user's embedding settings on upgrade. See `RawConfig::local_ai`.
        if raw.local_ai.is_some() {
            return Err(ConfigError::LocalAiTableRenamed);
        }
        let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let root = config_dir.join(raw.index.root.unwrap_or_else(|| ".".to_string()));
        // The root of the checkout the config was actually READ from (a linked worktree has its own
        // checked-out `rag-rat.toml`). Targets are validated against THIS, not the anchored main
        // root below: a linked branch can add a target dir that exists only in that branch, and the
        // launch must not fail with `MissingDirectory` against the main checkout (#219 review).
        let local_root = normalize_existing_dir(&root)?;
        // Anchor `root` to the MAIN worktree root, so EVERY invocation resolves to the same root
        // and thus the same base commit for the one shared index — whether launched from
        // the main checkout, a linked worktree, or any cwd (`root="."` resolves against the
        // launch dir, and a linked worktree has its OWN checked-out config). Per-worktree
        // results come from the `worktree` QUERY scope, never a per-worktree config root:
        // otherwise two processes rooted at different worktrees (different base commits)
        // write CONFLICTING overlay rows to the shared DB — a file present on one branch
        // races between readable and tombstone (#218/#219). The main worktree (and non-git
        // dirs) resolve to themselves, so single-worktree users see no change.
        let root = anchor_root_to_main_worktree(&local_root);
        // One database per repo, shared across worktrees: a relative path resolves against the MAIN
        // worktree TOP — NOT `root`, which may be a subdirectory — so every worktree of a repo AND
        // any `root="<subdir>"` config land on the SAME index. An absolute path is honored as-is.
        let database = match raw.index.database {
            Some(db) if Path::new(&db).is_absolute() => PathBuf::from(db),
            other => {
                let relative = other.unwrap_or_else(|| ".rag-rat/index.sqlite".to_string());
                main_worktree_root(&root).unwrap_or_else(|| root.clone()).join(relative)
            },
        };
        // Validate directories against the local checkout (`local_root`), where the config — and
        // any branch-only target dir — actually lives; the stored `Config.root` stays
        // anchored to main so every worktree shares one base index.
        // `ResolvedTarget.directories` are root-relative, so the stored targets are
        // identical either way (#219 review).
        let local_targets = resolve_targets(&local_root, raw.target_bindings, raw.target)?;
        // The stored `Config.targets` are the BASE targets — they drive discovery over the anchored
        // `root` (= main). When this config was read from a LINKED worktree, its branch
        // `rag-rat.toml` may NARROW or DROP a target that still exists on main; using the
        // branch targets for base discovery would classify the now-undiscovered main files
        // as deleted and tombstone them in the BASE scope, hiding committed files from main
        // queries. So when anchoring to a different (main) root, re-resolve the base
        // targets from MAIN's own `rag-rat.toml`. The branch's targets are NOT lost: the
        // overlay refresh reloads each linked worktree's own config
        // (`refresh_worktree_overlays`) and indexes the branch with its own target set (#219
        // review).
        let targets = main_base_targets(&root, &local_root).unwrap_or(local_targets);
        let llm = LlmConfig::try_from(raw.llm)?;
        let watch = raw.watch.into();
        let version_check = raw.version_check.into();
        let oracle = raw.oracle.into();
        let search = raw.search.into();

        Ok(Self { root, database, targets, llm, watch, version_check, oracle, search })
    }
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

/// The BASE targets for a config that was read from a LINKED worktree: the MAIN checkout's
/// `rag-rat.toml` targets, resolved against the anchored (main) `root`. `None` — so the caller
/// keeps the local (branch) targets — when this config is NOT anchored away from its own checkout
/// (`root == local_root`: the main checkout or a non-git dir), when main has no readable
/// `rag-rat.toml`, or when main's targets don't validate against `root` (e.g. a main target dir
/// that only the subdir layout reaches). Reading main's config keeps base DISCOVERY faithful to
/// main, so a branch that narrows/drops a target can't tombstone main's committed files in the base
/// scope (#219 review). The branch's own targets still drive its overlay via
/// `refresh_worktree_overlays`.
fn main_base_targets(root: &Path, local_root: &Path) -> Option<Vec<ResolvedTarget>> {
    if root == local_root {
        return None; // not anchored away — the local config IS the base config
    }
    let main_config_path = main_worktree_root(root)?.join("rag-rat.toml");
    let text = fs::read_to_string(&main_config_path).ok()?;
    let raw: RawConfig = toml::from_str(&text).ok()?;
    // Main's targets are root-relative; validate (and store) them against the anchored `root`,
    // which is the equivalent path under the main worktree the base index is discovered from.
    resolve_targets(root, raw.target_bindings, raw.target).ok()
}

/// The main worktree root, derived from the git common dir (`<main>/.git`). Returns `None` outside
/// a standard git repo (bare repo, custom `GIT_DIR`, git unavailable) so resolution falls back to
/// `root` — never guess.
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

fn normalize_existing_dir(path: &Path) -> Result<PathBuf, ConfigError> {
    let absolute =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let canonical = absolute.canonicalize()?;
    if !canonical.is_dir() {
        return Err(ConfigError::MissingDirectory(canonical));
    }
    Ok(canonical)
}

#[derive(Debug, Deserialize)]
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
    #[serde(default)]
    watch: RawWatch,
    #[serde(default)]
    version_check: RawVersionCheck,
    #[serde(default)]
    oracle: RawOracle,
    #[serde(default)]
    search: RawSearch,
    #[serde(default)]
    target_bindings: BTreeMap<String, Vec<String>>,
    #[serde(default, rename = "target")]
    target: Vec<RawTarget>,
}

#[derive(Debug, Default, Deserialize)]
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

#[derive(Debug, Default, Deserialize)]
struct RawVersionCheck {
    enabled: Option<bool>,
}

impl From<RawVersionCheck> for VersionCheckConfig {
    fn from(raw: RawVersionCheck) -> Self {
        Self { enabled: raw.enabled.unwrap_or(VersionCheckConfig::default().enabled) }
    }
}

#[derive(Debug, Default, Deserialize)]
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

#[derive(Debug, Default, Deserialize)]
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

#[derive(Debug, Default, Deserialize)]
struct RawIndex {
    root: Option<String>,
    database: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLlm {
    #[serde(default)]
    embedding: RawEmbedding,
}

impl TryFrom<RawLlm> for LlmConfig {
    type Error = ConfigError;

    fn try_from(raw: RawLlm) -> Result<Self, Self::Error> {
        Ok(Self { embedding: EmbeddingConfig::try_from(raw.embedding)? })
    }
}

#[derive(Debug, Default, Deserialize)]
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

#[derive(Debug, Default, Deserialize)]
struct RawRemoteEmbedding {
    model: Option<String>,
    endpoint: Option<String>,
    cookbook: Option<String>,
    query_endpoint: Option<String>,
    auth_env: Option<String>,
    batch_size: Option<u32>,
    request_timeout_s: Option<u64>,
}

/// Whether the URL's authority embeds userinfo (`user[:pass]@host`). Parses the authority as
/// everything after `scheme://` up to the next `/`, `?`, or `#`, and reports an `@` in it. A bare
/// host, an `@` in the path/query, or a URL with no scheme separator all return false. Used to keep
/// credentials out of the endpoint string before it is persisted into the index meta.
fn endpoint_authority_has_userinfo(endpoint: &str) -> bool {
    let after_scheme = endpoint.split_once("://").map_or(endpoint, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or(after_scheme);
    authority.contains('@')
}

impl TryFrom<RawRemoteEmbedding> for RemoteEmbeddingConfig {
    type Error = ConfigError;

    fn try_from(raw: RawRemoteEmbedding) -> Result<Self, Self::Error> {
        let default = RemoteEmbeddingConfig::default();
        let trimmed = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        // The Ollama API model name (e.g. `all-minilm`) — required, non-empty.
        let model = raw.model.unwrap_or_default();
        let model = model.trim();
        if model.is_empty() {
            return Err(ConfigError::RemoteEmbeddingMissingModel);
        }
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
        // EPHEMERAL: the LOCAL query box. Defaults to `DEFAULT_QUERY_ENDPOINT`; ignored for
        // connect.
        let query_endpoint = if cookbook.is_some() {
            Some(trimmed(raw.query_endpoint).unwrap_or_else(|| DEFAULT_QUERY_ENDPOINT.to_string()))
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
        Ok(Self {
            model: model.to_string(),
            endpoint,
            cookbook,
            query_endpoint,
            auth_env,
            batch_size: raw.batch_size.unwrap_or(default.batch_size),
            request_timeout_s: raw.request_timeout_s.unwrap_or(default.request_timeout_s),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
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

#[derive(Debug, Deserialize)]
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
        "[llm.embedding.remote] requires EXACTLY ONE of `endpoint` (connect to a running Ollama) \
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
        "[llm.embedding.remote] can only serve a transformer model over Ollama, but `model = \
         \"{0}\"` is not a transformer (it is a static/hash model, or `none`/disabled) — remove \
         the remote block, or select a transformer model (e.g. \
         `sentence-transformers/all-MiniLM-L6-v2`, `BAAI/bge-small-en-v1.5`, \
         `jinaai/jina-embeddings-v2-base-code`)"
    )]
    RemoteEmbeddingNonTransformerModel(String),
    #[error(
        "the `[local_ai]` table was renamed to `[llm]` (#317). Update your rag-rat.toml: rename \
         `[local_ai.embedding]` → `[llm.embedding]` (and any `[local_ai.embedding.remote]` / \
         `[local_ai.embedding.runtime]` → `[llm.embedding.remote]` / `[llm.embedding.runtime]`)"
    )]
    LocalAiTableRenamed,
    #[error("duplicate target name `{0}`")]
    DuplicateTarget(String),
    #[error("configured directory does not exist: {0}")]
    MissingDirectory(PathBuf),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

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
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
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
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
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
                endpoint: Some("http://localhost:11434".to_string()),
                cookbook: None,
                query_endpoint: None, // connect mode: no local query box
                auth_env: None,
                // defaults applied when omitted
                batch_size: 256,
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
            batch_size = 512
            request_timeout_s = 120
            "#,
        )
        .unwrap();
        let llm = LlmConfig::try_from(raw.llm).unwrap();
        assert_eq!(
            llm.embedding.remote,
            Some(RemoteEmbeddingConfig {
                model: "all-minilm".to_string(),
                endpoint: Some("http://localhost:11434".to_string()),
                cookbook: None,
                query_endpoint: None,
                auth_env: Some("OLLAMA_TOKEN".to_string()),
                batch_size: 512,
                request_timeout_s: 120,
            })
        );
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
}
