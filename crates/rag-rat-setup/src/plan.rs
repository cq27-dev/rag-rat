//! The setup plan: what a repository scan found, and the default configuration derived from it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rag_rat_base::config::EmbeddingBackend;
use rag_rat_base::language::Language;

use crate::render::supported_languages;
use crate::scan::{estimated_chunks, recommend_backend, resolved_bindings};

/// Directories the scan never descends into.
pub const SKIPPED_DIRS: &[&str] = &[
    ".git",
    ".rag-rat",
    ".direnv",
    ".next",
    ".turbo",
    ".venv",
    // Python virtualenv / dependency / cache trees — never project source. Skipping them at scan
    // time means their `.py` files never become dir candidates, so the "no default → promote the
    // largest" fallback can't select a `site-packages` tree (#167 review). NB: `virtualenv` is NOT
    // here — it's a real first-party package name; a virtualenv literally named `virtualenv/` is
    // caught by content (`pyvenv.cfg`) in `scan_dir` instead (#181).
    "venv",
    "site-packages",
    "__pycache__",
    ".tox",
    ".nox",
    "build",
    "dist",
    "node_modules",
    "target",
    // AI tooling directories — never project source (like .rag-rat above).
    ".claude",
    ".codex",
    ".omc",
    ".omx",
];

#[derive(Debug, Clone)]
pub struct InitPlan {
    pub root_value: String,
    pub languages: Vec<Language>,
    pub bindings: BTreeMap<Language, Vec<PathBuf>>,
    pub backend: EmbeddingBackend,
    /// Whether to write `[oracle] auto_run = true` — the opt-in background refresh of
    /// compiler-grade (SCIP) importance ranking. Default false (matches `OracleConfig`'s
    /// default).
    pub oracle_auto_run: bool,
    /// Whether to write `[llm.distill] enabled = true` — the opt-in distillation model pass.
    /// Default false; the wizard only turns it on with a resolvable issue tracker.
    pub distill_enabled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RepoScan {
    pub language_counts: BTreeMap<Language, usize>,
    pub dir_counts: BTreeMap<Language, BTreeMap<PathBuf, usize>>,
    pub direct_dir_counts: BTreeMap<Language, BTreeMap<PathBuf, usize>>,
    pub total_source_bytes: u64,
    /// The scan found a real Python virtualenv (a dir with a `pyvenv.cfg`) ANYWHERE the index
    /// would walk — not gitignored, not floored. The indexer floor can't cover a venv under an
    /// ambiguous name (`env`/`virtualenv`), so a `python = ["."]` walk WOULD index it; when
    /// one is present we must not auto-bind `.`. Gitignored / conventionally-floored venvs
    /// (`.venv`/`venv`) and first-party packages that merely share a venv-ish NAME (no
    /// `pyvenv.cfg`) are NOT recorded here — content detection, not the name, decides (#181
    /// review).
    pub has_python_virtualenv: bool,
    /// Full paths of ambiguous `.h` headers, held aside during the walk and assigned to a language
    /// only after the whole repo is seen: to **C++** if the repo has any C++ source
    /// (`.cpp`/`.cc`/…), else to **C**. Bare `Language::from_path` calls every `.h` C, which would
    /// bind a C++ library's header tree as `c` and parse it as C — see [`scan::assign_headers`].
    pub deferred_headers: Vec<PathBuf>,
    /// Per-language manifest root directories found during the walk (e.g. a dir holding a
    /// `go.mod`) — used to seed default bindings for languages whose source layout is
    /// manifest-anchored rather than dir-count-anchored. `BTreeMap`/`BTreeSet` (not
    /// `HashMap`/`HashSet`) for deterministic iteration order — same-scan-twice must yield
    /// identical output (BIND-08).
    pub manifest_roots: BTreeMap<Language, BTreeSet<PathBuf>>,
}

impl RepoScan {
    /// Read-only access to language file counts — used by `wizard/draft.rs` to build
    /// `SetupDraft::from_scan` without re-implementing `default_plan`'s language filtering.
    pub fn language_counts(&self) -> &BTreeMap<Language, usize> {
        &self.language_counts
    }

    /// Mutable access for tests in `wizard/draft.rs` that construct a `RepoScan` directly.
    #[cfg(test)]
    pub fn language_counts_mut(&mut self) -> &mut BTreeMap<Language, usize> {
        &mut self.language_counts
    }

