//! `WizardDraft` — the persistable model for the init wizard.
//!
//! This is the ONLY thing that renders to TOML. UI-only state (focused step, scroll, help
//! visibility) lives in `WizardUiState` (see `state.rs`) and never leaks in here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;

use rag_rat_base::config::{
    Config, DEFAULT_QUERY_ENDPOINT, EmbeddingBackend, OracleConfig, RemoteBackend,
    RemoteEmbeddingConfig, VersionCheckConfig,
};
use rag_rat_base::language::Language;
use toml_edit::{Array, DocumentMut, Item, Table};

use crate::init::render::{config_root_value, display_rel, render_config};
use crate::init::scan::{candidate_dirs, estimated_chunks, recommend_backend};
use crate::init::{InitPlan, RepoScan};

/// The embedding remote connection mode, derived from `RemoteEmbeddingConfig`.
///
/// CONNECT: an already-running Ollama endpoint (the `endpoint` field was set).
/// EPHEMERAL: provision an on-demand box via a cookbook recipe (the `cookbook` field was set).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteMode {
    Connect(String),
    Ephemeral(String),
}

/// The remote-embedding draft block — mirrors `RemoteEmbeddingConfig` with the mode explicit.
#[derive(Clone, Debug)]
pub(crate) struct RemoteDraft {
    /// The server-side embedding model name (an ollama name for `backend = ollama`, the
    /// HuggingFace id for infinity/vLLM).
    pub model: String,
    /// Which OpenAI-compatible server serves this block (`ollama` | `infinity` | `vllm`).
    pub backend: RemoteBackend,
    /// CONNECT (existing Ollama endpoint) or EPHEMERAL (cookbook recipe).
    pub mode: RemoteMode,
    /// EPHEMERAL-only: the LOCAL server queries embed against after the box is torn down; must run
    /// the same backend + model. Defaults per backend; a custom value is preserved.
    pub query_endpoint: Option<String>,
    /// EPHEMERAL-only: GPU to provision (provider-specific value).
    pub gpu: Option<String>,
    /// Optional Ollama context window for `/api/embed` requests.
    pub num_ctx: Option<u32>,
    /// How many texts per `/api/embed` request.
    pub batch_size: u32,
    /// How many remote `/api/embed` requests to keep in flight.
    pub concurrency: u32,
    /// Maximum total input characters per `/api/embed` request.
    pub max_batch_chars: usize,
    /// Name of the env-var holding the bearer token, if auth is needed.
    pub auth_env: Option<String>,
}

// ── Cookbook + GPU constants ────────────────────────────────────────────────────

/// GPUs available when provisioning via Modal.
pub(crate) const MODAL_GPUS: &[&str] = &[
    "T4",
    "L4",
    "A10",
    "L40S",
    "A100",
    "A100-40GB",
    "A100-80GB",
    "RTX-PRO-6000",
    "H100",
    "H100!",
    "H200",
    "B200",
    "B200+",
];

/// Current RunPod `gpuTypeId` values that are suitable for the NVIDIA Ollama image.
pub(crate) const RUNPOD_GPUS: &[&str] = &[
    "NVIDIA RTX A4000",
    "NVIDIA RTX A4500",
    "NVIDIA RTX A5000",
    "NVIDIA RTX A6000",
    "NVIDIA RTX 2000 Ada Generation",
    "NVIDIA RTX 4000 Ada Generation",
    "NVIDIA RTX 4000 SFF Ada Generation",
    "NVIDIA RTX 5000 Ada Generation",
    "NVIDIA RTX 6000 Ada Generation",
    "NVIDIA RTX PRO 4000 Blackwell",
    "NVIDIA RTX PRO 4500 Blackwell",
    "NVIDIA RTX PRO 5000 Blackwell",
    "NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition",
    "NVIDIA RTX PRO 6000 Blackwell Server Edition",
    "NVIDIA RTX PRO 6000 Blackwell Workstation Edition",
    "NVIDIA GeForce RTX 3090",
    "NVIDIA GeForce RTX 4080",
    "NVIDIA GeForce RTX 4080 SUPER",
    "NVIDIA GeForce RTX 4090",
    "NVIDIA GeForce RTX 5080",
    "NVIDIA GeForce RTX 5090",
    "NVIDIA L4",
    "NVIDIA L40",
    "NVIDIA L40S",
    "NVIDIA A40",
    "NVIDIA A100 80GB PCIe",
    "NVIDIA A100-SXM4-40GB",
    "NVIDIA A100-SXM4-80GB",
    "NVIDIA H100 PCIe",
    "NVIDIA H100 80GB HBM3",
    "NVIDIA H100 NVL",
    "NVIDIA H200",
    "NVIDIA H200 NVL",
    "NVIDIA B200",
    "NVIDIA B300 SXM6 AC",
    "Tesla V100-PCIE-16GB",
    "Tesla V100-SXM2-16GB",
];

/// Common Ollama server-side embedding model names, with their dimension and size.
/// These are all real, pullable models on Ollama Hub (verified June 2026).
pub(crate) const OLLAMA_EMBEDDING_MODELS: &[&str] = &[
    "all-minilm",                         // 384-dim · 46MB  — all-MiniLM-L6-v2
    "qllama/bge-small-en-v1.5:f16",       // 384-dim · 68MB  — BGE-small exact family
    "ordis/jina-embeddings-v2-base-code", // 768-dim · 323MB — Jina code exact family
    "nomic-embed-text",                   // 768-dim · 274MB — general 768d alternative
    "mxbai-embed-large",                  // 1024-dim · 670MB — best English retrieval
    "bge-m3",                             // 1024-dim · 1.2GB — multilingual hybrid
    "snowflake-arctic-embed2",            // 1024-dim · 1.2GB — highest throughput
    "qwen3-embedding:0.6b",               // 1024-dim · 639MB — best quality/size ratio
];

/// Known output dimension for curated Ollama embedding models.
pub(crate) fn ollama_model_dim(model: &str) -> Option<usize> {
    match model {
        "all-minilm" | "qllama/bge-small-en-v1.5:f16" => Some(384),
        "ordis/jina-embeddings-v2-base-code" | "nomic-embed-text" => Some(768),
        "mxbai-embed-large" | "bge-m3" | "snowflake-arctic-embed2" | "qwen3-embedding:0.6b" =>
            Some(1024),
        _ => None,
    }
}

/// Best-effort mapping from the local embedding model_id to a commonly-available
/// Ollama model name. Returns `None` for selectors the wizard cannot serve remotely.
pub(crate) fn ollama_model_for(embedding_model_id: &str) -> Option<&'static str> {
    match embedding_model_id {
        // Prefer same-family Ollama builds; dimension-only alternatives stay selectable manually.
        "sentence-transformers/all-MiniLM-L6-v2" => Some("all-minilm"),
        "BAAI/bge-small-en-v1.5" => Some("qllama/bge-small-en-v1.5:f16"),
        "jinaai/jina-embeddings-v2-base-code" => Some("ordis/jina-embeddings-v2-base-code"),
        _ => None,
    }
}

