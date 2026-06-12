use super::*;
use crate::cli::{
    BriefArgs, ClustersArgs, EvalArgs, GithubArgs, GithubCommand, HookAction, HooksArgs, IndexArgs,
    MaintenanceArgs, MemoryArgs, MemoryCommand, MigrateArgs, ModelsArgs, ModelsCommand, OracleArgs,
    OracleCommand, OracleRunArgs, OracleStatusArgs, QueryArgs, ReconcileArgs,
};

pub(crate) fn index(config: &Config, args: &IndexArgs) -> anyhow::Result<()> {
    if args.watch {
        return run_watch(config.clone());
    }
    // Serialize with the background watcher / other writers (busy_timeout backstops any heal on
    // the query path).
    let _lock = rag_rat_core::locks::FileLock::acquire_blocking(
        &rag_rat_core::locks::write_lock_path(&config.database),
    )?;
    let db = if args.full {
        IndexDatabase::rebuild_with_progress(config, render_index_progress)?
    } else if args.discover {
        IndexDatabase::index_discover_with_progress(config, render_index_progress)?
    } else {
        IndexDatabase::index_changed_with_progress(config, render_index_progress)?
    };
    // Re-anchor repo memories against the freshly indexed symbols/chunks so a moved or renamed
    // binding relocates (or is flagged) instead of silently pointing at a stale row. Memory rows
    // themselves are never deleted by indexing.
    if let Err(err) = db.memory_validate() {
        eprintln!("warning: repo-memory re-validation failed: {err}");
    }
    // After validate has refreshed anchor_status values, count non-current anchors with a
    // read-only query (doctor reads persisted values; no re-validation).
    let doctor_count = db.memory_doctor().map(|entries| entries.len()).unwrap_or(0);
    if doctor_count > 0 {
        eprintln!("⚠ {doctor_count} repo memories need re-anchoring — run 'rag-rat memory doctor'");
    }
    print_json(&db.status(&config.database)?)
}
pub(crate) fn query(config: &Config, args: &QueryArgs) -> anyhow::Result<()> {
    let query = args.query.join(" ");
    if query.trim().is_empty() {
        anyhow::bail!("query command needs a search string");
    }
    let db = open_index(config)?;
    if args.explain {
        print_query_explain(&db.search_explain(&query, 10, false)?);
        return Ok(());
    }
    print_json(&db.search(&query, 10, false)?)
}
pub(crate) fn brief(config: &Config, args: &BriefArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    let mode = rag_rat_core::query::repo_brief::RepoBriefMode::parse(args.mode.as_deref())?;
    print_json(&db.repo_brief(rag_rat_core::query::repo_brief::RepoBriefOptions {
        mode,
        limit: args.limit.unwrap_or(10),
        include_generated: args.include_generated,
        include_memories: !args.no_memories,
    })?)
}
pub(crate) fn clusters(config: &Config, args: &ClustersArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    print_json(&db.repo_clusters(rag_rat_core::query::clusters::RepoClustersOptions {
        limit: args.limit.unwrap_or(10),
        include_generated: args.include_generated,
        include_memories: !args.no_memories,
        min_cluster_size: args.min_cluster_size.unwrap_or(2),
    })?)
}
pub(crate) fn dump_config(config: &Config) -> anyhow::Result<()> {
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
    }))
}
pub(crate) fn eval(config: &Config, args: &EvalArgs) -> anyhow::Result<()> {
    let options = rag_rat_core::eval::EvalOptions {
        queries_path: args
            .queries
            .clone()
            .unwrap_or_else(|| default_eval_path(config, "queries.toml")),
        expected_path: args
            .expected
            .clone()
            .unwrap_or_else(|| default_eval_path(config, "expected_hits.toml")),
        update_baseline: args.update_baseline,
        scip_path: args.scip.clone().or_else(|| {
            let default = default_eval_path(config, "oracle.scip");
            default.exists().then_some(default)
        }),
    };
    let report = rag_rat_core::eval::run(config, &options)?;
    if args.json || options.update_baseline {
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
pub(crate) fn default_eval_path(config: &Config, file_name: &str) -> PathBuf {
    config.root.join("evals").join(file_name)
}

pub(crate) fn oracle(config: &Config, args: &OracleArgs) -> anyhow::Result<()> {
    match &args.command {
        OracleCommand::Run(run_args) => oracle_run(config, run_args),
        OracleCommand::Status(status_args) => {
            let db = open_index(config)?;
            oracle_status(&db, status_args)
        },
    }
}

/// Acquire the index write lock, open the DB, and run a CLOSURE under it. `oracle run` WRITES
/// `edge_oracle` / `oracle_runs`, so the join/write must serialize with the background watcher /
/// `index` — a concurrent indexer can delete+reinsert `edges` (cascading `edge_oracle`) between the
/// pass loading edge ids and writing verdicts. The lock is acquired BEFORE opening the DB so the
/// indexer can't slip in between open and the pass.
///
/// Scoped to JUST the join/write: the slow `rust-analyzer scip` subprocess runs OUTSIDE this (#82
/// P3), so the watcher isn't starved through the whole subprocess. The lock-free window that opens
/// between `.scip` production and the join is narrowed by the scip-vs-disk content gate: production
/// snapshots each document's disk hash at subprocess exit, and the join skips (never mis-joins) any
/// candidate whose call-site OR definition document drifted from that snapshot (#82 TOCTOU). The
/// snapshot is taken at exit, not when rust-analyzer read each file, so a mid-subprocess edit + a
/// pre-join reindex remains best-effort — pinning the pre-spawn `files.sha256` would close that
/// residual tail (follow-up).
fn with_oracle_write_lock<T>(
    config: &Config,
    body: impl FnOnce(&IndexDatabase) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _lock = rag_rat_core::locks::FileLock::acquire_blocking(
        &rag_rat_core::locks::write_lock_path(&config.database),
    )?;
    let db = open_index(config)?;
    body(&db)
}

/// `rag-rat oracle run` — either consume a pre-built `--scip` (deterministic; no tool needed) or
/// invoke the indexer to produce a `.scip` into a temp file and run the join over it. A missing /
/// unrunnable tool prints the install hint and exits 0 (the missing-embedding-model UX) — never an
/// error. Prints the `OracleReport` (or the `Blocked` outcome) as JSON.
fn oracle_run(config: &Config, args: &OracleRunArgs) -> anyhow::Result<()> {
    let tool = args.tool.core();
    if let Some(scip_path) = &args.scip {
        // Pre-built index: reading a file is fast, so this whole path runs under the lock.
        let scip_bytes = fs::read(scip_path).map_err(|err| {
            anyhow::anyhow!("failed to read SCIP index {}: {err}", scip_path.display())
        })?;
        // A pre-built index carries no detectable tool version; label the run by the source path's
        // file name AND a content fingerprint so re-running the same fixture is content-addressed
        // stably, while two DIFFERENT indexes that share a basename (`index.scip` from two trees)
        // get distinct run-ids instead of colliding onto one `tool_version` (#82 P3).
        let tool_version = format!(
            "scip-file:{}@{}",
            scip_path.file_name().and_then(|n| n.to_str()).unwrap_or("index.scip"),
            rag_rat_core::index::oracle::scip_content_fingerprint(&scip_bytes),
        );
        let report = with_oracle_write_lock(config, |db| {
            db.run_oracle_from_scip(tool, &tool_version, &scip_bytes)
        })?;
        return print_json(&serde_json::json!({
            "outcome": "completed",
            "tool": tool.as_db_str(),
            "tool_version": tool_version,
            "report": report,
        }));
    }

    // No pre-built index: produce the `.scip` with the tool BEFORE acquiring the write lock, so the
    // slow rust-analyzer subprocess doesn't hold the lock and starve the watcher (#82 P3). Only the
    // probe-recheck + join/write below run under the lock.
    let scip_output = config
        .database
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("rag-rat-oracle-{}.scip", std::process::id()));
    let production =
        rag_rat_core::index::oracle::produce_scip_with_tool(tool, &config.root, &scip_output);
    let _ = fs::remove_file(&scip_output);
    match production? {
        rag_rat_core::index::oracle::ScipProduction::Blocked { tool, program, hint } => {
            eprintln!("oracle: {hint}");
            print_json(&rag_rat_core::index::oracle::OracleRunOutcome::Blocked {
                tool,
                program,
                hint,
            })
        },
        rag_rat_core::index::oracle::ScipProduction::Produced {
            version,
            bytes,
            production_sha,
        } => {
            // The join's content gate revalidates against current disk bytes under the lock, and
            // `production_sha` (the per-document disk hashes captured the instant the subprocess
            // finished) pins the `.scip` to the content it was built against — so a file the
            // watcher reindexes in this lock-free window is skipped, not mis-joined
            // (#82 TOCTOU). Run only the join/write under the lock.
            let report = with_oracle_write_lock(config, |db| {
                db.run_oracle(tool, &version, &bytes, Some(&production_sha))
            })?;
            print_json(&serde_json::json!({
                "outcome": "completed",
                "tool": tool.as_db_str(),
                "tool_version": version,
                "report": report,
            }))
        },
    }
}

/// `rag-rat oracle status` — verdict counts for the latest run in this checkout, plus whether the
/// indexer tool is installed (its probe, a `Blocked` line when absent, never an error). Always an
/// ARRAY of per-tool objects: every known tool by default, one element under `--tool` — the shape
/// stays stable as language backends (#71 TS, #72 Kotlin) join the registry.
fn oracle_status(db: &IndexDatabase, args: &OracleStatusArgs) -> anyhow::Result<()> {
    let tools: Vec<rag_rat_core::index::oracle::OracleTool> = match args.tool {
        Some(tool) => vec![tool.core()],
        None => rag_rat_core::index::oracle::OracleTool::ALL.to_vec(),
    };
    let mut entries = Vec::with_capacity(tools.len());
    for tool in tools {
        let availability = db.probe_oracle_tool(tool);
        // Use the most recent run's version for the verdict counts; no run → no counts (status is
        // a read-only sibling — nothing to report against).
        let status = match db.latest_oracle_run_version(tool)? {
            Some(version) => Some(db.oracle_status(tool, &version)?),
            None => None,
        };
        entries.push(serde_json::json!({
            "tool": tool.as_db_str(),
            "tool_available": availability,
            "verdicts": status,
        }));
    }
    print_json(&entries)
}
pub(crate) fn models(config: &Config, args: &ModelsArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    match &args.command {
        None | Some(ModelsCommand::List) => print_json(&db.list_models()?),
        Some(ModelsCommand::Install { model_id }) => print_json(&db.install_model(model_id)?),
    }
}
pub(crate) fn reconcile(config: &Config, args: &ReconcileArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    if args.plan {
        let plan = db.reconcile_plan()?;
        if args.json {
            print_json(&plan)?;
        } else {
            print_reconcile_plan(&plan);
        }
        return Ok(());
    }
    let options = rag_rat_core::index::ai::ReconcileOptions {
        limit: args.limit,
        batch_size: args.batch_size.or(Some(config.local_ai.embedding.runtime.batch_size)),
        force: args.force,
        until_clean: args.until_clean,
        changed_first: args.changed_first,
        max_seconds: args.max_seconds,
        max_embedding_chars: args
            .max_embedding_chars
            .unwrap_or(config.local_ai.embedding.runtime.max_embedding_chars),
        intra_threads: config.local_ai.embedding.runtime.ort_threads.map(|n| n as usize),
    };
    let report = db.reconcile_with_options_progress(options, render_reconcile_progress)?;
    // After reconciling, surface non-current memory anchors so they don't rot silently.
    // Read-only count from persisted anchor_status; does not call memory_validate.
    let non_current = db.memory_anchor_health().map(|h| h.stale + h.gone).unwrap_or(0);
    if non_current > 0 {
        eprintln!("⚠ {non_current} repo memories need re-anchoring — run 'rag-rat memory doctor'");
    }
    print_json(&report)
}
pub(crate) fn run_watch(config: Config) -> anyhow::Result<()> {
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
pub(crate) fn set_env_if_absent(key: &str, value: Option<u32>) {
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
pub(crate) fn migrate(config: &Config, args: &MigrateArgs) -> anyhow::Result<()> {
    let status = if args.check {
        IndexDatabase::migration_check(&config.database)?
    } else {
        IndexDatabase::migrate(&config.database)?
    };
    print_json(&status)?;
    if args.check && status.state != rag_rat_core::index::schema::SchemaState::Compatible {
        anyhow::bail!("{}", status.message);
    }
    Ok(())
}
pub(crate) fn doctor(config: &Config) -> anyhow::Result<()> {
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
// Each `memory rebind` target sets one anchor field and defaults the rest, so the call sites
// below state only what differs.
fn symbol_bind_target(
    hit: &rag_rat_core::query::symbol::SymbolHit,
) -> rag_rat_core::query::memory::RepoMemoryBindTarget {
    rag_rat_core::query::memory::RepoMemoryBindTarget {
        symbol_id: Some(hit.symbol_id),
        logical_symbol_id: hit.logical_symbol_id,
        ..Default::default()
    }
}

fn path_bind_target(path: String) -> rag_rat_core::query::memory::RepoMemoryBindTarget {
    rag_rat_core::query::memory::RepoMemoryBindTarget { path: Some(path), ..Default::default() }
}

fn dir_bind_target(dir: String) -> rag_rat_core::query::memory::RepoMemoryBindTarget {
    rag_rat_core::query::memory::RepoMemoryBindTarget { dir: Some(dir), ..Default::default() }
}

fn chunk_bind_target(chunk_id: i64) -> rag_rat_core::query::memory::RepoMemoryBindTarget {
    rag_rat_core::query::memory::RepoMemoryBindTarget {
        chunk_id: Some(chunk_id),
        ..Default::default()
    }
}

pub(crate) fn memory(config: &Config, args: &MemoryArgs) -> anyhow::Result<()> {
    match &args.command {
        MemoryCommand::Doctor { json } => {
            let db = open_index(config)?;
            let entries = db.memory_doctor()?;
            if *json {
                print_json(&entries)?;
                let any_gone = entries.iter().any(|e| e.anchor_status == "gone");
                if any_gone {
                    anyhow::bail!("one or more memories have gone anchors");
                }
                return Ok(());
            }
            if entries.is_empty() {
                eprintln!("All active memory anchors are current.");
                return Ok(());
            }
            let mut any_gone = false;
            for entry in &entries {
                eprintln!("[{}] {} ({})", entry.anchor_status, entry.title, entry.memory_id);
                eprintln!("  binding: {} {}", entry.binding_kind, entry.binding_id);
                if entry.candidates.is_empty() {
                    if entry.anchor_status == "gone" {
                        eprintln!(
                            "  -> code appears deleted; rag-rat memory mark-obsolete {}",
                            entry.memory_id
                        );
                    }
                } else {
                    for candidate in &entry.candidates {
                        // Suggest --symbol-path (exact qualified-name match) rather than --symbol
                        // (substring): a fully-qualified candidate fed to --symbol would also hit
                        // longer siblings. Exact match plus cfg-group collapse makes this runnable.
                        eprintln!(
                            "  rag-rat memory rebind {} --symbol-path {}",
                            entry.memory_id, candidate
                        );
                    }
                }
                if entry.anchor_status == "gone" {
                    any_gone = true;
                }
            }
            if any_gone {
                anyhow::bail!("one or more memories have gone anchors");
            }
            Ok(())
        },
        MemoryCommand::Rebind { memory_id, symbol, symbol_path, symbol_id, path, chunk, dir } => {
            let db = open_index(config)?;
            let bind = if symbol.is_some() || symbol_path.is_some() || symbol_id.is_some() {
                let selector = rag_rat_core::query::symbol::SymbolSelector {
                    logical_symbol_id: None,
                    symbol_id: *symbol_id,
                    symbol_path: symbol_path.clone(),
                    symbol: symbol.clone(),
                    language: None,
                    allow_ambiguous: false,
                    limit: 10,
                };
                let label = symbol
                    .as_deref()
                    .or(symbol_path.as_deref())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("#{}", symbol_id.unwrap_or_default()));
                match db.select_symbol_for_bind(&selector)? {
                    Ok(Some(hit)) => symbol_bind_target(&hit),
                    Ok(None) => anyhow::bail!("symbol `{label}` not found"),
                    Err(disambiguation) => anyhow::bail!(
                        "symbol `{label}` is ambiguous — disambiguate with one of:\n{}",
                        disambiguation
                            .candidates
                            .iter()
                            .map(|c| format!(
                                "  --symbol-id {}   ({} in {})",
                                c.symbol_id, c.qualified_name, c.path
                            ))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                }
            } else if let Some(path) = path {
                path_bind_target(path.clone())
            } else if let Some(chunk_id) = chunk {
                chunk_bind_target(*chunk_id)
            } else if let Some(dir) = dir {
                dir_bind_target(dir.clone())
            } else {
                anyhow::bail!(
                    "memory rebind needs one of --symbol <name>, --symbol-path <path::name>, \
                     --symbol-id <id>, --path <path>, --chunk <id>, or --dir <dir>"
                );
            };
            print_json(&db.memory_rebind(memory_id, bind)?)
        },
        MemoryCommand::List { kind } => {
            let db = open_index(config)?;
            let summaries = db.memory_list(kind.as_deref())?;
            if summaries.is_empty() {
                eprintln!("No memories found.");
                return Ok(());
            }
            for s in &summaries {
                println!(
                    "{}  [{}/{}]  {}  ({}:{})",
                    s.memory_id, s.kind, s.status, s.title, s.binding_kind, s.binding_id
                );
            }
            Ok(())
        },
        MemoryCommand::Show { memory_id } => {
            let db = open_index(config)?;
            let Some(memory) = db.memory_get(memory_id)? else {
                anyhow::bail!("memory `{memory_id}` not found");
            };
            println!("Title:      {}", memory.title);
            println!("Kind:       {} / {} / {}", memory.kind, memory.status, memory.confidence);
            println!();
            println!("{}", memory.body);
            if !memory.bindings.is_empty() {
                println!();
                println!("Bindings:");
                for b in &memory.bindings {
                    println!("  {} {} [{}]", b.binding_kind, b.binding_id, b.anchor_status);
                }
            }
            Ok(())
        },
    }
}
pub(crate) fn github(config: &Config, args: &GithubArgs) -> anyhow::Result<()> {
    match &args.command {
        GithubCommand::Sync { from_refs, issue, offline } => {
            let db = open_index(config)?;
            let report = if let Some(issue) = issue {
                db.github_sync_issue(issue, *offline)?
            } else if *from_refs {
                db.github_sync_from_refs_with_progress(*offline, render_github_sync_progress)?
            } else {
                anyhow::bail!("github sync needs --from-refs or --issue <owner/repo#number>");
            };
            print_json(&report)
        },
    }
}
pub(crate) fn hooks(config: &Config, args: &HooksArgs) -> anyhow::Result<()> {
    if args.claude {
        return claude_hooks(config, args.action.as_str(), args.global);
    }
    let git = git_paths(&config.root)?;
    match args.action {
        HookAction::Install => {
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
        HookAction::Uninstall => {
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
        HookAction::Status => {
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
    }
}
pub(crate) fn claude_hooks(config: &Config, subcommand: &str, global: bool) -> anyhow::Result<()> {
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
            let status = claude_settings::hook_status(&settings);
            print_json(&serde_json::json!({
                "settings_path": path,
                "pretooluse_installed": status.pretooluse,
                "session_start_installed": status.session_start,
            }))
        },
        other => anyhow::bail!("unknown hooks subcommand `{other}`"),
    }
}
pub(crate) fn maintenance(config: &Config, args: &MaintenanceArgs) -> anyhow::Result<()> {
    let trigger = args.trigger.clone().unwrap_or_else(|| "manual".to_string());
    let max_seconds = args.max_seconds.unwrap_or(DEFAULT_MAINTENANCE_SECONDS);
    let branch_checkout = args.branch_checkout.clone();
    let old_head = args.old_head.clone();
    let new_head = args.new_head.clone();
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
    // so blocking here never holds up the git operation; busy_timeout backstops the query-path
    // heal.
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use rag_rat_core::config::{ResolvedTarget, TargetKind};
    use rag_rat_core::language::Language;
    use rag_rat_core::locks::{FileLock, write_lock_path};
    use rag_rat_core::{Config, IndexDatabase};

    use crate::cli::{OracleArgs, OracleCommand, OracleRunArgs, OracleToolArg};

    static N: AtomicU64 = AtomicU64::new(0);

    fn temp_config() -> (PathBuf, Config) {
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-oracle-lock-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n")
            .unwrap();
        let config = Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            local_ai: Default::default(),
            watch: Default::default(),
        };
        (root, config)
    }

    fn run_args() -> OracleArgs {
        // The `--scip` path is deterministic (no rust-analyzer); an empty (zero-byte) `.scip` is a
        // valid empty SCIP index → the pass completes writing no verdicts. We only assert the LOCK
        // discipline here, not the verdict content.
        OracleArgs {
            command: OracleCommand::Run(OracleRunArgs {
                tool: OracleToolArg::RustAnalyzer,
                scip: None, // set per-test to a written empty `.scip`
            }),
        }
    }

    /// #82 finding 5: `oracle run` acquires the repo write lock for the duration, so it can't race a
    /// concurrent indexer. We hold the write lock, kick off `oracle run` on a thread, and assert it
    /// does NOT complete while the lock is held; releasing the lock lets it finish.
    #[test]
    fn oracle_run_blocks_on_write_lock() {
        let (root, config) = temp_config();
        IndexDatabase::rebuild(&config).unwrap();
        // A valid empty SCIP index (zero-byte protobuf message) for the deterministic `--scip`
        // path.
        let scip_path = root.join("empty.scip");
        std::fs::write(&scip_path, []).unwrap();
        let mut args = run_args();
        if let OracleCommand::Run(run) = &mut args.command {
            run.scip = Some(scip_path);
        }

        // Hold the write lock the run must contend for.
        let lock = FileLock::acquire_blocking(&write_lock_path(&config.database)).unwrap();

        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let result = super::oracle(&config, &args);
            let _ = tx.send(result.is_ok());
        });

        // While we hold the lock, the run must be blocked acquiring it — nothing arrives.
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "oracle run completed while the write lock was held — it must block on the lock"
        );

        // Release the lock; the run proceeds and completes.
        drop(lock);
        let ok =
            rx.recv_timeout(Duration::from_secs(20)).expect("oracle run completes after unlock");
        assert!(ok, "oracle run should succeed once the lock is free");
        handle.join().unwrap();

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The lock is RELEASED after `oracle run` returns — a subsequent acquire succeeds immediately,
    /// proving the run doesn't leak the lock (which would wedge the watcher/index).
    #[test]
    fn oracle_run_releases_write_lock_after_completion() {
        let (root, config) = temp_config();
        IndexDatabase::rebuild(&config).unwrap();
        let scip_path = root.join("empty.scip");
        std::fs::write(&scip_path, []).unwrap();
        let mut args = run_args();
        if let OracleCommand::Run(run) = &mut args.command {
            run.scip = Some(scip_path);
        }

        super::oracle(&config, &args).unwrap();

        // The lock is free now — a non-blocking acquire must succeed.
        let lock = FileLock::try_acquire(&write_lock_path(&config.database)).unwrap();
        assert!(lock.is_some(), "oracle run must release the write lock when it returns");

        let _ = std::fs::remove_dir_all(&root);
    }
}
