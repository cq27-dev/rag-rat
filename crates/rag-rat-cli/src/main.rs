use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use rag_rat_core::{
    Config, IndexDatabase,
    config::EmbeddingRuntimeConfig,
    index::{IndexProgress, github::GitHubSyncAction},
    search::lexical::SearchHit,
};

mod claude_hook;
mod claude_settings;
mod init;

fn main() -> anyhow::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        usage();
        return Ok(());
    };
    if command == "init" {
        return init::run(&args);
    }
    if command == "claude-hook" {
        return claude_hook::run();
    }
    let config_path = option_value(&args, "--config").unwrap_or_else(|| "rag-rat.toml".to_string());
    let config = Config::load(&config_path)?;
    apply_embedding_runtime_env(&config.local_ai.embedding.runtime);

    match command {
        "index" => {
            if has_flag(&args, "--watch") {
                run_watch(config)?;
            } else {
                // Serialize with the background watcher / other writers (busy_timeout backstops
                // any heal on the query path).
                let _lock = rag_rat_core::locks::FileLock::acquire_blocking(
                    &rag_rat_core::locks::write_lock_path(&config.database),
                )?;
                let db = if has_flag(&args, "--full") {
                    IndexDatabase::rebuild_with_progress(&config, render_index_progress)?
                } else if has_flag(&args, "--discover") {
                    IndexDatabase::index_discover_with_progress(&config, render_index_progress)?
                } else {
                    IndexDatabase::index_changed_with_progress(&config, render_index_progress)?
                };
                // Re-anchor repo memories against the freshly indexed symbols/chunks so a moved or
                // renamed binding relocates (or is flagged) instead of silently pointing at a stale
                // row. Memory rows themselves are never deleted by indexing.
                if let Err(err) = db.memory_validate() {
                    eprintln!("warning: repo-memory re-validation failed: {err}");
                }
                print_json(&db.status(&config.database)?)?;
            }
        },
        "doctor" => {
            doctor(&config)?;
        },
        "migrate" => {
            migrate(&config, &args)?;
        },
        "query" => {
            let query = positional_after_options(&args).unwrap_or_default();
            if query.is_empty() {
                anyhow::bail!("query command needs a search string");
            }
            let db = IndexDatabase::open_config(&config)?;
            if has_flag(&args, "--explain") {
                print_query_explain(&db.search_explain(&query, 10, false)?);
            } else {
                print_json(&db.search(&query, 10, false)?)?;
            }
        },
        "brief" => {
            let db = IndexDatabase::open_config(&config)?;
            let mode = rag_rat_core::query::repo_brief::RepoBriefMode::parse(
                option_value(&args, "--mode").as_deref(),
            )?;
            let limit = option_value(&args, "--limit")
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(10);
            print_json(&db.repo_brief(rag_rat_core::query::repo_brief::RepoBriefOptions {
                mode,
                limit,
                include_generated: has_flag(&args, "--include-generated"),
                include_memories: !has_flag(&args, "--no-memories"),
            })?)?;
        },
        "clusters" => {
            let db = IndexDatabase::open_config(&config)?;
            let limit = option_value(&args, "--limit")
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(10);
            let min_cluster_size = option_value(&args, "--min-cluster-size")
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(2);
            print_json(&db.repo_clusters(rag_rat_core::query::clusters::RepoClustersOptions {
                limit,
                include_generated: has_flag(&args, "--include-generated"),
                include_memories: !has_flag(&args, "--no-memories"),
                min_cluster_size,
            })?)?;
        },
        "mcp" => {
            tokio::runtime::Runtime::new()?.block_on(rag_rat_mcp::server::run_stdio(config))?;
        },
        "github" => {
            github(&config, &args)?;
        },
        "hooks" => {
            hooks(&config, &args)?;
        },
        "maintenance" => {
            maintenance(&config, &args)?;
        },
        "models" => {
            models(&config, &args)?;
        },
        "reconcile" => {
            reconcile(&config, &args)?;
        },
        "gc" => {
            let db = IndexDatabase::open_config(&config)?;
            print_json(&db.gc()?)?;
        },
        "eval" => {
            eval(&config, &args)?;
        },
        "dump-config" => {
            let targets = config
                .targets
                .iter()
                .map(|target| {
                    serde_json::json!({
                        "name": target.name,
                        "language": target.language.as_str(),
                        "directories": target.directories,
                        "include": target.include,
                        "exclude": target.exclude,
                        "kind": target.kind.as_str(),
                    })
                })
                .collect::<Vec<_>>();
            print_json(&serde_json::json!({
                "root": config.root,
                "database": config.database,
                "local_ai": {
                    "embedding": {
                        "runtime": {
                            "batch_size": config.local_ai.embedding.runtime.batch_size,
                            "ort_threads": config.local_ai.embedding.runtime.ort_threads,
                            "omp_threads": config.local_ai.embedding.runtime.omp_threads,
                            "max_embedding_chars": config.local_ai.embedding.runtime.max_embedding_chars,
                        }
                    }
                },
                "targets": targets,
            }))?;
        },
        _ => {
            usage();
            anyhow::bail!("unknown command `{command}`");
        },
    }

    Ok(())
}

