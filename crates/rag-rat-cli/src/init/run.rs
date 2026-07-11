use super::wizard::{self, HookConflict, WizardResult, render_chained_hook};
use super::*;
use crate::{hook_script, install_hook, is_rag_rat_hook, make_executable, write_atomic};

pub(crate) fn run(args: &crate::cli::InitArgs, config_path: &str) -> anyhow::Result<()> {
    let options = InitOptions::from_args(args, config_path);
    let _terminal_reset = TerminalResetGuard::install_if_interactive(!options.yes)?;
    let root = env::current_dir()?.canonicalize()?;

    // A config authored in a LINKED worktree is a trap: `Config::load`'s governing seam resolves
    // every worktree of a repo through the MAIN worktree's `rag-rat.toml`, so a branch-local file
    // would be ignored the moment main gains one (and governs only by fallback until then).
    // Refuse with the pointer instead of writing a file that doesn't do what it says.
    if let Some(main_root) = rag_rat_core::config::linked_worktree_main_root(&root) {
        anyhow::bail!(
            "this is a linked git worktree; the repo's rag-rat.toml lives in the main worktree — \
             run `rag-rat init` there instead: {}",
            main_root.display()
        );
    }

    // The interactive path runs the full-screen ratatui wizard. `--yes` keeps the legacy
    // `default_plan` + `render_config` flow; `--dry-run` remains interactive unless paired with
    // `--yes`, so the preview reflects the wizard choices.
    if options.yes {
        let scan = scan_repo(&root)?;
        let root_value = config_root_value(&root, &options.config_path);
        return run_non_interactive(&options, root_value, &scan);
    }
    run_interactive(&options)
}

/// The non-interactive (`--yes`) / `--dry-run` flow: render from `default_plan` and either print
/// (dry-run) or write + index + install. Unchanged from the pre-wizard implementation.
fn run_non_interactive(
    options: &InitOptions,
    root_value: String,
    scan: &RepoScan,
) -> anyhow::Result<()> {
    let plan = default_plan(root_value, scan);
    // A plan with no bindings would render an empty `[target_bindings]` and index nothing — a
    // useless config. `init -y` can reach this when every detected language drops to a dependency
    // tree (e.g. Python whose only `.py` live under a virtualenv); the interactive path already
    // bails, so guard the non-interactive path the same way rather than write an unusable file
    // (#181 review).
    if plan.bindings.is_empty() {
        anyhow::bail!(
            "init found no indexable source to bind — every detected language resolved to a \
             dependency tree (e.g. a virtualenv). Nothing to configure."
        );
    }
    let config_text = render_config(&plan);

    if options.dry_run {
        println!("{config_text}");
        return Ok(());
    }

    if options.config_path.exists() && !options.force && !options.yes {
        let overwrite = Confirm::new()
            .with_prompt(format!("Overwrite {}?", options.config_path.display()))
            .default(false)
            .interact()?;
        if !overwrite {
            anyhow::bail!("init cancelled; {} already exists", options.config_path.display());
        }
    }

    if let Some(parent) = options.config_path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(&options.config_path, config_text)?;
    eprintln!("init: wrote {}", options.config_path.display());

    let config = Config::load(&options.config_path)?;
    apply_embedding_runtime_env(&config.llm.embedding.runtime);
    let db = setup_index(&config)?;
    setup_model_and_reconcile(&config, &db, options.yes)?;
    offer_hooks_install(&config, options.yes)?;
    eprintln!("init: complete");
    print_mcp_connect_hint();
    Ok(())
}