/// The LOCAL query-embedding endpoint the wizard writes for an EPHEMERAL infinity/vLLM config.
/// After the provisioned box is torn down, queries embed against a LOCAL server (same backend +
/// model), and config validation REQUIRES an explicit `query_endpoint` for non-ollama backends —
/// the ollama-default `localhost:11434` only fits ollama, so a wizard-written infinity/vLLM
/// ephemeral config would otherwise fail to load. Defaults to the backend's standard local port.
/// `None` when the config default already suffices: ollama (11434) and connect mode (queries hit
/// the endpoint).
pub(crate) fn wizard_query_endpoint(
    mode: &RemoteMode,
    backend: RemoteBackend,
) -> Option<&'static str> {
    match (mode, backend) {
        (RemoteMode::Ephemeral(_), RemoteBackend::Infinity | RemoteBackend::Vllm) =>
            Some(default_backend_endpoint(backend)),
        _ => None,
    }
}

/// The default LOCAL endpoint the wizard uses for a backend: ollama 11434, infinity 7997, vLLM
/// 8000. The single source for BOTH the connect `endpoint` default and the ephemeral
/// `query_endpoint` default, so the endpoint always matches the selected backend's route + port.
pub(crate) fn default_backend_endpoint(backend: RemoteBackend) -> &'static str {
    match backend {
        RemoteBackend::Ollama => DEFAULT_QUERY_ENDPOINT,
        RemoteBackend::Infinity => "http://localhost:7997",
        RemoteBackend::Vllm => "http://localhost:8000",
    }
}

/// Whether `url` is one of the wizard's known default LOCAL endpoints (any backend's) — a value the
/// wizard itself wrote, so it is safe to REPLACE when the backend or mode changes. A URL outside
/// this set is a user customization and is preserved.
pub(crate) fn is_default_backend_endpoint(url: &str) -> bool {
    [RemoteBackend::Ollama, RemoteBackend::Infinity, RemoteBackend::Vllm]
        .into_iter()
        .any(|b| default_backend_endpoint(b) == url)
}

/// Hook installation selections.
#[derive(Clone, Debug, Default)]
pub(crate) struct HooksDraft {
    /// Whether to install the rag-rat git maintenance hooks (post-checkout/merge/rewrite/commit).
    pub git: bool,
}

/// The persistable wizard model — the only thing that renders to `rag-rat.toml`.
///
/// Built either from a fresh `RepoScan` (`from_scan`) or by loading an existing `Config`
/// (`from_config`). The writer reads ONLY this struct; no UI concept leaks in here.
#[derive(Clone, Debug)]
pub(crate) struct WizardDraft {
    /// The `[index] root` value (a relative path such as `"."` or `".."`).
    ///
    /// ALWAYS a relative path — exactly what gets written to `[index] root` in the TOML.
    /// Use `config_root_value` to derive this from an absolute root + config path.
    pub root_value: String,
    /// Resolved absolute repo root, for dir-existence validation; NOT serialized — only
    /// `root_value` renders to TOML.
    pub root_abs: PathBuf,
    /// The RESOLVED database path — where this repo's index actually lands, for display.
    ///
    /// For a fresh draft this is the keyless default (`config::default_database_path`: the
    /// machine-global store, or a pre-existing legacy `.rag-rat/index.sqlite`); for a reconfigure
    /// it is `Config::load`'s resolved `database` (an explicit key, or the same keyless default).
    /// NOT emitted: `write_fresh` renders a keyless config (the A7 global default) and
    /// `patch_existing` never touches `database` (toml_edit preserves an existing explicit key
    /// untouched). Reserved seam — keep the narrow allow until a step surfaces/edits it.
    #[allow(dead_code)]
    pub db_path: String,
    /// Per-language directory bindings (`[target_bindings]`).
    pub bindings: BTreeMap<Language, Vec<PathBuf>>,
    /// Whether the original TOML contains rich `[[target]]` blocks that the wizard preserves but
    /// does not edit through `[target_bindings]`.
    pub has_rich_targets: bool,
    /// Names from preserved rich `[[target]]` blocks. Simple target bindings generate target
    /// names from `Language::as_str()`, so the wizard must block collisions before writing.
    pub rich_target_names: BTreeSet<String>,
    /// The embedding model selector (`[llm.embedding] model`).
    pub model: String,
    /// Optional remote-embedding block (`[llm.embedding.remote]`); `None` → in-process only.
    pub remote: Option<RemoteDraft>,
    /// `[oracle] auto_run`.
    pub oracle_auto_run: bool,
    /// `[oracle] auto_run_quiet_period_secs`. Captured from config but not yet edited/emitted (the
    /// Oracle step owns only `auto_run`; the interval knobs stay as `render_config` commented
    /// defaults). Reserved seam — see the module doc on `oracle.rs`.
    #[allow(dead_code)]
    pub oracle_quiet_secs: u64,
    /// `[oracle] auto_run_min_interval_secs`. Reserved seam alongside `oracle_quiet_secs`.
    #[allow(dead_code)]
    pub oracle_min_interval_secs: u64,
    /// `[version_check] enabled`.
    pub version_check: bool,
    /// Hook installation selections (Task 16 fills `git` from current hook status).
    pub hooks: HooksDraft,
}

impl WizardDraft {
    /// Build a fresh draft from a `RepoScan`, mirroring `init::run::default_plan` logic:
    ///
    /// - Languages: those with a non-zero file count in `scan`.
    /// - Directories: the default-flagged candidates from `candidate_dirs`.
    /// - Embedding backend: `recommend_backend` based on estimated chunk count.
    /// - Oracle: off (must be opted in explicitly).
    /// - Version check: on (the default).
    /// - Hooks: all false (Task 16 fills in actual installed status).
    ///
    /// `root_value` is the relative `[index] root` string (e.g. `"."`) already computed by the
    /// caller. `root_abs` is the absolute canonical repo root used for dir-existence validation.
    pub(crate) fn from_scan(scan: &RepoScan, root_value: String, root_abs: PathBuf) -> Self {
        let mut bindings: BTreeMap<Language, Vec<PathBuf>> = BTreeMap::new();

        for &lang in Language::all() {
            if scan.language_counts().get(&lang).copied().unwrap_or(0) == 0 {
                continue;
            }
            let candidates = candidate_dirs(scan, lang);
            let defaults: Vec<PathBuf> =
                candidates.iter().filter(|c| c.default).map(|c| c.path.clone()).collect();

            // Python env-only repo: no safe default → omit the binding entirely (matches
            // default_plan's behaviour — `candidate_dirs` deliberately refuses to promote `.`
            // over a dependency tree).
            if lang == Language::Python && !candidates.is_empty() && defaults.is_empty() {
                continue;
            }

            let dirs = if defaults.is_empty() {
                // Non-Python with no default candidate: fall back to "." (index everything).
                vec![PathBuf::from(".")]
            } else {
                defaults
            };
            bindings.insert(lang, dirs);
        }

        let backend = recommend_backend(estimated_chunks(scan.total_source_bytes()));
        let oracle = OracleConfig::default();

        // The RESOLVED landing spot for the keyless config this draft renders (display only):
        // the machine-global store, or a pre-existing legacy `.rag-rat/index.sqlite`.
        let db_path = rag_rat_base::config::default_database_path(&root_abs, &root_abs, None)
            .display()
            .to_string();

        Self {
            root_value,
            root_abs,
            db_path,
            bindings,
            has_rich_targets: false,
            rich_target_names: BTreeSet::new(),
            model: backend.as_str().to_string(),
            remote: None,
            oracle_auto_run: false,
            oracle_quiet_secs: oracle.auto_run_quiet_period_secs,
            oracle_min_interval_secs: oracle.auto_run_min_interval_secs,
            version_check: VersionCheckConfig::default().enabled,
            hooks: HooksDraft::default(),
        }
    }