fn eval(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let options = rag_rat_core::eval::EvalOptions {
        queries_path: option_value(args, "--queries")
            .map(Into::into)
            .unwrap_or_else(|| default_eval_path(config, "queries.toml")),
        expected_path: option_value(args, "--expected")
            .map(Into::into)
            .unwrap_or_else(|| default_eval_path(config, "expected_hits.toml")),
        update_baseline: has_flag(args, "--update-baseline"),
    };
    let report = rag_rat_core::eval::run(config, &options)?;
    if has_flag(args, "--json") || options.update_baseline {
        print_json(&report)?;
    } else {
        print_eval_summary(&report);
    }
    if !report.pass {
        anyhow::bail!(
            "eval failed: stale_current_source_violations={}, failed_queries={}",
            report.metrics.stale_current_source_violations,
            report.results.iter().filter(|result| !result.passed).count()
        );
    }
    Ok(())
}

fn default_eval_path(config: &Config, file_name: &str) -> PathBuf {
    config.root.join("evals").join(file_name)
}

fn print_eval_summary(report: &rag_rat_core::eval::EvalReport) {
    println!(
        "eval: pass={} queries={} skipped={} mrr@10={:.3} recall@10={:.3} path_hit_rate={:.3} symbol_hit_rate={:.3}",
        report.pass,
        report.queries,
        report.results.iter().filter(|result| result.skipped).count(),
        report.metrics.mrr_at_10,
        report.metrics.recall_at_10,
        report.metrics.path_hit_rate,
        report.metrics.symbol_hit_rate
    );
    println!(
        "eval: stale_current_source_violations={} stale_hit_rate={:.3} latency_p50_ms={:.1} latency_p95_ms={:.1}",
        report.metrics.stale_current_source_violations,
        report.metrics.stale_hit_rate,
        report.metrics.latency_p50_ms,
        report.metrics.latency_p95_ms
    );
    println!(
        "eval: graph_evidence_hit_rate={:.3} impact_hit_rate={:.3} git_evidence_hit_rate={:.3} papertrail_evidence_hit_rate={:.3}",
        report.metrics.graph_evidence_hit_rate,
        report.metrics.impact_hit_rate,
        report.metrics.git_evidence_hit_rate,
        report.metrics.papertrail_evidence_hit_rate
    );
    if let Some(precision) = report.metrics.papertrail_precision_sample {
        println!("eval: papertrail_precision_sample={precision:.3}");
    }
    println!(
        "eval: hash_vector_baseline model={} available={} current_artifacts={} mrr@10={:.3} recall@10={:.3} delta_mrr@10={:+.3} delta_recall@10={:+.3}",
        report.hash_vector_baseline.model_id,
        report.hash_vector_baseline.available,
        report.hash_vector_baseline.current_artifacts,
        report.hash_vector_baseline.metrics.mrr_at_10,
        report.hash_vector_baseline.metrics.recall_at_10,
        report.hash_vector_baseline.delta_mrr_at_10,
        report.hash_vector_baseline.delta_recall_at_10
    );
    for result in report.results.iter().filter(|result| !result.passed) {
        println!(
            "eval: failed {} missing_paths={:?} missing_symbols={:?} missing_graph_targets={:?} missing_impact_categories={:?} missing_impact_paths={:?} missing_impact_symbols={:?} missing_git_subjects={:?} missing_papertrail_kinds={:?} stale_current_source_violations={}",
            result.id,
            result.missing_paths,
            result.missing_symbols,
            result.missing_graph_targets,
            result.missing_impact_categories,
            result.missing_impact_paths,
            result.missing_impact_symbols,
            result.missing_git_subjects,
            result.missing_papertrail_kinds,
            result.stale_current_source_violations
        );
    }
    for result in report.results.iter().filter(|result| result.skipped) {
        println!(
            "eval: skipped {} reason={}",
            result.id,
            result.skip_reason.as_deref().unwrap_or("not applicable")
        );
    }
}

