use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;
use thiserror::Error;

use crate::language::{Language, LanguageError};

#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub database: PathBuf,
    pub targets: Vec<ResolvedTarget>,
    pub local_ai: LocalAiConfig,
    pub watch: WatchConfig,
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
pub struct LocalAiConfig {
    pub embedding: EmbeddingConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbeddingConfig {
    /// Which embedding backend to use for semantic (vector) recall. `init` picks a default based
    /// on repo size; see [`EmbeddingBackend`].
    pub backend: EmbeddingBackend,
    pub runtime: EmbeddingRuntimeConfig,
}

/// The embedding backend selector (`[local_ai.embedding] model = "..."`).
///
/// - `FastEmbed` (default): MiniLM transformer — best quality, but the cold backfill is CPU-bound
///   (~10-100 chunks/sec), so impractical for very large repos.
/// - `Model2Vec`: static token-vector lookup + mean-pool — ~100-500× faster on CPU at some
///   retrieval-quality cost (no context/word-order). The choice for huge repos that still want
///   vectors.
/// - `None`: structural + BM25 only; no dense vectors. `semantic_search` degrades to BM25. The
///   cheapest option for enormous codebases where any embedding backfill is too slow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EmbeddingBackend {
    #[default]
    FastEmbed,
    Model2Vec,
    None,
}

impl EmbeddingBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FastEmbed => "minilm",
            Self::Model2Vec => "model2vec",
            Self::None => "none",
        }
    }

    /// The persisted embedding-model id this backend installs/activates, or `None` for the
    /// embeddings-off choice. Kept as a string so `rag-rat-core::index::ai` model ids stay the
    /// single source of truth without `config` depending on `index`.
    pub fn model_id(self) -> Option<&'static str> {
        match self {
            Self::FastEmbed => Some("fastembed-all-minilm-l6-v2"),
            Self::Model2Vec => Some("model2vec-potion-retrieval-32m"),
            Self::None => None,
        }
    }
}

impl FromStr for EmbeddingBackend {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "minilm" | "fastembed" | "minilm-l6" => Ok(Self::FastEmbed),
            "model2vec" | "potion" | "static" => Ok(Self::Model2Vec),
            "none" | "off" | "bm25" => Ok(Self::None),
            other => Err(ConfigError::UnknownEmbeddingBackend(other.to_string())),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub name: String,
    pub language: Language,
    pub directories: Vec<PathBuf>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub kind: TargetKind,
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

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&text)?;
        let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let root = config_dir.join(raw.index.root.unwrap_or_else(|| ".".to_string()));
        let root = normalize_existing_dir(&root)?;
        // One database per repo: resolve a relative database path against the *main* worktree so
        // all linked worktrees of a repo share one index (the commit/worktree overlay is built for
        // exactly this). An absolute path is honored as-is. The main worktree resolves against its
        // own root (unchanged), so single-worktree users see no change.
        let database = match raw.index.database {
            Some(db) if Path::new(&db).is_absolute() => PathBuf::from(db),
            other => {
                let relative = other.unwrap_or_else(|| ".rag-rat/index.sqlite".to_string());
                shared_db_base(&root).join(relative)
            },
        };
        let targets = resolve_targets(&root, raw.target_bindings, raw.target)?;
        let local_ai = LocalAiConfig::try_from(raw.local_ai)?;
        let watch = raw.watch.into();

        Ok(Self { root, database, targets, local_ai, watch })
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
            include: language.simple_extensions().iter().map(|ext| format!("**/*.{ext}")).collect(),
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
            include: target.include.unwrap_or_else(|| {
                language.simple_extensions().iter().map(|ext| format!("**/*.{ext}")).collect()
            }),
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

/// Base directory a relative `database` path resolves against. For a **linked** git worktree this
/// is the **main** worktree root (so all worktrees share one index DB); for the main worktree or a
/// non-git dir it is `root` unchanged — single-worktree setups keep their existing DB location.
fn shared_db_base(root: &Path) -> PathBuf {
    match main_worktree_root(root) {
        Some(main_root) if main_root != root => main_root,
        _ => root.to_path_buf(),
    }
}

/// The main worktree root, derived from the git common dir (`<main>/.git`). Returns `None` outside
/// a standard git repo (bare repo, custom `GIT_DIR`, git unavailable) so resolution falls back to
/// `root` — never guess.
fn main_worktree_root(root: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let common_dir = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if common_dir.is_empty() {
        return None;
    }
    let common_dir = root.join(common_dir).canonicalize().ok()?;
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
    local_ai: RawLocalAi,
    #[serde(default)]
    watch: RawWatch,
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
struct RawIndex {
    root: Option<String>,
    database: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLocalAi {
    #[serde(default)]
    embedding: RawEmbedding,
}

impl TryFrom<RawLocalAi> for LocalAiConfig {
    type Error = ConfigError;

    fn try_from(raw: RawLocalAi) -> Result<Self, Self::Error> {
        Ok(Self { embedding: EmbeddingConfig::try_from(raw.embedding)? })
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawEmbedding {
    /// `model = "minilm" | "model2vec" | "none"` — the embedding backend selector.
    model: Option<String>,
    #[serde(default)]
    runtime: RawEmbeddingRuntime,
}

impl TryFrom<RawEmbedding> for EmbeddingConfig {
    type Error = ConfigError;

    fn try_from(raw: RawEmbedding) -> Result<Self, Self::Error> {
        let backend = match raw.model.as_deref() {
            Some(value) => value.parse()?,
            None => EmbeddingBackend::default(),
        };
        Ok(Self { backend, runtime: raw.runtime.into() })
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
    #[error("unknown embedding backend `{0}` (expected `minilm`, `model2vec`, or `none`)")]
    UnknownEmbeddingBackend(String),
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

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn shared_db_base_shares_one_db_across_worktrees() {
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("ragrat-cfg-{}-{id}", std::process::id()));
        let main = tmp.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("seed.txt"), "x").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "seed"]);
        let linked = tmp.join("wt");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

        let main_c = main.canonicalize().unwrap();
        let linked_c = linked.canonicalize().unwrap();

        // Main worktree resolves to itself (no redirect → existing DB location preserved).
        assert_eq!(shared_db_base(&main_c), main_c);
        // Linked worktree redirects to the main worktree → one shared DB.
        assert_eq!(shared_db_base(&linked_c), main_c);

        // A non-git directory falls back to itself.
        let plain = tmp.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let plain_c = plain.canonicalize().unwrap();
        assert_eq!(shared_db_base(&plain_c), plain_c);

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

            [local_ai.embedding.runtime]
            batch_size = 128
            ort_threads = 2
            omp_threads = 1
            max_embedding_chars = 5000
            "#,
        )
        .unwrap();

        let local_ai = LocalAiConfig::try_from(raw.local_ai).unwrap();

        assert_eq!(local_ai.embedding.runtime, EmbeddingRuntimeConfig {
            batch_size: 128,
            ort_threads: Some(2),
            omp_threads: Some(1),
            max_embedding_chars: 5000,
        });
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
    fn rejects_unknown_language() {
        let root = std::env::current_dir().unwrap();
        let simple = BTreeMap::from([("python".to_string(), vec![".".to_string()])]);

        let err = resolve_targets(&root, simple, Vec::new()).unwrap_err();

        assert!(err.to_string().contains("unknown language"));
    }
}