    /// Total source bytes scanned — used by `wizard/draft.rs` to call `estimated_chunks`.
    pub fn total_source_bytes(&self) -> u64 {
        self.total_source_bytes
    }

    /// Mutable setter for tests in `wizard/draft.rs`.
    #[cfg(test)]
    pub fn set_total_source_bytes(&mut self, n: u64) {
        self.total_source_bytes = n;
    }
}

#[derive(Debug, Clone)]
pub struct DirCandidate {
    pub path: PathBuf,
    pub count: usize,
    pub default: bool,
}

pub fn default_plan(root_value: String, scan: &RepoScan) -> InitPlan {
    let languages = supported_languages()
        .into_iter()
        .filter(|language| scan.language_counts.get(language).copied().unwrap_or_default() > 0)
        .collect::<Vec<_>>();
    let languages = if languages.is_empty() { vec![Language::Rust] } else { languages };
    let bindings: BTreeMap<Language, Vec<PathBuf>> = languages
        .iter()
        .filter_map(|language| {
            let defaults = resolved_bindings(scan, *language);
            if defaults.is_empty() { None } else { Some((*language, defaults)) }
        })
        .collect();
    // Keep `languages` consistent with the bindings actually emitted (a dropped env-only Python
    // must not linger in the language list).
    let languages =
        languages.into_iter().filter(|language| bindings.contains_key(language)).collect();
    let backend = recommend_backend(estimated_chunks(scan.total_source_bytes));
    // Non-interactive default mirrors `OracleConfig`'s default: off until explicitly enabled.
    InitPlan {
        root_value,
        languages,
        bindings,
        backend,
        oracle_auto_run: false,
        distill_enabled: false,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rag_rat_base::config::Config;

    use super::*;
    use crate::render::{config_root_value, render_config};
    use crate::scan::{candidate_dirs, estimated_chunks, recommend_backend};

    #[test]
    fn render_config_uses_selected_language_bindings() {
        let plan = InitPlan {
            root_value: ".".to_string(),
            languages: vec![Language::Rust, Language::TypeScript],
            bindings: BTreeMap::from([
                (Language::Rust, vec![PathBuf::from("crates/app/src")]),
                (Language::TypeScript, vec![PathBuf::from("web/src"), PathBuf::from("app/src")]),
            ]),
            backend: EmbeddingBackend::model2vec(),
            oracle_auto_run: false,
            distill_enabled: false,
        };

        let text = render_config(&plan);

        assert!(text.contains("[index]"));
        // A7: NO active `database` key — the keyless config resolves to the machine-global store.
        // The deprecated per-repo opt-out is documented as a COMMENT only, so check for an active
        // (uncommented) key, not the substring.
        assert!(
            !text.lines().any(|line| line.trim_start().starts_with("database")),
            "a fresh config must not activate the deprecated per-repo `database` key:\n{text}"
        );
        assert!(
            text.contains("# database = \".rag-rat/index.sqlite\""),
            "the per-repo opt-out stays discoverable as a comment"
        );
        assert!(text.contains("rust = [\"crates/app/src\"]"));
        assert!(text.contains("typescript = [\"web/src\", \"app/src\"]"));
        assert!(text.contains("[llm.embedding]"));
        // The selector is now the model_id (HF path), not an alias (#317).
        assert!(text.contains("model = \"minishlab/potion-retrieval-32M\""));
        // The oracle section is always rendered so the knob is discoverable; default is OFF.
        assert!(text.contains("[oracle]"));
        assert!(text.contains("auto_run = false"));
    }

    #[test]
    fn render_config_enables_oracle_auto_run_when_opted_in() {
        let plan = InitPlan {
            root_value: ".".to_string(),
            languages: vec![Language::Rust],
            bindings: BTreeMap::from([(Language::Rust, vec![PathBuf::from("src")])]),
            backend: EmbeddingBackend::fast_embed(),
            oracle_auto_run: true,
            distill_enabled: false,
        };
        assert!(render_config(&plan).contains("auto_run = true"));
    }

    #[test]
    fn rendered_swift_binding_round_trips_with_default_glob() {
        let root = rag_rat_base::test_scratch::ScratchDir::new("render-swift");
        std::fs::create_dir_all(root.join("Sources/App")).unwrap();
        std::fs::write(root.join("Sources/App/App.swift"), "struct App {}\n").unwrap();
        let plan = InitPlan {
            root_value: ".".to_string(),
            languages: vec![Language::Swift],
            bindings: BTreeMap::from([(Language::Swift, vec![PathBuf::from("Sources")])]),
            backend: EmbeddingBackend::fast_embed(),
            oracle_auto_run: false,
            distill_enabled: false,
        };

        let text = render_config(&plan);
        assert!(text.contains("swift = [\"Sources\"]"));
        std::fs::write(root.join("rag-rat.toml"), text).unwrap();
        let config = Config::load(root.join("rag-rat.toml")).unwrap();
        assert_eq!(config.targets.len(), 1);
        assert_eq!(config.targets[0].language, Language::Swift);
        assert_eq!(config.targets[0].directories, vec![PathBuf::from("Sources")]);
        assert_eq!(config.targets[0].include, vec!["**/*.swift"]);
    }

    #[test]
    fn render_config_emits_full_commented_surface_that_round_trips() {
        // The generated config documents the full surface (commented), still parses via
        // Config::load, and the example [[target]] / [watch] / [version_check] tables stay
        // COMMENTED — only the active bindings + model take effect.
        let root = rag_rat_base::test_scratch::ScratchDir::new("render");
        std::fs::create_dir_all(root.join("include")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let plan = InitPlan {
            root_value: ".".to_string(),
            languages: vec![Language::Cpp],
            bindings: BTreeMap::from([(Language::Cpp, vec![
                PathBuf::from("include"),
                PathBuf::from("src"),
            ])]),
            backend: EmbeddingBackend::fast_embed(),
            oracle_auto_run: false,
            distill_enabled: false,
        };
        let text = render_config(&plan);
        assert!(text.contains("# [[target]]"), "documents the expanded target form");
        assert!(text.contains("# [watch]"));
        assert!(text.contains("# [version_check]"));
        assert!(text.contains("# [log]"));
        assert!(text.contains("# [llm.embedding.runtime]"));
        assert!(text.contains("# [init.cookbooks.modal]"));
        assert!(text.contains("# [init.cookbooks.my-provider]"));
        assert!(text.contains("`.h`"), "explains the cpp .h-header binding");

        std::fs::write(root.join("rag-rat.toml"), &text).unwrap();
        let config = Config::load(root.join("rag-rat.toml")).unwrap();
        // Exactly the one active cpp target — the example [[target]] stayed commented.
        assert_eq!(config.targets.len(), 1);
        assert_eq!(config.targets[0].language, Language::Cpp);
        // Commented [watch] falls back to its default (enabled).
        assert!(config.watch.enabled);
        // Commented [log] falls back to its default (disabled).
        assert!(!config.log.enabled);
    }

    /// A7: the freshly rendered config has NO `database` key, so `Config::load` resolves it to the
    /// consolidated GLOBAL store — the flip covers the primary onboarding path, not just
    /// hand-written configs. Path resolution only; nothing is created at the global path.
    #[test]
    fn rendered_config_is_keyless_and_resolves_to_the_global_database() {
        let root = rag_rat_base::test_scratch::ScratchDir::new("render-globaldb");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        // Identity-bearing root: the global default requires a derivable repo identity (a
        // committed git repo); an identity-less root stays per-root.
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@e"],
            &["config", "user.name", "t"],
            &["add", "-A"],
            &["commit", "-qm", "seed"],
        ] {
            rag_rat_base::test_git::run(&root, args);
        }
        let plan = InitPlan {
            root_value: ".".to_string(),
            languages: vec![Language::Rust],
            bindings: BTreeMap::from([(Language::Rust, vec![PathBuf::from("src")])]),
            backend: EmbeddingBackend::fast_embed(),
            oracle_auto_run: false,
            distill_enabled: false,
        };
        std::fs::write(root.join("rag-rat.toml"), render_config(&plan)).unwrap();

        let config = Config::load(root.join("rag-rat.toml")).unwrap();
        assert_eq!(
            config.database,
            rag_rat_base::data_dir::global_database_path()
                .expect("a data dir resolves in the test environment"),
            "a fresh init's keyless config lands on the machine-global store",
        );
    }

    #[test]
    fn config_load_ignores_active_init_cookbook_catalog() {
        let root = rag_rat_base::test_scratch::ScratchDir::new("init-catalog");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("rag-rat.toml"),
            r#"
            [index]
            root = "."

            [target_bindings]
            rust = ["src"]

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [init.cookbooks.modal]
            gpus = ["tiny", "huge"]

            [init.cookbooks.custom]
            label = "Custom"
            command = "./recipes/custom.mjs"
            gpus = []
            "#,
        )
        .unwrap();

        let config = Config::load(root.join("rag-rat.toml")).unwrap();

        assert_eq!(config.targets.len(), 1);
        assert_eq!(
            config.llm.embedding.backend.as_config_str(),
            "sentence-transformers/all-MiniLM-L6-v2"
        );
    }

    #[test]
    fn recommend_backend_scales_with_repo_size() {
        assert_eq!(recommend_backend(estimated_chunks(500_000)), EmbeddingBackend::fast_embed());
        assert_eq!(recommend_backend(estimated_chunks(50_000_000)), EmbeddingBackend::model2vec());
    }

    #[test]
    fn default_plan_selects_detected_src_dirs() {
        let scan = RepoScan {
            language_counts: BTreeMap::from([(Language::Rust, 2), (Language::Markdown, 1)]),
            dir_counts: BTreeMap::from([
                (
                    Language::Rust,
                    BTreeMap::from([(PathBuf::from("."), 2), (PathBuf::from("src"), 2)]),
                ),
                (
                    Language::Markdown,
                    BTreeMap::from([(PathBuf::from("."), 1), (PathBuf::from("docs"), 1)]),
                ),
            ]),
            direct_dir_counts: BTreeMap::new(),
            total_source_bytes: 0,
            has_python_virtualenv: false,
            deferred_headers: Vec::new(),
            manifest_roots: BTreeMap::new(),
        };

        let plan = default_plan(".".to_string(), &scan);

        assert_eq!(plan.languages, vec![Language::Rust, Language::Markdown]);
        assert_eq!(plan.bindings[&Language::Rust], vec![PathBuf::from("src")]);
        // `.` recursively covers `docs`, so `dedup_ancestors` (BIND-07) collapses the
        // redundant descendant binding — `.` alone is the correct default set.
        assert_eq!(plan.bindings[&Language::Markdown], vec![PathBuf::from(".")]);
    }

    #[test]
    fn c_defaults_include_direct_source_feature_dirs() {
        let scan = RepoScan {
            language_counts: BTreeMap::from([(Language::C, 10)]),
            dir_counts: BTreeMap::from([(
                Language::C,
                BTreeMap::from([
                    (PathBuf::from("."), 10),
                    (PathBuf::from("drivers"), 1),
                    (PathBuf::from("drivers/entropy"), 1),
                    (PathBuf::from("samples"), 9),
                    (PathBuf::from("samples/simple_txrx"), 9),
                    (PathBuf::from("samples/simple_txrx/src"), 9),
                ]),
            )]),
            direct_dir_counts: BTreeMap::from([(
                Language::C,
                BTreeMap::from([
                    (PathBuf::from("drivers/entropy"), 1),
                    (PathBuf::from("samples/simple_txrx/src"), 1),
                ]),
            )]),
            total_source_bytes: 0,
            has_python_virtualenv: false,
            deferred_headers: Vec::new(),
            manifest_roots: BTreeMap::new(),
        };

        let defaults = candidate_dirs(&scan, Language::C)
            .into_iter()
            .filter(|candidate| candidate.default)
            .map(|candidate| candidate.path)
            .collect::<Vec<_>>();

        assert!(defaults.contains(&PathBuf::from("drivers/entropy")));
        assert!(defaults.contains(&PathBuf::from("samples/simple_txrx/src")));
        assert!(!defaults.contains(&PathBuf::from("drivers")));
        assert!(!defaults.contains(&PathBuf::from(".")));
    }

    #[test]
    fn nested_config_uses_repo_root_relative_to_config_dir() {
        assert_eq!(config_root_value(Path::new("/repo"), Path::new("profiles/rag-rat.toml")), "..");
        assert_eq!(
            config_root_value(Path::new("/repo"), Path::new("profiles/dev/rag-rat.toml")),
            "../.."
        );
        assert_eq!(config_root_value(Path::new("/repo"), Path::new("rag-rat.toml")), ".");
    }
}