fn print_query_explain(hits: &[SearchHit]) {
    for (index, hit) in hits.iter().enumerate() {
        if index > 0 {
            println!();
        }
        println!(
            "{}:{}-{} {}",
            hit.path,
            hit.start_line,
            hit.end_line,
            hit.symbol_path.as_deref().unwrap_or("<chunk>")
        );
        println!("score: {:.3}", hit.score);
        if let Some(components) = &hit.score_components {
            println!("  bm25: {:.3}", components.bm25);
            println!("  vector: {:.3}", components.vector);
            println!("  symbol: {:.3}", components.symbol);
            println!("  graph: {:.3}", components.graph);
            println!("  git: {:.3}", components.git);
            println!("  github: {:.3}", components.github);
            if let Some(note) = &components.vector_note {
                println!("  vector_note: {note}");
            }
        }
        println!("summary:");
        for line in hit.summary.lines() {
            println!("  {line}");
        }
    }
}

fn models(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let db = IndexDatabase::open_config(config)?;
    match args.get(1).map(String::as_str) {
        Some("list") | None => print_json(&db.list_models()?),
        Some("install") => {
            let Some(model_id) = args.get(2) else {
                anyhow::bail!("models install needs a model id");
            };
            print_json(&db.install_model(model_id)?)
        },
        Some(other) => anyhow::bail!("unknown models subcommand `{other}`"),
    }
}

fn reconcile(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let db = IndexDatabase::open_config(config)?;
    if has_flag(args, "--plan") {
        let plan = db.reconcile_plan()?;
        if has_flag(args, "--json") {
            print_json(&plan)
        } else {
            print_reconcile_plan(&plan);
            Ok(())
        }?;
        return Ok(());
    }
    let limit = option_value(args, "--limit").map(|value| value.parse()).transpose()?;
    let batch_size = option_value(args, "--batch-size")
        .map(|value| value.parse())
        .transpose()?
        .or(Some(config.local_ai.embedding.runtime.batch_size));
    let force = has_flag(args, "--force");
    let max_seconds = option_value(args, "--max-seconds").map(|value| value.parse()).transpose()?;
    let max_embedding_chars = option_value(args, "--max-embedding-chars")
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(config.local_ai.embedding.runtime.max_embedding_chars);
    let options = rag_rat_core::index::ai::ReconcileOptions {
        limit,
        batch_size,
        force,
        until_clean: has_flag(args, "--until-clean"),
        changed_first: has_flag(args, "--changed-first"),
        max_seconds,
        max_embedding_chars,
        intra_threads: config.local_ai.embedding.runtime.ort_threads.map(|n| n as usize),
    };
    print_json(&db.reconcile_with_options_progress(options, render_reconcile_progress)?)
}