    /// Build a draft from an existing `Config` (the reconfigure path).
    ///
    /// `config_path` is the path to the `rag-rat.toml` file being reconfigured; it is used to
    /// derive the relative `[index] root` value (via `config_root_value`) so `root_value` is
    /// always a relative path, never an absolute one.
    ///
    /// Field mappings:
    /// - `cfg.targets` grouped by `ResolvedTarget.language` → `bindings`; directories merged in
    ///   declaration order (stable — `Config` preserves the TOML `targets` order).
    /// - `cfg.llm.embedding.backend.as_str()` → `model`.
    /// - `cfg.llm.embedding.remote` → `remote` (`endpoint` → `RemoteMode::Connect`, `cookbook` →
    ///   `RemoteMode::Ephemeral`).
    /// - `cfg.oracle.*` → oracle fields.
    /// - `cfg.version_check.enabled` → `version_check`.
    /// - Hook status: Task 16 fills `git`; defaulted to `false` here.
    pub(crate) fn from_config(cfg: &Config, config_path: &std::path::Path) -> Self {
        // Group target directories by language. Config preserves TOML order; BTreeMap gives a
        // stable language-sorted order for the wizard display.
        let mut bindings: BTreeMap<Language, Vec<PathBuf>> = BTreeMap::new();
        for target in &cfg.targets {
            bindings.entry(target.language).or_default().extend(target.directories.iter().cloned());
        }

        let remote = cfg.llm.embedding.remote.as_ref().map(remote_draft_from_config);
        let oracle = &cfg.oracle;

        Self {
            // Derive the relative `[index] root` value from the absolute cfg.root + config_path,
            // so reconfigure never bakes an absolute path into the user's TOML.
            root_value: config_root_value(&cfg.root, config_path),
            // cfg.root is the canonical absolute path from Config::load; kept for dir-existence
            // validation (Directories step) but never written to TOML.
            root_abs: cfg.root.clone(),
            db_path: cfg.database.to_string_lossy().into_owned(),
            bindings,
            has_rich_targets: false,
            rich_target_names: BTreeSet::new(),
            model: cfg.llm.embedding.backend.as_str().to_string(),
            remote,
            oracle_auto_run: oracle.auto_run,
            oracle_quiet_secs: oracle.auto_run_quiet_period_secs,
            oracle_min_interval_secs: oracle.auto_run_min_interval_secs,
            version_check: cfg.version_check.enabled,
            // Task 16 fills git from the current installed hook status.
            hooks: HooksDraft::default(),
        }
    }

    /// Build a reconfigure draft from both the resolved config and the original TOML text.
    ///
    /// `Config::load` intentionally resolves rich targets and relative cookbook paths for runtime
    /// use. The wizard writer owns only `[target_bindings]` and the literal remote fields, so the
    /// reconfigure path must recover those literals from the raw document before patching.
    pub(crate) fn from_existing(raw: &str, cfg: &Config, config_path: &std::path::Path) -> Self {
        let mut draft = Self::from_config(cfg, config_path);
        let Ok(doc) = raw.parse::<DocumentMut>() else {
            return draft;
        };
        if let Some(root) = raw_root_value(&doc) {
            draft.root_value = root;
        }
        draft.has_rich_targets = has_rich_targets(&doc);
        draft.rich_target_names = raw_rich_target_names(&doc);
        draft.bindings = raw_target_bindings(&doc).unwrap_or_default();
        if let Some(remote) = raw_remote_draft(&doc) {
            draft.remote = Some(remote);
        }
        draft
    }

    pub(crate) fn conflicting_rich_target_names(&self) -> Vec<String> {
        self.bindings
            .keys()
            .map(|lang| lang.as_str())
            .filter(|name| self.rich_target_names.contains(*name))
            .map(ToOwned::to_owned)
            .collect()
    }

    /// Build an `InitPlan` from this draft — the bridge to `render_config`.
    ///
    /// Language order follows the `BTreeMap` key order (alphabetical by `Language::as_str`), which
    /// is stable and matches the insertion order the wizard and `from_scan` use.
    pub(crate) fn to_init_plan(&self) -> InitPlan {
        let languages: Vec<Language> = self.bindings.keys().copied().collect();
        let backend =
            EmbeddingBackend::from_str(&self.model).unwrap_or(EmbeddingBackend::fast_embed());
        InitPlan {
            root_value: self.root_value.clone(),
            languages,
            bindings: self.bindings.clone(),
            backend,
            oracle_auto_run: self.oracle_auto_run,
        }
    }

    /// Render a fresh `rag-rat.toml` by starting with the canonical legacy renderer, then applying
    /// the wizard-owned fields that `InitPlan` cannot carry.
    pub(crate) fn write_fresh(&self) -> String {
        let base = render_config(&self.to_init_plan());
        self.patch_existing(&base).unwrap_or(base)
    }