/// The interactive flow: run the full-screen ratatui wizard, then write the TOML it returns and
/// apply the hook selections.
///
/// `existing` reconfigure: when a `rag-rat.toml` is already present we load it (and keep its raw
/// text) so the wizard renders a preserving patch / diff; otherwise it is a fresh init. The model
/// is downloaded in-wizard (the Embedding step's verify probe), so the post-write reconcile is
/// fast.
fn run_interactive(options: &InitOptions) -> anyhow::Result<()> {
    let existing = load_existing_for_wizard(options)?;
    let scan_root = interactive_scan_root(Some(&existing))?;
    let scan = scan_repo(&scan_root)?;

    let Some(result) =
        wizard::run_wizard(scan, existing.config, &options.config_path, scan_root.clone())?
    else {
        // The user quit the wizard — write nothing.
        eprintln!("init: cancelled");
        return Ok(());
    };

    if options.dry_run {
        println!("{}", result.toml);
        return Ok(());
    }

    if let Some(parent) = options.config_path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(&options.config_path, &result.toml)?;
    eprintln!("init: wrote {}", options.config_path.display());

    let config = Config::load(&options.config_path)?;
    apply_embedding_runtime_env(&config.llm.embedding.runtime);
    let db = setup_index(&config)?;
    setup_model_and_reconcile(&config, &db, false)?;
    apply_wizard_hooks(&config, &result)?;
    eprintln!("init: complete");
    print_mcp_connect_hint();
    Ok(())
}

struct ExistingWizardConfig {
    config: Option<(String, Config)>,
    local_root: Option<PathBuf>,
}

fn load_existing_for_wizard(options: &InitOptions) -> anyhow::Result<ExistingWizardConfig> {
    if !options.config_path.exists() || options.force {
        return Ok(ExistingWizardConfig { config: None, local_root: None });
    }

    let raw = fs::read_to_string(&options.config_path)?;
    let local_root = local_config_root_from_raw(&raw, &options.config_path)?;
    let config = match Config::load(&options.config_path) {
        Ok(config) => Some((raw, config)),
        Err(err) => {
            eprintln!(
                "init: existing config {} could not be loaded ({err}); starting from a fresh draft",
                options.config_path.display()
            );
            None
        },
    };
    Ok(ExistingWizardConfig { config, local_root })
}

fn interactive_scan_root(existing: Option<&ExistingWizardConfig>) -> anyhow::Result<PathBuf> {
    if let Some(root) = existing.and_then(|existing| existing.local_root.clone()) {
        return Ok(root);
    }
    if let Some((_, config)) = existing.and_then(|existing| existing.config.as_ref()) {
        return Ok(config.root.clone());
    }
    Ok(env::current_dir()?.canonicalize()?)
}

fn local_config_root_from_raw(raw: &str, config_path: &Path) -> anyhow::Result<Option<PathBuf>> {
    let Ok(doc) = raw.parse::<toml_edit::DocumentMut>() else {
        return Ok(None);
    };
    let Some(root) = doc
        .get("index")
        .and_then(|item| item.as_table_like())
        .and_then(|table| table.get("root"))
        .and_then(|item| item.as_str())
    else {
        return Ok(None);
    };
    let root_path = Path::new(root);
    let candidate = if root_path.is_absolute() {
        root_path.to_path_buf()
    } else {
        config_dir(config_path)?.join(root_path)
    };
    match candidate.canonicalize() {
        Ok(path) => Ok(Some(path)),
        Err(_) => Ok(None),
    }
}

fn config_dir(config_path: &Path) -> anyhow::Result<PathBuf> {
    let parent = config_path.parent().filter(|path| !path.as_os_str().is_empty());
    let dir = match (config_path.is_absolute(), parent) {
        (true, Some(parent)) => parent.to_path_buf(),
        (true, None) => PathBuf::from("/"),
        (false, Some(parent)) => env::current_dir()?.join(parent),
        (false, None) => env::current_dir()?,
    };
    Ok(dir.canonicalize().unwrap_or(dir))
}

/// Apply the git maintenance hooks the wizard selected, honoring each foreign-hook conflict
/// resolution.
///
/// Mirrors the install mechanics the `hooks` command uses, but driven by the wizard's `HooksDraft`
/// and per-hook `HookConflict` map instead of re-prompting. Claude/Codex agent hooks are installed
/// by the rag-rat plugin, not here.
fn apply_wizard_hooks(config: &Config, result: &WizardResult) -> anyhow::Result<()> {
    if result.hooks.git {
        apply_git_hooks(config, &result.hook_conflicts)?;
    }
    Ok(())
}