fn run_watch(config: Config) -> anyhow::Result<()> {
    let Some(_watcher) = rag_rat_core::watch::Watcher::spawn(config.clone()) else {
        anyhow::bail!("watcher is disabled ([watch] enabled = false or RAG_RAT_NO_WATCH set)");
    };
    eprintln!("rag-rat: watching {} for changes (Ctrl-C to stop)", config.root.display());
    // The watcher runs on its own thread; park here. Ctrl-C ends the process and the OS releases
    // the locks; the next session's startup catch-up covers any edit in flight.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

pub(crate) fn apply_embedding_runtime_env(runtime: &EmbeddingRuntimeConfig) {
    // `ort_threads` is applied via fastembed's session `with_intra_threads` (see
    // FastEmbedEmbedder::new), not an env var — ONNX Runtime does not read `ORT_NUM_THREADS`.
    // `omp_threads` IS effective: Microsoft's prebuilt ORT is OpenMP-based and honors
    // `OMP_NUM_THREADS`, so it is the real thread lever for the default binaries.
    set_env_if_absent("OMP_NUM_THREADS", runtime.omp_threads);
}

fn set_env_if_absent(key: &str, value: Option<u32>) {
    let Some(value) = value else {
        return;
    };
    if env::var_os(key).is_some() {
        return;
    }
    // This is called at process startup before rag-rat creates its Tokio runtime or initializes
    // FastEmbed/ONNX. CLI-provided environment variables intentionally take precedence.
    unsafe {
        env::set_var(key, value.to_string());
    }
}

fn print_reconcile_plan(plan: &rag_rat_core::index::ai::ReconcilePlan) {
    let embeddings = &plan.embeddings;
    println!("Embeddings");
    println!("  model: {}", embeddings.model_id);
    println!("  model_version: {}", embeddings.model_version);
    println!("  dim: {}", embeddings.dim);
    println!("  available: {}", embeddings.available);
    if let Some(message) = &embeddings.message {
        println!("  message: {message}");
    }
    println!("  current: {}", embeddings.current);
    println!("  missing: {}", embeddings.missing);
    println!("  stale: {}", embeddings.stale);
    println!("  model_changed: {}", embeddings.model_changed);
    println!("  dim_changed: {}", embeddings.dim_changed);
    println!("  failed_retryable: {}", embeddings.failed_retryable);
    println!("  failed_waiting: {}", embeddings.failed_waiting);
    println!("  blocked: {}", embeddings.blocked);
    println!("  skipped: {}", embeddings.skipped_total);
    if !embeddings.skipped_by_policy.is_empty() {
        println!("  skipped_by_policy:");
        for (policy, count) in &embeddings.skipped_by_policy {
            println!("    {policy}: {count}");
        }
    }
    if !embeddings.missing_by_priority.is_empty() {
        println!("  missing_by_priority:");
        for (priority, count) in &embeddings.missing_by_priority {
            println!("    {priority}: {count}");
        }
    }
    println!();
    println!("Summaries");
    println!(
        "  {}",
        if plan.summaries.enabled { "enabled" } else { plan.summaries.message.as_str() }
    );
}

pub(crate) fn render_reconcile_progress(progress: rag_rat_core::index::ai::ReconcileProgress) {
    match progress {
        rag_rat_core::index::ai::ReconcileProgress::Started {
            model_id,
            total_chunks,
            batch_size,
        } => {
            eprintln!("reconcile: model={model_id} chunks={total_chunks} batch_size={batch_size}");
        },
        rag_rat_core::index::ai::ReconcileProgress::Batch {
            processed_chunks,
            total_chunks,
            embeddings_written,
            blocked_chunks,
        } => {
            let percent = progress_percent(processed_chunks, total_chunks);
            eprintln!(
                "reconcile: {processed_chunks}/{total_chunks} ({percent:>3}%) written={embeddings_written} blocked={blocked_chunks}"
            );
        },
        rag_rat_core::index::ai::ReconcileProgress::Finished {
            processed_chunks,
            embeddings_written,
            blocked_chunks,
        } => {
            eprintln!(
                "reconcile: complete processed={processed_chunks} written={embeddings_written} blocked={blocked_chunks}"
            );
        },
    }
}

fn progress_percent(current: u64, total: u64) -> u64 {
    current.saturating_mul(100).checked_div(total).unwrap_or(100).min(100)
}

fn migrate(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let status = if has_flag(args, "--check") {
        IndexDatabase::migration_check(&config.database)?
    } else {
        IndexDatabase::migrate(&config.database)?
    };
    print_json(&status)?;
    if has_flag(args, "--check")
        && status.state != rag_rat_core::index::schema::SchemaState::Compatible
    {
        anyhow::bail!("{}", status.message);
    }
    Ok(())
}

fn doctor(config: &Config) -> anyhow::Result<()> {
    let schema = IndexDatabase::migration_check(&config.database)?;
    let (index, discovery, storage) =
        if schema.state == rag_rat_core::index::schema::SchemaState::Compatible {
            let db = IndexDatabase::open_config(config)?;
            (
                Some(serde_json::to_value(db.status(&config.database)?)?),
                Some(serde_json::to_value(db.discovery_status(config)?)?),
                Some(serde_json::to_value(db.storage_status()?)?),
            )
        } else {
            (None, None, None)
        };
    print_json(&serde_json::json!({
        "config_root": config.root,
        "database": config.database,
        "schema": schema,
        "storage": storage,
        "discovery": discovery,
        "targets": config.targets.iter().map(|target| serde_json::json!({
            "name": target.name,
            "language": target.language.as_str(),
            "directories": target.directories,
            "kind": target.kind.as_str(),
        })).collect::<Vec<_>>(),
        "index": index,
        "mcp": {
            "transport": "stdio",
            "tools": rag_rat_mcp::tools::TOOL_NAMES,
            "source_read_only": true,
            "index_writes": "sqlite_auto_heal"
        }
    }))
}

fn github(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let Some(subcommand) = args.get(1).map(String::as_str) else {
        anyhow::bail!("github command needs a subcommand");
    };
    match subcommand {
        "sync" => {
            let db = IndexDatabase::open_config(config)?;
            let offline = has_flag(args, "--offline");
            let report = if let Some(issue) = option_value(args, "--issue") {
                db.github_sync_issue(&issue, offline)?
            } else if has_flag(args, "--from-refs") {
                db.github_sync_from_refs_with_progress(offline, render_github_sync_progress)?
            } else {
                anyhow::bail!("github sync needs --from-refs or --issue <owner/repo#number>");
            };
            print_json(&report)
        },
        other => anyhow::bail!("unknown github subcommand `{other}`"),
    }
}

fn render_github_sync_progress(progress: rag_rat_core::index::github::GitHubSyncProgress) {
    match progress.action {
        GitHubSyncAction::Syncing => eprintln!(
            "github sync: {}/{} fetching {}/{}#{}",
            progress.current, progress.total, progress.owner, progress.repo, progress.number
        ),
        GitHubSyncAction::Skipped => eprintln!(
            "github sync: {}/{} skip cached {}/{}#{}",
            progress.current, progress.total, progress.owner, progress.repo, progress.number
        ),
        GitHubSyncAction::Synced => eprintln!(
            "github sync: {}/{} synced {}/{}#{}",
            progress.current, progress.total, progress.owner, progress.repo, progress.number
        ),
        GitHubSyncAction::Failed => eprintln!(
            "github sync: {}/{} failed {}/{}#{}: {}",
            progress.current,
            progress.total,
            progress.owner,
            progress.repo,
            progress.number,
            progress.message.unwrap_or_else(|| "unknown error".to_string())
        ),
        GitHubSyncAction::RebuildingFts => {
            eprintln!("github sync: rebuilding GitHub FTS cache")
        },
    }
}

pub(crate) const MANAGED_HOOKS: &[&str] =
    &["post-checkout", "post-merge", "post-rewrite", "post-commit"];
const HOOK_MARKER: &str = "# Generated by rag-rat.";
const DEFAULT_MAINTENANCE_SECONDS: u64 = 30;

fn hooks(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let Some(subcommand) = args.get(1).map(String::as_str) else {
        anyhow::bail!("hooks command needs install, uninstall, or status");
    };
    if args.iter().any(|a| a == "--claude") {
        return claude_hooks(config, subcommand, args.iter().any(|a| a == "--global"));
    }
    let git = git_paths(&config.root)?;
    match subcommand {
        "install" => {
            fs::create_dir_all(&git.hooks_dir)?;
            let mut installed = Vec::new();
            for hook in MANAGED_HOOKS {
                install_hook(&git.hooks_dir, hook)?;
                installed.push(*hook);
            }
            print_json(&serde_json::json!({
                "status": "installed",
                "repo_root": git.worktree_root,
                "git_dir": git.git_dir,
                "git_common_dir": git.git_common_dir,
                "hooks_dir": git.hooks_dir,
                "hooks": installed,
            }))
        },
        "uninstall" => {
            let mut removed = Vec::new();
            let mut kept = Vec::new();
            for hook in MANAGED_HOOKS {
                let path = git.hooks_dir.join(hook);
                if !path.exists() {
                    continue;
                }
                if is_rag_rat_hook(&path)? {
                    fs::remove_file(&path)?;
                    removed.push(*hook);
                } else {
                    kept.push(*hook);
                }
            }
            print_json(&serde_json::json!({
                "status": "uninstalled",
                "hooks_dir": git.hooks_dir,
                "removed": removed,
                "kept_unmanaged": kept,
            }))
        },
        "status" => {
            let hooks = MANAGED_HOOKS
                .iter()
                .map(|hook| {
                    let path = git.hooks_dir.join(hook);
                    let managed = is_rag_rat_hook(&path).unwrap_or(false);
                    serde_json::json!({
                        "name": hook,
                        "path": path,
                        "exists": path.exists(),
                        "managed": managed,
                    })
                })
                .collect::<Vec<_>>();
            print_json(&serde_json::json!({
                "repo_root": git.worktree_root,
                "git_dir": git.git_dir,
                "git_common_dir": git.git_common_dir,
                "hooks_dir": git.hooks_dir,
                "hooks": hooks,
            }))
        },
        other => anyhow::bail!("unknown hooks subcommand `{other}`"),
    }
}

fn claude_hooks(config: &Config, subcommand: &str, global: bool) -> anyhow::Result<()> {
    let path = claude_settings::settings_path(&config.root, global)?;
    let mut settings = claude_settings::read_settings(&path)?;
    match subcommand {
        "install" => {
            let changed = claude_settings::merge_hook_entries(&mut settings);
            if changed {
                claude_settings::write_settings(&path, &settings)?;
            }
            print_json(&serde_json::json!({
                "status": if changed { "installed" } else { "already_installed" },
                "settings_path": path,
                "matchers": ["Grep", "Bash"],
            }))
        },
        "uninstall" => {
            let changed = claude_settings::remove_hook_entries(&mut settings);
            if changed {
                claude_settings::write_settings(&path, &settings)?;
            }
            print_json(&serde_json::json!({
                "status": if changed { "uninstalled" } else { "not_installed" },
                "settings_path": path,
            }))
        },
        "status" => {
            let (grep, bash) = claude_settings::hook_status(&settings);
            print_json(&serde_json::json!({
                "settings_path": path,
                "grep_matcher_installed": grep,
                "bash_matcher_installed": bash,
            }))
        },
        other => anyhow::bail!("unknown hooks subcommand `{other}`"),
    }
}

fn maintenance(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let trigger = option_value(args, "--trigger").unwrap_or_else(|| "manual".to_string());
    let max_seconds = option_value(args, "--max-seconds")
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(DEFAULT_MAINTENANCE_SECONDS);
    let branch_checkout = option_value(args, "--branch-checkout");
    let old_head = option_value(args, "--old-head");
    let new_head = option_value(args, "--new-head");
    let started = Instant::now();

    if trigger == "post-checkout" && branch_checkout.as_deref() == Some("0") {
        print_json(&serde_json::json!({
            "trigger": trigger,
            "status": "skipped",
            "reason": "file checkout",
            "old_head": old_head,
            "new_head": new_head,
            "branch_checkout": branch_checkout,
        }))?;
        return Ok(());
    }

    // Serialize with the background watcher (and other writers). The hook backgrounds this command,
    // so blocking here never holds up the git operation; busy_timeout backstops the query-path heal.
    let _lock = rag_rat_core::locks::FileLock::acquire_blocking(
        &rag_rat_core::locks::write_lock_path(&config.database),
    )?;

    let db = IndexDatabase::index_discover_with_progress(config, render_index_progress)?;
    let elapsed = started.elapsed().as_secs();
    let remaining_seconds = max_seconds.saturating_sub(elapsed);
    let reconcile_report = if remaining_seconds > 0 {
        let options = rag_rat_core::index::ai::ReconcileOptions {
            limit: None,
            batch_size: Some(config.local_ai.embedding.runtime.batch_size),
            force: false,
            until_clean: false,
            changed_first: true,
            max_seconds: Some(remaining_seconds),
            max_embedding_chars: config.local_ai.embedding.runtime.max_embedding_chars,
            intra_threads: config.local_ai.embedding.runtime.ort_threads.map(|n| n as usize),
        };
        Some(db.reconcile_with_options_progress(options, render_reconcile_progress)?)
    } else {
        None
    };
    // Prune index rows for git contexts that are no longer live (worktree-safe; keeps every
    // live worktree's HEAD). Cheap and bounded, so it runs every maintenance pass.
    let gc_report = db.gc().ok();
    // Re-anchor repo memories: post-checkout/merge/rewrite/commit are exactly when files move,
    // rename, or change, so relocate symbol/chunk bindings (or flag them) here rather than
    // leaving stale anchors until a manual memory_validate.
    let memory_validation = db.memory_validate().ok();
    let plan = db.reconcile_plan()?;
    print_json(&serde_json::json!({
        "trigger": trigger,
        "status": "complete",
        "old_head": old_head,
        "new_head": new_head,
        "branch_checkout": branch_checkout,
        "max_seconds": max_seconds,
        "elapsed_seconds": started.elapsed().as_secs_f64(),
        "reconcile": reconcile_report,
        "gc": gc_report,
        "memory_validation": memory_validation,
        "remaining_backlog": {
            "model": plan.embeddings.model_id,
            "current": plan.embeddings.current,
            "missing": plan.embeddings.missing,
            "stale": plan.embeddings.stale,
            "failed_retryable": plan.embeddings.failed_retryable,
            "failed_waiting": plan.embeddings.failed_waiting,
            "blocked": plan.embeddings.blocked,
            "skipped": plan.embeddings.skipped_total,
            "missing_by_priority": plan.embeddings.missing_by_priority,
            "skipped_by_policy": plan.embeddings.skipped_by_policy,
        }
    }))
}

#[derive(Debug)]
pub(crate) struct GitPaths {
    worktree_root: PathBuf,
    git_dir: PathBuf,
    git_common_dir: PathBuf,
    pub(crate) hooks_dir: PathBuf,
}

pub(crate) fn git_paths(root: &Path) -> anyhow::Result<GitPaths> {
    let worktree_root = git_rev_parse(root, "--show-toplevel")?;
    let git_dir = git_rev_parse(root, "--git-dir")?;
    let git_common_dir = git_rev_parse(root, "--git-common-dir")?;
    let hooks_dir = git_rev_parse(root, "--git-path hooks")?;
    Ok(GitPaths { worktree_root, git_dir, git_common_dir, hooks_dir })
}

fn git_rev_parse(root: &Path, arg: &str) -> anyhow::Result<PathBuf> {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).arg("rev-parse");
    for part in arg.split_whitespace() {
        command.arg(part);
    }
    let output = command.output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse {arg} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value = String::from_utf8(output.stdout)?.trim().to_string();
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() { path } else { root.join(path) })
}

