use super::*;

pub(crate) fn run(args: &crate::cli::InitArgs, config_path: &str) -> anyhow::Result<()> {
    let options = InitOptions::from_args(args, config_path);
    let _terminal_reset = TerminalResetGuard::install_if_interactive(!options.yes)?;
    let root = env::current_dir()?.canonicalize()?;
    let scan = scan_repo(&root)?;
    let root_value = config_root_value(&root, &options.config_path);
    let plan = if options.yes {
        default_plan(root_value, &scan)
    } else {
        prompt_plan(root, root_value, &scan)?
    };
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
    apply_embedding_runtime_env(&config.local_ai.embedding.runtime);
    let db = setup_index(&config)?;
    setup_model_and_reconcile(&config, &db, options.yes)?;
    offer_mcp_install(&config, &options.config_path, options.yes)?;
    offer_hooks_install(&config, options.yes)?;
    eprintln!("init: complete");
    Ok(())
}
pub(crate) fn prompt_plan(
    root: PathBuf,
    root_value: String,
    scan: &RepoScan,
) -> anyhow::Result<InitPlan> {
    println!("Repository root: {}", root.display());
    println!();
    println!("Detected languages:");
    print_language_summary(scan);
    println!();

    let language_items = supported_languages()
        .iter()
        .map(|language| {
            let count = scan.language_counts.get(language).copied().unwrap_or_default();
            format!("{} ({count} files)", language.as_str())
        })
        .collect::<Vec<_>>();
    let defaults = supported_languages()
        .iter()
        .map(|language| scan.language_counts.get(language).copied().unwrap_or_default() > 0)
        .collect::<Vec<_>>();
    let selected = MultiSelect::new()
        .with_prompt("Select languages to index")
        .items(&language_items)
        .defaults(&defaults)
        .interact()?;
    let languages =
        selected.into_iter().map(|index| supported_languages()[index]).collect::<Vec<_>>();
    if languages.is_empty() {
        anyhow::bail!("init needs at least one selected language");
    }

    let mut bindings = BTreeMap::new();
    for language in &languages {
        let candidates = candidate_dirs(scan, *language);
        if candidates.is_empty() {
            bindings.insert(*language, vec![PathBuf::from(".")]);
            continue;
        }
        println!();
        println!("Candidate paths for {}:", language.as_str());
        let items = candidates
            .iter()
            .map(|candidate| {
                format!("{} ({} files)", display_rel(&candidate.path), candidate.count)
            })
            .collect::<Vec<_>>();
        let defaults = candidates.iter().map(|candidate| candidate.default).collect::<Vec<_>>();
        let selected = MultiSelect::new()
            .with_prompt(format!("Select {} roots", language.as_str()))
            .items(&items)
            .defaults(&defaults)
            .interact()?;
        let dirs: Vec<_> =
            selected.into_iter().map(|index| candidates[index].path.clone()).collect();
        if dirs.is_empty() {
            anyhow::bail!("init needs at least one selected root for {}", language.as_str());
        }
        bindings.insert(*language, dirs);
    }

    let backend = prompt_backend(scan)?;
    let oracle_auto_run = prompt_oracle_auto_run()?;
    Ok(InitPlan { root_value, languages, bindings, backend, oracle_auto_run })
}