    /// Patch an existing `rag-rat.toml` in place, preserving all comments and unknown tables.
    ///
    /// Only wizard-owned keys are touched:
    /// - `[index] root`
    /// - `[target_bindings]` (rebuilt from `self.bindings`)
    /// - `[llm.embedding] model`
    /// - `[llm.embedding.remote]` block (set or removed depending on `self.remote`)
    /// - `[oracle] auto_run`
    /// - `[version_check] enabled`
    ///
    /// All other tables, keys, and comments are left intact.
    pub(crate) fn patch_existing(&self, original: &str) -> anyhow::Result<String> {
        let mut doc: DocumentMut = original.parse()?;

        // [index] root
        doc["index"]["root"] = toml_edit::value(self.root_value.clone());

        // [target_bindings] — rebuild from scratch (wipes old bindings for owned languages, but
        // leaves any unknown keys inside [target_bindings] by only inserting/overwriting our own).
        // The simpler and correct approach for wizard ownership: replace the whole table's entries
        // for languages we own, and leave any others alone. Since the wizard owns ALL languages we
        // know about, we replace the whole table to match the canonical output.
        {
            let tb = if self.bindings.is_empty() {
                doc.get_mut("target_bindings")
            } else {
                Some(doc["target_bindings"].or_insert(Item::Table(Table::new())))
            };
            if let Some(tb) = tb {
                if tb.as_table_like_mut().is_none() {
                    *tb = Item::Table(Table::new());
                }
                if let Some(table) = tb.as_table_like_mut() {
                    // Remove any keys corresponding to languages we now own (to avoid stale
                    // entries).
                    for lang in Language::all() {
                        table.remove(lang.as_str());
                    }
                    // Insert the current bindings.
                    for (lang, paths) in &self.bindings {
                        let mut arr = Array::new();
                        for path in paths {
                            arr.push(display_rel(path));
                        }
                        table.insert(lang.as_str(), toml_edit::value(arr));
                    }
                }
            }
        }

        // [llm.embedding] model
        doc["llm"]["embedding"]["model"] = toml_edit::value(self.model.clone());

        // [llm.embedding.remote] — set or remove the block.
        if let Some(remote) = &self.remote {
            let remote_item =
                doc["llm"]["embedding"]["remote"].or_insert(Item::Table(Table::new()));
            if remote_item.as_table_like_mut().is_none() {
                *remote_item = Item::Table(Table::new());
            }
            if let Some(t) = remote_item.as_table_like_mut() {
                t.insert("model", toml_edit::value(remote.model.clone()));
                t.insert("backend", toml_edit::value(remote.backend.as_db_str()));
                match &remote.mode {
                    RemoteMode::Connect(ep) => {
                        t.insert("endpoint", toml_edit::value(ep.clone()));
                        t.remove("cookbook");
                    },
                    RemoteMode::Ephemeral(cb) => {
                        t.insert("cookbook", toml_edit::value(cb.clone()));
                        t.remove("endpoint");
                    },
                }
                // `query_endpoint` is DRIVEN by the draft — the draft is the single source of
                // truth. For EPHEMERAL: write `remote.query_endpoint` when `Some` (the per-backend
                // default or a preserved custom value, both managed by the steps constructors),
                // else drop it so ollama falls back to the config default (11434). CONNECT ignores
                // query_endpoint entirely, so it is always removed.
                match &remote.mode {
                    RemoteMode::Ephemeral(_) => match &remote.query_endpoint {
                        Some(qe) => {
                            t.insert("query_endpoint", toml_edit::value(qe.clone()));
                        },
                        None => {
                            t.remove("query_endpoint");
                        },
                    },
                    RemoteMode::Connect(_) => {
                        t.remove("query_endpoint");
                    },
                }
                t.insert("batch_size", toml_edit::value(i64::from(remote.batch_size)));
                t.insert("concurrency", toml_edit::value(i64::from(remote.concurrency)));
                t.insert(
                    "max_batch_chars",
                    toml_edit::value(i64::try_from(remote.max_batch_chars).unwrap_or(i64::MAX)),
                );
                if let Some(gpu) = &remote.gpu {
                    t.insert("gpu", toml_edit::value(gpu.clone()));
                } else {
                    t.remove("gpu");
                }
                if let Some(num_ctx) = remote.num_ctx {
                    t.insert("num_ctx", toml_edit::value(i64::from(num_ctx)));
                } else {
                    t.remove("num_ctx");
                }
                if let Some(env) = &remote.auth_env {
                    t.insert("auth_env", toml_edit::value(env.clone()));
                } else {
                    t.remove("auth_env");
                }
            }
        } else if let Some(llm_table) = doc["llm"].as_table_like_mut()
            && let Some(embed_item) = llm_table.get_mut("embedding")
            && let Some(embed_table) = embed_item.as_table_like_mut()
        {
            embed_table.remove("remote");
        }

        // [oracle] auto_run
        doc["oracle"]["auto_run"] = toml_edit::value(self.oracle_auto_run);

        // [version_check] enabled
        doc["version_check"]["enabled"] = toml_edit::value(self.version_check);

        Ok(doc.to_string())
    }
}

/// Convert a `RemoteEmbeddingConfig` to a `RemoteDraft`.
///
/// The mode is inferred from which field is set — config validation guarantees exactly one of
/// `endpoint` (Connect) or `cookbook` (Ephemeral) is present.
fn remote_draft_from_config(r: &RemoteEmbeddingConfig) -> RemoteDraft {
    let mode = if let Some(ep) = &r.endpoint {
        RemoteMode::Connect(ep.clone())
    } else if let Some(cb) = &r.cookbook {
        RemoteMode::Ephemeral(cb.clone())
    } else {
        // Unreachable: config validation requires exactly one of endpoint/cookbook.
        RemoteMode::Connect(String::new())
    };
    RemoteDraft {
        model: r.model.clone(),
        backend: r.backend,
        mode,
        query_endpoint: r.query_endpoint.clone(),
        gpu: r.gpu.clone(),
        num_ctx: r.num_ctx,
        batch_size: r.batch_size,
        concurrency: r.concurrency,
        max_batch_chars: r.max_batch_chars,
        auth_env: r.auth_env.clone(),
    }
}

fn raw_target_bindings(doc: &DocumentMut) -> Option<BTreeMap<Language, Vec<PathBuf>>> {
    let table = doc.get("target_bindings")?.as_table_like()?;
    let mut bindings = BTreeMap::new();
    for lang in Language::all() {
        let Some(array) = table.get(lang.as_str()).and_then(Item::as_array) else {
            continue;
        };
        let paths =
            array.iter().filter_map(|value| value.as_str().map(PathBuf::from)).collect::<Vec<_>>();
        if !paths.is_empty() {
            bindings.insert(*lang, paths);
        }
    }
    Some(bindings)
}

fn has_rich_targets(doc: &DocumentMut) -> bool {
    doc.get("target").and_then(Item::as_array_of_tables).is_some_and(|targets| !targets.is_empty())
}