pub(crate) fn install_hook(hooks_dir: &Path, hook: &str) -> anyhow::Result<()> {
    let path = hooks_dir.join(hook);
    if path.exists() && !is_rag_rat_hook(&path)? {
        anyhow::bail!(
            "{} already exists and is not managed by rag-rat; move it aside or merge manually",
            path.display()
        );
    }
    fs::write(&path, hook_script(hook))?;
    make_executable(&path)?;
    Ok(())
}

fn is_rag_rat_hook(path: &Path) -> anyhow::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(fs::read_to_string(path)?.contains(HOOK_MARKER))
}

fn hook_script(hook: &str) -> String {
    let command = match hook {
        "post-checkout" => {
            r#"rag-rat maintenance \
    --trigger post-checkout \
    --old-head "$1" \
    --new-head "$2" \
    --branch-checkout "$3" \
    --max-seconds 30"#
        },
        "post-merge" => {
            r#"rag-rat maintenance \
    --trigger post-merge \
    --max-seconds 30 \
    "$@""#
        },
        "post-rewrite" => {
            r#"rag-rat maintenance \
    --trigger post-rewrite \
    --max-seconds 30 \
    "$@""#
        },
        // git passes no positional args to post-commit; HEAD has already advanced, so the
        // maintenance discover-index re-keys the just-committed files under the new commit.
        "post-commit" => {
            r#"rag-rat maintenance \
    --trigger post-commit \
    --max-seconds 30"#
        },
        _ => unreachable!("unknown managed hook"),
    };
    format!(
        r#"#!/bin/sh
{HOOK_MARKER} Edit rag-rat config, not this hook.

if [ "${{RAG_RAT_HOOK_DISABLE:-}}" = "1" ]; then
  exit 0
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
cd "$repo_root" || exit 0

RAG_RAT_HOOK_DISABLE=1 \
  {command} >"${{TMPDIR:-/tmp}}/rag-rat-{hook}.log" 2>&1 &

exit 0
"#
    )
}