/// Install the rag-rat git maintenance hooks per the wizard's foreign-hook conflict resolutions.
///
/// For each managed hook: a clean slot (or one already managed by rag-rat) installs normally; a
/// foreign file is resolved by the user's [`HookConflict`] choice — `Skip` leaves it, `Overwrite`
/// replaces it, `Chain` wraps it via [`render_chained_hook`], `UninstallRagRatOnly` is a no-op for
/// a foreign file, and `Abort` skips all hook changes.
fn apply_git_hooks(
    config: &Config,
    conflicts: &std::collections::HashMap<&'static str, HookConflict>,
) -> anyhow::Result<()> {
    let git = match git_paths(&config.root) {
        Ok(git) => git,
        Err(err) => {
            eprintln!("init: skipped git hooks (not a git worktree: {err})");
            return Ok(());
        },
    };
    // `Abort` on any resolved conflict means "don't touch hooks at all".
    if conflicts.values().any(|c| *c == HookConflict::Abort) {
        eprintln!("init: skipped git hooks (conflict aborted)");
        return Ok(());
    }
    fs::create_dir_all(&git.hooks_dir)?;
    let mut installed = Vec::new();
    for &hook in crate::MANAGED_HOOKS {
        let path = git.hooks_dir.join(hook);
        let foreign = path.exists() && !is_rag_rat_hook(&path)?;
        match conflicts.get(hook).copied() {
            // A foreign file with an explicit resolution.
            Some(HookConflict::Skip) | Some(HookConflict::UninstallRagRatOnly) => {
                // Leave the foreign hook in place; install nothing for this slot.
            },
            Some(HookConflict::Overwrite) => {
                write_atomic(&path, hook_script(hook).as_bytes())?;
                make_executable(&path)?;
                installed.push(hook);
            },
            Some(HookConflict::Chain) => {
                let original = fs::read_to_string(&path).unwrap_or_default();
                write_atomic(&path, render_chained_hook(&original, hook).as_bytes())?;
                make_executable(&path)?;
                installed.push(hook);
            },
            Some(HookConflict::Abort) => unreachable!("handled above"),
            // No conflict recorded: a clean slot, or one already managed by rag-rat.
            None => {
                if foreign {
                    // A foreign file with no resolution should have been caught by the wizard's
                    // unresolved-conflict gate; be conservative and leave it untouched.
                    eprintln!(
                        "init: leaving unmanaged hook {} in place (no resolution recorded)",
                        path.display()
                    );
                } else {
                    // `install_hook` is safe here: the slot is empty or already a rag-rat hook.
                    install_hook(&git.hooks_dir, hook)?;
                    installed.push(hook);
                }
            },
        }
    }
    eprintln!("init: installed git hooks in {} ({:?})", git.hooks_dir.display(), installed);
    Ok(())
}
pub(crate) fn default_plan(root_value: String, scan: &RepoScan) -> InitPlan {
    let languages = supported_languages()
        .into_iter()
        .filter(|language| scan.language_counts.get(language).copied().unwrap_or_default() > 0)
        .collect::<Vec<_>>();
    let languages = if languages.is_empty() { vec![Language::Rust] } else { languages };
    let bindings: BTreeMap<Language, Vec<PathBuf>> = languages
        .iter()
        .filter_map(|language| {
            let candidates = candidate_dirs(scan, *language);
            let defaults = candidates
                .iter()
                .filter(|candidate| candidate.default)
                .map(|candidate| candidate.path.clone())
                .collect::<Vec<_>>();
            if !defaults.is_empty() {
                return Some((*language, defaults));
            }
            // No safe default. For Python this is an env-only repo (every `.py` under a dependency
            // tree — `candidate_dirs` deliberately refuses to promote `.` over it, #173): OMIT the
            // binding rather than fall back to `["."]`, which would index the installed deps (#181
            // — the empty-default state must survive into the non-interactive plan, not
            // get reconverted to `.` here). For other languages, `.` is the reasonable
            // "index everything" default.
            if *language == Language::Python && !candidates.is_empty() {
                return None;
            }
            Some((*language, vec![PathBuf::from(".")]))
        })
        .collect();
    // Keep `languages` consistent with the bindings actually emitted (a dropped env-only Python
    // must not linger in the language list).
    let languages =
        languages.into_iter().filter(|language| bindings.contains_key(language)).collect();
    let backend = recommend_backend(estimated_chunks(scan.total_source_bytes));
    // Non-interactive default mirrors `OracleConfig`'s default: off until explicitly enabled.
    InitPlan { root_value, languages, bindings, backend, oracle_auto_run: false }
}
pub(crate) fn setup_index(config: &Config) -> anyhow::Result<IndexDatabase> {
    eprintln!("init: migrating SQLite schema");
    // SCHEMA-ONLY: init's subject repo is NOT registered yet (this is its very first index), so on
    // a global DB that already holds another repo the healing `migrate`'s open-time healers would
    // attribute an owed heal to that SIBLING via the witness's sole-repo arm — the same
    // unregistered-subject rule consolidate follows (see `scoped_repo_witness`'s limit). The
    // config-bearing index open below registers the new repo and runs the heals correctly scoped.
    let migration = IndexDatabase::migrate_schema_only(&config.database)?;
    if migration.state != rag_rat_core::index::schema::SchemaState::Compatible {
        anyhow::bail!("{}", migration.message);
    }
    eprintln!("init: indexing discovered files");
    IndexDatabase::index_discover_with_progress(config, render_index_progress)
}
pub(crate) fn setup_model_and_reconcile(
    config: &Config,
    db: &IndexDatabase,
    assume_yes: bool,
) -> anyhow::Result<()> {
    let backend = config.llm.embedding.backend;
    let Some(model_id) = backend.model_id() else {
        eprintln!(
            "init: embeddings disabled (model = \"none\") — structural + BM25 search only, no \
             vector backfill"
        );
        return Ok(());
    };
    let install = assume_yes
        || Confirm::new()
            .with_prompt(format!(
                "Install the {} embedding model and reconcile vectors now?",
                backend.as_str()
            ))
            .default(true)
            .interact()?;
    if !install {
        eprintln!("init: skipped model install and reconcile");
        return Ok(());
    }
    eprintln!("init: installing model {model_id}");
    // A `[remote]` block (endpoint from the toml, validated at config-parse) installs the model
    // over Ollama; absent → local install.
    let remote = config.llm.embedding.remote.as_ref();
    match db.install_model(model_id, remote) {
        Ok(model) => eprintln!("init: model status {} {}", model.model_id, model.status),
        // Soft fallback to hash ONLY for a LOCAL install whose feature wasn't compiled in
        // (fastembed/model2vec missing). A REMOTE install failure (endpoint down, auth wrong,
        // cookbook failed, dim mismatch) must PROPAGATE — the user asked for remote GPU embedding,
        // so silently degrading to hash with only an eprintln would be wrong (R4). Gate on
        // `remote.is_none()`.
        Err(err)
            if remote.is_none()
                && (model_id == FASTEMBED_MODEL_ID || model_id == MODEL2VEC_MODEL_ID) =>
        {
            eprintln!("init: {} install failed: {err}", backend.as_str());
            eprintln!("init: falling back to {HASH_MODEL_ID}");
            // Hash is a local backend — no remote config needed for the fallback.
            db.install_model(HASH_MODEL_ID, None)?;
        },
        Err(err) => return Err(err),
    }
    eprintln!("init: reconciling embeddings");
    db.reconcile_with_options_progress(
        ReconcileOptions {
            limit: None,
            batch_size: Some(config.llm.embedding.runtime.batch_size),
            force: false,
            until_clean: true,
            changed_first: true,
            max_seconds: None,
            max_embedding_chars: config.llm.embedding.runtime.max_embedding_chars,
            intra_threads: config.llm.embedding.runtime.ort_threads.map(|n| n as usize),
            // `init`'s reconcile is the deliberate bulk pass — provision an ephemeral box if
            // active.
            provision_remote: true,
        },
        render_reconcile_progress,
    )?;
    Ok(())
}
/// `init` does not register the MCP server with the user's agent — that is one short command the
/// user runs themselves (`init` shelling out to `claude`/`codex` was fragile and hid the command).
/// Print the project-scoped one-liner instead, so the connection step stays discoverable. See the
/// README "Connect it to your agent (MCP)" section.
fn print_mcp_connect_hint() {
    eprintln!("Next — connect your coding agent to rag-rat (run from this repo):");
    eprintln!("    claude mcp add --scope project rag-rat -- rag-rat mcp   # Claude Code");
    eprintln!("    codex  mcp add rag-rat -- rag-rat mcp                   # Codex");
}
pub(crate) fn offer_hooks_install(config: &Config, assume_yes: bool) -> anyhow::Result<()> {
    let install = assume_yes
        || Confirm::new()
            .with_prompt("Install rag-rat git maintenance hooks?")
            .default(false)
            .interact()?;
    if !install {
        return Ok(());
    }
    let git = git_paths(&config.root)?;
    fs::create_dir_all(&git.hooks_dir)?;
    for hook in crate::MANAGED_HOOKS {
        crate::install_hook(&git.hooks_dir, hook)?;
    }
    eprintln!("init: installed hooks in {}", git.hooks_dir.display());
    Ok(())
}
#[cfg(test)]
mod default_plan_tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn wizard_git_hook_install_skips_non_git_roots() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        let config_path = root.path().join("rag-rat.toml");
        std::fs::write(
            &config_path,
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();

        apply_git_hooks(&config, &std::collections::HashMap::new()).unwrap();

        assert!(!root.path().join(".git/hooks").exists());
    }

    #[test]
    fn interactive_reconfigure_scans_configured_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        let config_path = root.path().join("rag-rat.toml");
        std::fs::write(
            &config_path,
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        let options = InitOptions {
            yes: false,
            dry_run: false,
            force: false,
            config_path: config_path.clone(),
        };
        let existing = load_existing_for_wizard(&options).unwrap();

        assert_eq!(
            interactive_scan_root(Some(&existing)).unwrap(),
            root.path().canonicalize().unwrap()
        );
        assert!(existing.config.is_some());
    }

    #[test]
    fn local_config_root_resolves_relative_to_config_dir() {
        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("linked");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("rag-rat.toml");
        let raw = "[index]\nroot = \".\"\n";

        assert_eq!(
            local_config_root_from_raw(raw, &config_path).unwrap(),
            Some(config_dir.canonicalize().unwrap())
        );
    }

    #[test]
    fn invalid_existing_config_falls_back_to_fresh_draft() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        let config_path = root.path().join("rag-rat.toml");
        std::fs::write(
            &config_path,
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n\
             [llm.embedding]\nmodel = \"none\"\n[llm.embedding.remote]\nmodel = \
             \"all-minilm\"\nendpoint = \"http://localhost:11434\"\n",
        )
        .unwrap();
        let options = InitOptions {
            yes: false,
            dry_run: false,
            force: false,
            config_path: config_path.clone(),
        };

        let existing = load_existing_for_wizard(&options).unwrap();

        assert!(existing.config.is_none());
        assert_eq!(existing.local_root, Some(root.path().canonicalize().unwrap()));
    }

    /// #181: a repo whose only `.py` files live under a dependency tree must NOT get
    /// `python = ["."]` — `default_plan` carries the no-safe-default state through by omitting the
    /// Python binding entirely (rather than reconverting an empty default list to `.`).
    #[test]
    fn env_only_python_writes_no_binding_not_dot() {
        let root = Path::new("/repo");
        let mut scan = RepoScan::default();
        for name in ["a.py", "b.py"] {
            *scan.language_counts.entry(Language::Python).or_default() += 1;
            add_file_to_dir_counts(
                root,
                &root.join("env/lib/site-packages/pkg").join(name),
                Language::Python,
                &mut scan,
            )
            .unwrap();
        }
        let plan = default_plan(".".to_string(), &scan);
        assert!(
            !plan.bindings.contains_key(&Language::Python),
            "env-only Python must get NO binding, not `.`: {:?}",
            plan.bindings
        );
        assert!(!plan.languages.contains(&Language::Python));
    }

    /// #173: a root entrypoint (`manage.py`) directly under the root binds `.` — preserved.
    #[test]
    fn root_entrypoint_binds_dot() {
        let root = Path::new("/repo");
        let mut scan = RepoScan::default();
        *scan.language_counts.entry(Language::Python).or_default() += 1;
        add_file_to_dir_counts(root, &root.join("manage.py"), Language::Python, &mut scan).unwrap();
        let plan = default_plan(".".to_string(), &scan);
        assert_eq!(plan.bindings.get(&Language::Python), Some(&vec![PathBuf::from(".")]));
    }

    /// #181 review: a root entrypoint ALONGSIDE a root UNFLOORED venv (`env`/`.env`/`virtualenv`) must
    /// NOT bind `.` — the walk would ingest the venv (the indexer floor can't cover those names).
    /// With only the root entrypoint and no package dir, Python is omitted rather than indexing
    /// the venv. (A FLOORED venv like `.venv` does NOT set this flag, so it keeps binding `.` —
    /// see `root_entrypoint_binds_dot` and
    /// `only_unfloored_dependency_dirs_block_the_dot_default`.)
    #[test]
    fn root_entrypoint_with_root_venv_does_not_bind_dot() {
        let root = Path::new("/repo");
        let mut scan = RepoScan::default();
        *scan.language_counts.entry(Language::Python).or_default() += 1;
        add_file_to_dir_counts(root, &root.join("manage.py"), Language::Python, &mut scan).unwrap();
        // An unfloored venv (`virtualenv`/`env`/`.env`) sits at the root — what `scan_dir` records.
        scan.has_python_virtualenv = true;
        let plan = default_plan(".".to_string(), &scan);
        assert!(
            !plan.bindings.contains_key(&Language::Python),
            "a root venv must suppress the `.` default: {:?}",
            plan.bindings
        );
    }

    /// A normal Python package dir still binds (the env-only omission must not over-reach).
    #[test]
    fn python_package_dir_still_binds() {
        let root = Path::new("/repo");
        let mut scan = RepoScan::default();
        for name in ["__init__.py", "views.py"] {
            *scan.language_counts.entry(Language::Python).or_default() += 1;
            add_file_to_dir_counts(
                root,
                &root.join("myapp").join(name),
                Language::Python,
                &mut scan,
            )
            .unwrap();
        }
        let plan = default_plan(".".to_string(), &scan);
        assert_eq!(plan.bindings.get(&Language::Python), Some(&vec![PathBuf::from("myapp")]));
    }

    #[test]
    fn init_propagates_a_remote_install_failure_instead_of_falling_back_to_hash() {
        // R4: with a `[remote]` block, a remote-install failure (here: a closed-port connect
        // endpoint) must PROPAGATE — the user asked for remote GPU embedding, so init must NOT
        // silently degrade to hash. (Contrast a LOCAL feature-missing install, which still falls
        // back.) `assume_yes=true` skips the prompt.
        let n = std::sync::atomic::AtomicU64::new(0);
        let id = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("ragrat-r4-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
        // A closed port: bind then drop so the connect is refused at probe time.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        std::fs::write(
            root.join("rag-rat.toml"),
            format!(
                "[index]\nroot = \".\"\n\n[target_bindings]\nrust = [\"src\"]\n\n\
                 [llm.embedding]\nmodel = \"sentence-transformers/all-MiniLM-L6-v2\"\n\n\
                 [llm.embedding.remote]\nendpoint = \"http://127.0.0.1:{port}\"\nmodel = \
                 \"all-minilm\"\n"
            ),
        )
        .unwrap();

        let config = Config::load(root.join("rag-rat.toml")).unwrap();
        let db = IndexDatabase::rebuild(&config).unwrap();
        let err = setup_model_and_reconcile(&config, &db, true)
            .expect_err("a remote-install failure must propagate, not fall back to hash");
        // The Ollama probe against the dead endpoint failed and that error surfaced.
        assert!(err.to_string().to_lowercase().contains("ollama"), "{err}");
        // And the active model was NOT silently switched to hash.
        let active = db
            .list_models()
            .unwrap()
            .into_iter()
            .find(|m| m.model_id == rag_rat_core::embedding_models::HASH_MODEL_ID)
            .unwrap();
        assert!(!active.installed, "hash must NOT have been installed as a silent fallback");

        let _ = std::fs::remove_dir_all(&root);
    }
}