fn raw_rich_target_names(doc: &DocumentMut) -> BTreeSet<String> {
    doc.get("target")
        .and_then(Item::as_array_of_tables)
        .map(|targets| {
            targets
                .iter()
                .filter_map(|target| target.get("name").and_then(Item::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn raw_root_value(doc: &DocumentMut) -> Option<String> {
    doc.get("index")?.as_table_like()?.get("root")?.as_str().map(ToOwned::to_owned)
}

fn raw_remote_draft(doc: &DocumentMut) -> Option<RemoteDraft> {
    let remote = doc
        .get("llm")?
        .as_table_like()?
        .get("embedding")?
        .as_table_like()?
        .get("remote")?
        .as_table_like()?;
    let string = |key: &str| remote.get(key).and_then(Item::as_str).map(str::to_string);
    let model = string("model")?;
    let mode = if let Some(endpoint) = string("endpoint") {
        RemoteMode::Connect(endpoint)
    } else {
        RemoteMode::Ephemeral(string("cookbook")?)
    };
    let batch_size = remote
        .get("batch_size")
        .and_then(Item::as_integer)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or_else(|| RemoteEmbeddingConfig::default().batch_size);
    let concurrency = remote
        .get("concurrency")
        .and_then(Item::as_integer)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or_else(|| {
            RemoteEmbeddingConfig::omitted_concurrency_default(matches!(
                &mode,
                RemoteMode::Connect(_)
            ))
        })
        .max(1);
    let max_batch_chars = remote
        .get("max_batch_chars")
        .and_then(Item::as_integer)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or_else(|| RemoteEmbeddingConfig::default().max_batch_chars)
        .max(1);
    let backend =
        string("backend").and_then(|s| RemoteBackend::from_db_str(&s)).unwrap_or_default();
    Some(RemoteDraft {
        model,
        backend,
        mode,
        query_endpoint: string("query_endpoint"),
        gpu: string("gpu"),
        num_ctx: remote
            .get("num_ctx")
            .and_then(Item::as_integer)
            .and_then(|n| u32::try_from(n).ok()),
        batch_size,
        concurrency,
        max_batch_chars,
        auth_env: string("auth_env"),
    })
}

#[cfg(test)]
mod tests {
    use rag_rat_base::embedding_models::{Backend, EMBEDDING_MODELS};

    use super::*;

    #[test]
    fn ollama_defaults_cover_remote_capable_models_with_matching_dimensions() {
        for spec in EMBEDDING_MODELS {
            let default = ollama_model_for(spec.model_id);
            if spec.backend == Backend::FastEmbed {
                let server_model = default.unwrap_or_else(|| {
                    panic!("missing Ollama default for remote-capable model {}", spec.model_id)
                });
                assert!(
                    OLLAMA_EMBEDDING_MODELS.contains(&server_model),
                    "{server_model} must be listed in curated Ollama models"
                );
                assert_eq!(
                    ollama_model_dim(server_model),
                    Some(spec.dim),
                    "{} must bind to a dimension-compatible Ollama model",
                    spec.model_id
                );
            } else {
                assert_eq!(
                    default, None,
                    "{} is not remote-capable and must not claim an Ollama default",
                    spec.model_id
                );
            }
        }
    }

    #[test]
    fn curated_ollama_models_have_known_dimensions() {
        for model in OLLAMA_EMBEDDING_MODELS {
            assert!(ollama_model_dim(model).is_some(), "{model} is missing its dimension");
        }
    }

    #[test]
    fn ollama_defaults_prefer_same_embedding_family() {
        assert_eq!(ollama_model_for("sentence-transformers/all-MiniLM-L6-v2"), Some("all-minilm"));
        assert_eq!(
            ollama_model_for("BAAI/bge-small-en-v1.5"),
            Some("qllama/bge-small-en-v1.5:f16")
        );
        assert_eq!(
            ollama_model_for("jinaai/jina-embeddings-v2-base-code"),
            Some("ordis/jina-embeddings-v2-base-code")
        );
    }

    #[test]
    fn cookbook_gpu_lists_use_current_provider_tokens() {
        assert!(MODAL_GPUS.contains(&"A10"));
        assert!(MODAL_GPUS.contains(&"H200"));
        assert!(MODAL_GPUS.contains(&"B200+"));
        assert!(!MODAL_GPUS.contains(&"A10G"));

        assert!(RUNPOD_GPUS.contains(&"NVIDIA RTX A4000"));
        assert!(RUNPOD_GPUS.contains(&"NVIDIA GeForce RTX 4090"));
        assert!(RUNPOD_GPUS.contains(&"NVIDIA A100 80GB PCIe"));
        assert!(RUNPOD_GPUS.contains(&"NVIDIA H100 PCIe"));
        assert!(RUNPOD_GPUS.contains(&"NVIDIA H200"));
        assert!(RUNPOD_GPUS.contains(&"NVIDIA B300 SXM6 AC"));
        assert!(!RUNPOD_GPUS.contains(&"NVIDIA A100"));
        assert!(!RUNPOD_GPUS.contains(&"NVIDIA H100"));
        assert!(!RUNPOD_GPUS.contains(&"NVIDIA RTX 4090"));
    }

    /// A7 reconfigure invariant: an EXPLICIT `[index] database` key in the original TOML survives
    /// `patch_existing` byte-for-byte — the wizard never un-migrates a deliberately per-repo config
    /// (and never adds a key to a keyless one; `write_fresh` renders keyless).
    #[test]
    fn patch_existing_preserves_an_explicit_database_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let config_path = dir.path().join("rag-rat.toml");
        let raw = "[index]\nroot = \".\"\ndatabase = \
                   \"custom/index.sqlite\"\n\n[llm.embedding]\nmodel = \
                   \"none\"\n\n[target_bindings]\nrust = [\"src\"]\n";
        std::fs::write(&config_path, raw).unwrap();
        let cfg = rag_rat_base::config::Config::load(&config_path).unwrap();

        let d = WizardDraft::from_existing(raw, &cfg, &config_path);
        // The display seam shows the RESOLVED path (the explicit key, made absolute by load).
        // Normalize separators: the display path uses `\` on Windows (correct for the user), but
        // the suffix check is separator-agnostic.
        assert!(
            d.db_path.replace('\\', "/").ends_with("custom/index.sqlite"),
            "reconfigure displays the explicit database path, got {}",
            d.db_path
        );
        let patched = d.patch_existing(raw).unwrap();
        assert!(
            patched.contains("database = \"custom/index.sqlite\""),
            "the explicit key must survive a reconfigure untouched:\n{patched}"
        );

        // The fresh-write path is the keyless counterpart: no active `database` key rendered.
        let fresh = d.write_fresh();
        assert!(
            !fresh.lines().any(|line| line.trim_start().starts_with("database")),
            "write_fresh must render a keyless config:\n{fresh}"
        );
    }

    #[test]
    fn from_config_recovers_bindings_and_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let config_path = dir.path().join("rag-rat.toml");
        std::fs::write(
            &config_path,
            "[index]\nroot = \".\"\n[llm.embedding]\nmodel = \"none\"\n[target_bindings]\nrust = \
             [\"src\"]\n",
        )
        .unwrap();
        let cfg = rag_rat_base::config::Config::load(&config_path).unwrap();
        let d = WizardDraft::from_config(&cfg, &config_path);
        assert_eq!(d.model, "none");
        assert_eq!(d.bindings.get(&Language::Rust).unwrap(), &vec![std::path::PathBuf::from(
            "src"
        )]);
        // root_value must be the relative TOML value, not an absolute path.
        assert_eq!(
            d.root_value, ".",
            "root_value should be relative \".\", got {:?}",
            d.root_value
        );
        // root_abs must be the absolute canonical path.
        assert!(d.root_abs.is_absolute(), "root_abs should be absolute, got {:?}", d.root_abs);
        assert_eq!(d.root_abs, cfg.root);
    }

    #[test]
    fn from_config_maps_remote_connect_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let config_path = dir.path().join("rag-rat.toml");
        std::fs::write(
            &config_path,
            "[index]\nroot = \".\"\n\
             [target_bindings]\nrust = [\"src\"]\n\n\
             [llm.embedding]\nmodel = \"sentence-transformers/all-MiniLM-L6-v2\"\n\n\
             [llm.embedding.remote]\nmodel = \"all-minilm\"\nendpoint = \
             \"http://localhost:11434\"\nnum_ctx = 4096\nbatch_size = 64\nconcurrency = \
             8\nmax_batch_chars = 96000\n",
        )
        .unwrap();
        let cfg = rag_rat_base::config::Config::load(&config_path).unwrap();
        let d = WizardDraft::from_config(&cfg, &config_path);
        let remote = d.remote.expect("remote block should be present");
        assert_eq!(remote.model, "all-minilm");
        assert_eq!(remote.num_ctx, Some(4096));
        assert_eq!(remote.concurrency, 8);
        assert_eq!(remote.max_batch_chars, 96_000);
        assert!(
            matches!(remote.mode, RemoteMode::Connect(ref ep) if ep == "http://localhost:11434")
        );
    }

    #[test]
    fn from_config_maps_oracle_and_version_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let config_path = dir.path().join("rag-rat.toml");
        std::fs::write(
            &config_path,
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n\n[llm.embedding]\nmodel \
             = \"none\"\n\n[oracle]\nauto_run = true\nauto_run_quiet_period_secs = \
             300\nauto_run_min_interval_secs = 7200\n\n[version_check]\nenabled = false\n",
        )
        .unwrap();
        let cfg = rag_rat_base::config::Config::load(&config_path).unwrap();
        let d = WizardDraft::from_config(&cfg, &config_path);
        assert!(d.oracle_auto_run);
        assert_eq!(d.oracle_quiet_secs, 300);
        assert_eq!(d.oracle_min_interval_secs, 7200);
        assert!(!d.version_check);
    }

    #[test]
    fn patch_preserves_comments_and_unknown_keys() {
        let original = "# my notes\n[index]\nroot = \".\"\n[future]\nthing = \
                        1\n[target_bindings]\nrust = [\"src\"]\n";
        let mut d =
            WizardDraft::from_scan(&RepoScan::default(), ".".into(), std::path::PathBuf::from("."));
        d.bindings.insert(rag_rat_base::language::Language::Rust, vec!["crates".into()]);
        let out = d.patch_existing(original).unwrap();
        assert!(out.contains("# my notes"), "comment must be kept");
        assert!(out.contains("[future]"), "unknown table must be kept");
        assert!(out.contains("rust = [\"crates\"]"), "owned binding must be patched");
    }

    #[test]
    fn patch_replaces_non_table_target_bindings() {
        // `target_bindings` is a scalar string — not a table. The patch must overwrite it with
        // a proper table containing the draft's bindings rather than silently skipping the write.
        let original = "[index]\nroot = \".\"\n[llm.embedding]\nmodel = \"none\"\ntarget_bindings \
                        = \"garbage\"\n[oracle]\nauto_run = false\n[version_check]\nenabled = \
                        true\n";
        let mut d =
            WizardDraft::from_scan(&RepoScan::default(), ".".into(), std::path::PathBuf::from("."));
        d.bindings.insert(Language::Rust, vec!["src".into()]);
        let out = d.patch_existing(original).unwrap();
        let patched: toml_edit::DocumentMut = out.parse().expect("output must be valid TOML");
        assert!(
            patched["target_bindings"].is_table(),
            "target_bindings must be a table after patch, got: {:?}",
            patched["target_bindings"]
        );
        let rust_val = &patched["target_bindings"]["rust"];
        assert!(
            rust_val.is_value(),
            "rust binding must be present in patched table, got: {:?}",
            rust_val
        );
    }

    #[test]
    fn fresh_write_persists_remote_and_version_check() {
        let mut d =
            WizardDraft::from_scan(&RepoScan::default(), ".".into(), std::path::PathBuf::from("."));
        d.bindings.insert(Language::Rust, vec!["src".into()]);
        d.model = "sentence-transformers/all-MiniLM-L6-v2".to_string();
        d.version_check = false;
        d.remote = Some(RemoteDraft {
            model: "all-minilm".to_string(),
            backend: RemoteBackend::Ollama,
            mode: RemoteMode::Ephemeral("@rag-rat/cookbook modal".to_string()),
            query_endpoint: None,
            gpu: None,
            num_ctx: None,
            batch_size: 128,
            concurrency: 16,
            max_batch_chars: 192_000,
            auth_env: Some("OLLAMA_TOKEN".to_string()),
        });

        let out = d.write_fresh();
        let doc: DocumentMut = out.parse().unwrap();
        let remote = doc["llm"]["embedding"]["remote"].as_table_like().unwrap();

        assert_eq!(remote.get("cookbook").and_then(Item::as_str), Some("@rag-rat/cookbook modal"));
        assert_eq!(remote.get("backend").and_then(Item::as_str), Some("ollama"));
        assert_eq!(remote.get("batch_size").and_then(Item::as_integer), Some(128));
        assert_eq!(remote.get("concurrency").and_then(Item::as_integer), Some(16));
        assert_eq!(remote.get("max_batch_chars").and_then(Item::as_integer), Some(192_000));
        assert_eq!(remote.get("auth_env").and_then(Item::as_str), Some("OLLAMA_TOKEN"));
        assert_eq!(doc["version_check"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn patch_updates_inline_remote_table() {
        let original = "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n\
                        [llm.embedding]\nmodel = \"sentence-transformers/all-MiniLM-L6-v2\"\n\
                        remote = { model = \"old\", endpoint = \"http://old:11434\" }\n";
        let mut d =
            WizardDraft::from_scan(&RepoScan::default(), ".".into(), std::path::PathBuf::from("."));
        d.bindings.insert(Language::Rust, vec!["src".into()]);
        d.model = "sentence-transformers/all-MiniLM-L6-v2".to_string();
        d.remote = Some(RemoteDraft {
            model: "all-minilm".to_string(),
            backend: RemoteBackend::Ollama,
            mode: RemoteMode::Connect("http://new:11434".to_string()),
            query_endpoint: None,
            gpu: None,
            num_ctx: Some(4096),
            batch_size: 64,
            concurrency: 12,
            max_batch_chars: 144_000,
            auth_env: None,
        });

        let out = d.patch_existing(original).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        let remote = doc["llm"]["embedding"]["remote"].as_table_like().unwrap();

        assert_eq!(remote.get("endpoint").and_then(Item::as_str), Some("http://new:11434"));
        assert_eq!(remote.get("backend").and_then(Item::as_str), Some("ollama"));
        assert_eq!(remote.get("model").and_then(Item::as_str), Some("all-minilm"));
        assert_eq!(remote.get("num_ctx").and_then(Item::as_integer), Some(4096));
        assert_eq!(remote.get("concurrency").and_then(Item::as_integer), Some(12));
        assert_eq!(remote.get("max_batch_chars").and_then(Item::as_integer), Some(144_000));
        assert!(remote.get("cookbook").is_none());
    }

    #[test]
    fn remote_backend_round_trips_through_raw_and_patch() {
        // The wizard-owned `[remote] backend` key must survive a patch write and a raw re-parse.
        for (backend, token) in [
            (RemoteBackend::Ollama, "ollama"),
            (RemoteBackend::Infinity, "infinity"),
            (RemoteBackend::Vllm, "vllm"),
        ] {
            let mut d = WizardDraft::from_scan(
                &RepoScan::default(),
                ".".into(),
                std::path::PathBuf::from("."),
            );
            d.bindings.insert(Language::Rust, vec!["src".into()]);
            d.model = "sentence-transformers/all-MiniLM-L6-v2".to_string();
            let mode = RemoteMode::Ephemeral("@rag-rat/cookbook modal".to_string());
            d.remote = Some(RemoteDraft {
                model: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
                backend,
                query_endpoint: wizard_query_endpoint(&mode, backend).map(str::to_string),
                mode,
                gpu: None,
                num_ctx: None,
                batch_size: 256,
                concurrency: 32,
                max_batch_chars: 192_000,
                auth_env: None,
            });

            let out = d.write_fresh();
            let doc: DocumentMut = out.parse().unwrap();
            assert_eq!(
                doc["llm"]["embedding"]["remote"]["backend"].as_str(),
                Some(token),
                "backend must render to `{token}`"
            );
            let parsed = raw_remote_draft(&doc).expect("remote block must re-parse");
            assert_eq!(parsed.backend, backend, "backend must round-trip via raw_remote_draft");
        }
    }

    #[test]
    fn raw_remote_draft_defaults_backend_to_ollama_when_absent() {
        // A pre-backend-selector `[remote]` block (no `backend` key) must default to ollama.
        let raw = "[index]\nroot = \".\"\n[target_bindings]\nrust = \
                   [\"src\"]\n[llm.embedding]\nmodel = \
                   \"sentence-transformers/all-MiniLM-L6-v2\"\n[llm.embedding.remote]\nmodel = \
                   \"all-minilm\"\nendpoint = \"http://localhost:11434\"\n";
        let doc: DocumentMut = raw.parse().unwrap();
        assert_eq!(raw_remote_draft(&doc).unwrap().backend, RemoteBackend::Ollama);
    }

    #[test]
    fn from_config_reads_infinity_backend() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let config_path = dir.path().join("rag-rat.toml");
        std::fs::write(
            &config_path,
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n\n[llm.embedding]\nmodel \
             = \"sentence-transformers/all-MiniLM-L6-v2\"\n\n[llm.embedding.remote]\nmodel = \
             \"sentence-transformers/all-MiniLM-L6-v2\"\nbackend = \"infinity\"\nendpoint = \
             \"http://localhost:7997\"\nquery_endpoint = \"http://localhost:7997\"\n",
        )
        .unwrap();
        let cfg = rag_rat_base::config::Config::load(&config_path).unwrap();
        let d = WizardDraft::from_existing(
            &std::fs::read_to_string(&config_path).unwrap(),
            &cfg,
            &config_path,
        );
        assert_eq!(d.remote.unwrap().backend, RemoteBackend::Infinity);
    }

    #[test]
    fn wizard_written_ephemeral_infinity_config_carries_query_endpoint_and_loads() {
        // Regression: an ephemeral infinity/vLLM config MUST carry a `query_endpoint` or
        // `Config::load` rejects it (`RemoteQueryEndpointRequiredForBackend`). The wizard writes
        // the backend's default local port so the generated config loads out of the box.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let config_path = dir.path().join("rag-rat.toml");
        let mut d =
            WizardDraft::from_scan(&RepoScan::default(), ".".into(), dir.path().to_path_buf());
        d.bindings.insert(Language::Rust, vec!["src".into()]);
        d.model = "sentence-transformers/all-MiniLM-L6-v2".to_string();
        d.remote = Some(RemoteDraft {
            model: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            backend: RemoteBackend::Infinity,
            mode: RemoteMode::Ephemeral("@rag-rat/cookbook modal".to_string()),
            query_endpoint: Some("http://localhost:7997".to_string()),
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            auth_env: None,
        });
        let out = d.write_fresh();
        std::fs::write(&config_path, &out).unwrap();

        let doc: DocumentMut = out.parse().unwrap();
        let remote = doc["llm"]["embedding"]["remote"].as_table_like().unwrap();
        assert_eq!(remote.get("backend").and_then(Item::as_str), Some("infinity"));
        assert_eq!(
            remote.get("query_endpoint").and_then(Item::as_str),
            Some("http://localhost:7997"),
            "ephemeral infinity must get a default query_endpoint"
        );

        // The written config must LOAD — this is exactly what regressed without the query_endpoint.
        let cfg = rag_rat_base::config::Config::load(&config_path)
            .expect("wizard-written ephemeral infinity config must load");
        let r = cfg.llm.embedding.remote.expect("remote present");
        assert_eq!(r.backend, RemoteBackend::Infinity);
        assert_eq!(r.query_endpoint.as_deref(), Some("http://localhost:7997"));
    }

    #[test]
    fn from_existing_recovers_custom_query_endpoint() {
        // A custom ephemeral `query_endpoint` in the raw TOML must land on the draft (via
        // `raw_remote_draft`), so a reconfigure switching ephemeral→connect can reuse it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let config_path = dir.path().join("rag-rat.toml");
        let raw = "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n\n[llm.embedding]\n\
                   model = \"sentence-transformers/all-MiniLM-L6-v2\"\n\n[llm.embedding.remote]\n\
                   model = \"sentence-transformers/all-MiniLM-L6-v2\"\nbackend = \"infinity\"\n\
                   cookbook = \"@rag-rat/cookbook modal\"\nquery_endpoint = \
                   \"http://gpu-box.local:9999\"\n";
        std::fs::write(&config_path, raw).unwrap();
        let cfg = Config::load(&config_path).unwrap();

        let d = WizardDraft::from_existing(raw, &cfg, &config_path);
        let remote = d.remote.expect("remote present");
        assert_eq!(remote.backend, RemoteBackend::Infinity);
        assert_eq!(remote.query_endpoint.as_deref(), Some("http://gpu-box.local:9999"));
    }

    #[test]
    fn patch_drives_ephemeral_query_endpoint_from_the_draft() {
        // `query_endpoint` is DRIVEN by the draft (the single source of truth), NOT recomputed from
        // the raw doc: `patch_existing` writes exactly `remote.query_endpoint` for an EPHEMERAL
        // config (steps.rs owns computing the per-backend default / preserving a custom value), and
        // drops it entirely for a CONNECT config. Whatever the raw doc held is overwritten.
        let raw = "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n\
                   [llm.embedding]\nmodel = \"sentence-transformers/all-MiniLM-L6-v2\"\n\
                   [llm.embedding.remote]\nmodel = \"x\"\nbackend = \"infinity\"\ncookbook = \
                   \"@rag-rat/cookbook modal\"\nquery_endpoint = \"http://stale:1234\"\n";
        let mut d = WizardDraft::from_scan(&RepoScan::default(), ".".into(), PathBuf::from("."));
        d.bindings.insert(Language::Rust, vec!["src".into()]);
        let mut remote = RemoteDraft {
            model: "x".to_string(),
            backend: RemoteBackend::Vllm,
            mode: RemoteMode::Ephemeral("@rag-rat/cookbook modal".to_string()),
            query_endpoint: Some("http://localhost:8000".to_string()),
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            auth_env: None,
        };
        d.remote = Some(remote.clone());
        let q = |out: &str| -> Option<String> {
            let doc: DocumentMut = out.parse().unwrap();
            doc["llm"]["embedding"]["remote"]
                .as_table_like()
                .unwrap()
                .get("query_endpoint")
                .and_then(Item::as_str)
                .map(str::to_string)
        };

        // Ephemeral with `Some` → the draft value is written, overwriting the stale raw-doc value.
        assert_eq!(q(&d.patch_existing(raw).unwrap()), Some("http://localhost:8000".to_string()));

        // Ephemeral with `None` (e.g. ollama, which uses the config default) → the key is dropped.
        remote.query_endpoint = None;
        d.remote = Some(remote.clone());
        assert_eq!(q(&d.patch_existing(raw).unwrap()), None);

        // CONNECT ignores query_endpoint → the key is dropped even if the draft still carries one.
        remote.mode = RemoteMode::Connect("http://localhost:8000".to_string());
        remote.query_endpoint = Some("http://localhost:8000".to_string());
        d.remote = Some(remote);
        assert_eq!(q(&d.patch_existing(raw).unwrap()), None);
    }

    #[test]
    fn from_existing_keeps_rich_targets_out_of_simple_bindings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let config_path = dir.path().join("rag-rat.toml");
        let raw = "[index]\nroot = \".\"\n[llm.embedding]\nmodel = \"none\"\n[[target]]\nname = \
                   \"core\"\nlanguage = \"rust\"\ndirectories = [\"src\"]\n";
        std::fs::write(&config_path, raw).unwrap();
        let cfg = Config::load(&config_path).unwrap();

        let d = WizardDraft::from_existing(raw, &cfg, &config_path);
        let out = d.patch_existing(raw).unwrap();

        assert!(d.bindings.is_empty());
        assert!(d.has_rich_targets);
        assert!(d.rich_target_names.contains("core"));
        assert!(out.contains("[[target]]"));
        assert!(!out.contains("[target_bindings]"));
    }

    #[test]
    fn from_existing_preserves_raw_root_literal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crate/src")).unwrap();
        let config_path = dir.path().join("crate/rag-rat.toml");
        let raw = "[index]\nroot = \".\"\n[target_bindings]\nrust = \
                   [\"src\"]\n[llm.embedding]\nmodel = \"none\"\n";
        std::fs::write(&config_path, raw).unwrap();
        let cfg = Config::load(&config_path).unwrap();

        let d = WizardDraft::from_existing(raw, &cfg, &config_path);
        let out = d.patch_existing(raw).unwrap();
        let doc: DocumentMut = out.parse().unwrap();

        assert_eq!(d.root_value, ".");
        assert_eq!(doc["index"]["root"].as_str(), Some("."));
    }

    #[test]
    fn from_existing_preserves_relative_cookbook_literal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("recipe.mjs"), "export {}\n").unwrap();
        let config_path = dir.path().join("rag-rat.toml");
        let raw = "[index]\nroot = \".\"\n[target_bindings]\nrust = \
                   [\"src\"]\n[llm.embedding]\nmodel = \
                   \"sentence-transformers/all-MiniLM-L6-v2\"\n[llm.embedding.remote]\nmodel = \
                   \"all-minilm\"\ncookbook = \"./recipe.mjs\"\n";
        std::fs::write(&config_path, raw).unwrap();
        let cfg = Config::load(&config_path).unwrap();
        let resolved = cfg.llm.embedding.remote.as_ref().unwrap().cookbook.as_deref().unwrap();
        assert!(PathBuf::from(resolved).is_absolute());

        let d = WizardDraft::from_existing(raw, &cfg, &config_path);
        let out = d.patch_existing(raw).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        let remote = doc["llm"]["embedding"]["remote"].as_table_like().unwrap();

        assert!(matches!(
            d.remote.as_ref().map(|remote| &remote.mode),
            Some(RemoteMode::Ephemeral(cookbook)) if cookbook == "./recipe.mjs"
        ));
        assert_eq!(d.remote.as_ref().map(|remote| remote.concurrency), Some(32));
        assert_eq!(remote.get("cookbook").and_then(Item::as_str), Some("./recipe.mjs"));
        assert_eq!(remote.get("concurrency").and_then(Item::as_integer), Some(32));
    }

    #[test]
    fn from_existing_defaults_legacy_connect_concurrency_to_single_flight() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let config_path = dir.path().join("rag-rat.toml");
        let raw = "[index]\nroot = \".\"\n[target_bindings]\nrust = \
                   [\"src\"]\n[llm.embedding]\nmodel = \
                   \"sentence-transformers/all-MiniLM-L6-v2\"\n[llm.embedding.remote]\nmodel = \
                   \"all-minilm\"\nendpoint = \"http://localhost:11434\"\n";
        std::fs::write(&config_path, raw).unwrap();
        let cfg = Config::load(&config_path).unwrap();
        assert_eq!(cfg.llm.embedding.remote.as_ref().unwrap().concurrency, 1);

        let d = WizardDraft::from_existing(raw, &cfg, &config_path);
        let out = d.patch_existing(raw).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        let remote = doc["llm"]["embedding"]["remote"].as_table_like().unwrap();

        assert!(matches!(
            d.remote.as_ref().map(|remote| &remote.mode),
            Some(RemoteMode::Connect(endpoint)) if endpoint == "http://localhost:11434"
        ));
        assert_eq!(d.remote.as_ref().map(|remote| remote.concurrency), Some(1));
        assert_eq!(remote.get("endpoint").and_then(Item::as_str), Some("http://localhost:11434"));
        assert_eq!(remote.get("concurrency").and_then(Item::as_integer), Some(1));
    }

    #[test]
    fn from_scan_selects_default_dirs_and_backend() {
        use crate::init::scan::add_file_to_dir_counts;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Create the source files so candidate_dirs can stat them.
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();

        let mut scan = RepoScan::default();
        *scan.language_counts_mut().entry(Language::Rust).or_default() += 2;
        add_file_to_dir_counts(root, &root.join("src/lib.rs"), Language::Rust, &mut scan).unwrap();
        add_file_to_dir_counts(root, &root.join("src/main.rs"), Language::Rust, &mut scan).unwrap();
        scan.set_total_source_bytes(1_000);

        let root_abs = root.to_path_buf();
        let d = WizardDraft::from_scan(&scan, ".".to_string(), root_abs.clone());
        assert!(d.bindings.contains_key(&Language::Rust));
        assert_eq!(d.bindings[&Language::Rust], vec![PathBuf::from("src")]);
        // Small repo (1 KB) → fast_embed (MiniLM).
        assert!(d.model.contains("MiniLM"), "expected MiniLM for small repo, got {}", d.model);
        assert!(!d.oracle_auto_run);
        assert!(d.version_check);
        assert!(!d.hooks.git);
        // root_abs must be the absolute path passed in.
        assert_eq!(d.root_abs, root_abs);
    }
}