#[cfg(unix)]
fn make_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub(crate) fn render_index_progress(progress: IndexProgress) {
    match progress {
        IndexProgress::Started { database, mode } => {
            eprintln!("index: {} using {}", mode.label(), database.display());
        },
        IndexProgress::Discovering => {
            eprintln!("index: discovering files");
        },
        IndexProgress::Discovered { files } => {
            eprintln!("index: discovered {files} files");
        },
        IndexProgress::PreparingFile { current, total, path, language, kind } => {
            let percent = progress_percent(
                u64::try_from(current).unwrap_or(u64::MAX),
                u64::try_from(total).unwrap_or(u64::MAX),
            );
            eprintln!(
                "index: preparing {current}/{total} ({percent:>3}%) [{}:{}] {}",
                kind.as_str(),
                language.as_str(),
                path.display()
            );
        },
        IndexProgress::IndexingFile { current, total, path, language, kind } => {
            let percent = progress_percent(
                u64::try_from(current).unwrap_or(u64::MAX),
                u64::try_from(total).unwrap_or(u64::MAX),
            );
            eprintln!(
                "index: {current}/{total} ({percent:>3}%) [{}:{}] {}",
                kind.as_str(),
                language.as_str(),
                path.display()
            );
        },
        IndexProgress::IndexingGitHistory => {
            eprintln!("index: indexing git history");
        },
        IndexProgress::RebuildingLogicalSymbols => {
            eprintln!("index: rebuilding logical symbols");
        },
        IndexProgress::ResolvingGraph => {
            eprintln!("index: resolving graph edges");
        },
        IndexProgress::SyncingFts => {
            eprintln!("index: syncing SQLite FTS");
        },
        IndexProgress::RebuildingFts => {
            eprintln!("index: rebuilding SQLite FTS");
        },
        IndexProgress::Finished { files } => {
            eprintln!("index: complete ({files} files)");
        },
    }
}