/// Offer the opt-in background oracle. Default NO: it needs a language tool (e.g. rust-analyzer) on
/// PATH and spends CPU on a throttled SCIP pass, so it should be a deliberate choice. Writing
/// `auto_run = false` either way keeps the knob visible in the generated config.
pub(crate) fn prompt_oracle_auto_run() -> anyhow::Result<bool> {
    println!();
    println!(
        "Compiler-grade ranking: a background pass can refresh SCIP-based importance ranking as \
         the"
    );
    println!(
        "repo changes (needs a language tool such as rust-analyzer on PATH; runs only in the MCP"
    );
    println!("server, heavily throttled). You can flip `[oracle] auto_run` later.");
    Ok(Confirm::new()
        .with_prompt("Enable background compiler-grade ranking refresh?")
        .default(false)
        .interact()?)
}
pub(crate) fn prompt_backend(scan: &RepoScan) -> anyhow::Result<EmbeddingBackend> {
    let estimate = estimated_chunks(scan.total_source_bytes);
    let recommended = recommend_backend(estimate);
    println!();
    println!(
        "Embedding backend (≈{estimate} chunks from {} of source):",
        human_bytes(scan.total_source_bytes)
    );
    println!("  recommended: {}", backend_label(recommended));
    let choices =
        [EmbeddingBackend::FastEmbed, EmbeddingBackend::Model2Vec, EmbeddingBackend::None];
    let default_index = choices.iter().position(|backend| *backend == recommended).unwrap_or(0);
    let items = choices.iter().map(|backend| backend_label(*backend)).collect::<Vec<_>>();
    let selected = Select::new()
        .with_prompt("Select embedding backend")
        .items(&items)
        .default(default_index)
        .interact()?;
    Ok(choices[selected])
}
pub(crate) fn human_bytes(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}
pub(crate) fn default_plan(root_value: String, scan: &RepoScan) -> InitPlan {
    let languages = supported_languages()
        .into_iter()
        .filter(|language| scan.language_counts.get(language).copied().unwrap_or_default() > 0)
        .collect::<Vec<_>>();
    let languages = if languages.is_empty() { vec![Language::Rust] } else { languages };
    let bindings = languages
        .iter()
        .map(|language| {
            let dirs = candidate_dirs(scan, *language)
                .into_iter()
                .filter(|candidate| candidate.default)
                .map(|candidate| candidate.path)
                .collect::<Vec<_>>();
            let dirs = if dirs.is_empty() { vec![PathBuf::from(".")] } else { dirs };
            (*language, dirs)
        })
        .collect();
    let backend = recommend_backend(estimated_chunks(scan.total_source_bytes));
    // Non-interactive default mirrors `OracleConfig`'s default: off until explicitly enabled.
    InitPlan { root_value, languages, bindings, backend, oracle_auto_run: false }
}
pub(crate) fn setup_index(config: &Config) -> anyhow::Result<IndexDatabase> {
    eprintln!("init: migrating SQLite schema");
    let migration = IndexDatabase::migrate(&config.database)?;
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
    let backend = config.local_ai.embedding.backend;
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
    match db.install_model(model_id) {
        Ok(model) => eprintln!("init: model status {} {}", model.model_id, model.status),
        Err(err) if model_id == FASTEMBED_MODEL_ID || model_id == MODEL2VEC_MODEL_ID => {
            eprintln!("init: {} install failed: {err}", backend.as_str());
            eprintln!("init: falling back to {HASH_MODEL_ID}");
            db.install_model(HASH_MODEL_ID)?;
        },
        Err(err) => return Err(err),
    }
    eprintln!("init: reconciling embeddings");
    db.reconcile_with_options_progress(
        ReconcileOptions {
            limit: None,
            batch_size: Some(config.local_ai.embedding.runtime.batch_size),
            force: false,
            until_clean: true,
            changed_first: true,
            max_seconds: None,
            max_embedding_chars: config.local_ai.embedding.runtime.max_embedding_chars,
            intra_threads: config.local_ai.embedding.runtime.ort_threads.map(|n| n as usize),
        },
        render_reconcile_progress,
    )?;
    Ok(())
}
pub(crate) fn offer_mcp_install(
    config: &Config,
    config_path: &Path,
    assume_yes: bool,
) -> anyhow::Result<()> {
    let absolute_config = absolute_config_path(config, config_path)?;
    if assume_yes
        || Confirm::new()
            .with_prompt("Install rag-rat MCP for Claude Code?")
            .default(false)
            .interact()?
    {
        install_claude_mcp(&absolute_config)?;
    }
    if assume_yes
        || Confirm::new().with_prompt("Install rag-rat MCP for Codex?").default(false).interact()?
    {
        install_codex_mcp(&absolute_config)?;
    }
    Ok(())
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
pub(crate) fn install_claude_mcp(config_path: &Path) -> anyhow::Result<()> {
    let exe = current_exe_for_mcp()?;
    let status = Command::new("claude")
        .arg("mcp")
        .arg("add")
        .arg("--scope")
        .arg("project")
        .arg("rag-rat")
        .arg("--")
        .arg(&exe)
        .arg("mcp")
        .arg("--config")
        .arg(config_path)
        .status();
    match status {
        Ok(status) if status.success() => eprintln!("init: installed Claude Code MCP server"),
        Ok(status) => eprintln!("init: claude mcp add exited with status {status}"),
        Err(err) => eprintln!("init: could not run claude mcp add: {err}"),
    }
    Ok(())
}
pub(crate) fn install_codex_mcp(config_path: &Path) -> anyhow::Result<()> {
    let exe = current_exe_for_mcp()?;
    let status = Command::new("codex")
        .arg("mcp")
        .arg("add")
        .arg("rag-rat")
        .arg("--")
        .arg(&exe)
        .arg("mcp")
        .arg("--config")
        .arg(config_path)
        .status();
    match status {
        Ok(status) if status.success() => eprintln!("init: installed Codex MCP server"),
        Ok(status) => {
            eprintln!("init: codex mcp add exited with status {status}");
            print_codex_config_snippet(&exe, config_path);
        },
        Err(err) => {
            eprintln!("init: could not run codex mcp add: {err}");
            print_codex_config_snippet(&exe, config_path);
        },
    }
    Ok(())
}
pub(crate) fn current_exe_for_mcp() -> anyhow::Result<PathBuf> {
    env::current_exe().map_err(Into::into)
}
pub(crate) fn print_codex_config_snippet(exe: &Path, config_path: &Path) {
    eprintln!(
        "Add this to ~/.codex/config.toml if your Codex build does not support `codex mcp add`:"
    );
    eprintln!("[mcp_servers.rag-rat]");
    eprintln!("command = {:?}", exe.display().to_string());
    eprintln!("args = [\"mcp\", \"--config\", {:?}]", config_path.display().to_string());
}
pub(crate) fn absolute_config_path(config: &Config, config_path: &Path) -> anyhow::Result<PathBuf> {
    if config_path.is_absolute() {
        Ok(config_path.to_path_buf())
    } else {
        Ok(config.root.join(config_path).canonicalize()?)
    }
}