fn usage() {
    eprintln!(
        "usage: rag-rat <init|index|doctor|migrate|query|brief|clusters|mcp|github|hooks|claude-hook|maintenance|models|reconcile|gc|eval|dump-config> [--config <path>] [query]\n\
         default config: rag-rat.toml\n\
         examples:\n\
         rag-rat init\n\
         rag-rat init --dry-run\n\
         rag-rat index\n\
         rag-rat index --changed\n\
         rag-rat index --discover\n\
         rag-rat index --full\n\
         rag-rat index --watch\n\
         rag-rat migrate --check\n\
         rag-rat github sync --from-refs\n\
         rag-rat hooks install\n\
         rag-rat hooks status\n\
         rag-rat maintenance --trigger post-checkout --max-seconds 30\n\
         rag-rat models list\n\
         rag-rat models install embedding-hash\n\
         rag-rat models install fastembed-all-minilm-l6-v2\n\
         rag-rat reconcile --plan\n\
         rag-rat reconcile --limit 100 --batch-size 32\n\
         rag-rat reconcile --changed-first --max-seconds 60 --batch-size 64\n\
         rag-rat reconcile --until-clean --batch-size 64\n\
         rag-rat gc\n\
         rag-rat reconcile --force --limit 100 --batch-size 32\n\
         rag-rat eval\n\
         rag-rat eval --json\n\
         rag-rat eval --update-baseline\n\
         rag-rat query \"semantic recall\"\n\
         rag-rat query --explain \"runtime shutdown\"\n\
         rag-rat brief --mode spine\n\
         rag-rat brief --mode churn\n\
         rag-rat brief --mode god_modules\n\
         rag-rat clusters --limit 10"
    );
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|window| window[0] == name).map(|window| window[1].clone())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}

fn positional_after_options(args: &[String]) -> Option<String> {
    let mut values = Vec::new();
    let mut skip_next = false;
    for arg in args.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--config"
            || arg == "--issue"
            || arg == "--limit"
            || arg == "--batch-size"
            || arg == "--queries"
            || arg == "--expected"
            || arg == "--trigger"
            || arg == "--max-seconds"
            || arg == "--max-embedding-chars"
            || arg == "--old-head"
            || arg == "--new-head"
            || arg == "--branch-checkout"
        {
            skip_next = true;
            continue;
        }
        if arg.starts_with("--") {
            continue;
        }
        values.push(arg.clone());
    }
    Some(values.join(" ")).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::progress_percent;

    #[test]
    fn progress_percent_is_capped() {
        assert_eq!(progress_percent(0, 0), 100);
        assert_eq!(progress_percent(50, 100), 50);
        assert_eq!(progress_percent(17_024, 11_998), 100);
    }
}
