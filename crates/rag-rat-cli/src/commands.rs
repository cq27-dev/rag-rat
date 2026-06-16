use std::sync::OnceLock;

use rag_rat_core::OutputFormat;

use super::*;
#[cfg(feature = "eval")]
use crate::cli::EvalArgs;
use crate::cli::{
    BriefArgs, ClustersArgs, GithubArgs, GithubCommand, HookAction, HooksArgs,
    ImportantSymbolsArgs, IndexArgs, MaintenanceArgs, MemoryArgs, MemoryCommand, ModelsArgs,
    ModelsCommand, OracleArgs, OracleCommand, OracleReportArgs, OracleRunArgs, OracleStatusArgs,
    QueryArgs, ReconcileArgs,
};

/// Process-wide output format, set once from the global `--json` flag in `main` before any command
/// runs. A `OnceLock` keeps `print_output` (~30 call sites) from threading an `OutputFormat`
/// through every command signature; it defaults to TOON if `main` never sets it (e.g. a unit test
/// calling a command helper directly).
static OUTPUT_FORMAT: OnceLock<OutputFormat> = OnceLock::new();

/// Set the global output format. Called once from `main` from the parsed `--json` flag; a second
/// call is a no-op (`OnceLock::set` returns `Err`), so tests can't accidentally clobber it.
pub(crate) fn set_output_format(format: OutputFormat) {
    let _ = OUTPUT_FORMAT.set(format);
}

/// The format `print_output` renders in — TOON unless `main` set JSON from `--json`.
pub(crate) fn output_format() -> OutputFormat {
    OUTPUT_FORMAT.get().copied().unwrap_or_default()
}

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
    print_output(&db.status(&config.database)?)
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
    print_output(&db.search(&query, 10, false)?)
}
pub(crate) fn brief(config: &Config, args: &BriefArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    let mode = rag_rat_core::query::repo_brief::RepoBriefMode::parse(args.mode.as_deref())?;
    print_output(&db.repo_brief(rag_rat_core::query::repo_brief::RepoBriefOptions {
        mode,
        limit: args.limit.unwrap_or(10),
        include_generated: args.include_generated,
        include_memories: !args.no_memories,
    })?)
}
pub(crate) fn clusters(config: &Config, args: &ClustersArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    print_output(&db.repo_clusters(rag_rat_core::query::clusters::RepoClustersOptions {
        limit: args.limit.unwrap_or(10),
        include_generated: args.include_generated,
        include_memories: !args.no_memories,
        min_cluster_size: args.min_cluster_size.unwrap_or(2),
    })?)
}
pub(crate) fn important_symbols(
    config: &Config,
    args: &ImportantSymbolsArgs,
) -> anyhow::Result<()> {
    let db = open_index(config)?;
    // CLI stays global-by-default: no auto-seed from the git diff (`auto_seed_from_diff: false`).
    // Only an explicit `--personalize` seeds — the intentional divergence from the MCP default.
    let mut result = db.important_symbols(rag_rat_core::index::ImportantSymbolsRequest {
        limit: args.limit.unwrap_or(20) as usize,
        personalize: args.personalize.clone(),
        auto_seed_from_diff: false,
    })?;
    apply_auto_run_ranking_hint(&mut result, config);
    print_output(&result)
}

/// Swap the heuristic-ranking nudge to the background-oracle wording when `[oracle] auto_run` is on
/// (the core method, config-unaware, emits the manual `oracle run` variant). No-op when the hint is
/// absent (a current oracle run exists) or auto_run is off.
pub(crate) fn apply_auto_run_ranking_hint(
    result: &mut rag_rat_core::query::pagerank::ImportantSymbolsResult,
    config: &Config,
) {
    if config.oracle.auto_run && result.ranking_hint.is_some() {
        result.ranking_hint =
            Some(rag_rat_core::query::pagerank::RANKING_HINT_AUTO_RUN.to_string());
    }
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
    print_output(&serde_json::json!({
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
/// `version-check`: refresh the crates.io cache (network, synchronous — this is the explicit path)
/// and print current vs latest plus how to update. Best-effort: an offline/refused check still
/// prints the current version with a null latest. No network when disabled in config.
pub(crate) fn version_check(config: &Config) -> anyhow::Result<()> {
    use rag_rat_core::version_check;
    if !config.version_check.enabled {
        return print_output(&serde_json::json!({
            "enabled": false,
            "current_version": version_check::current_version(),
            "note": "version checking is disabled ([version_check] enabled = false in rag-rat.toml)",
        }));
    }
    // Prefer the just-fetched result; only fall back to the cache when the network fetch itself
    // failed — so a successful check still reports even if the cache write didn't land (read-only
    // checkout, full disk).
    let cached = version_check::refresh(&config.database)
        .or_else(|| version_check::read_cache(&config.database));
    print_output(&version_check::build_status(version_check::current_version(), cached.as_ref()))
}
#[cfg(feature = "eval")]
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
    // `eval` prints a greppable human summary by default; the global `--json` (or a baseline
    // rewrite, which needs the machine record) switches to the structured report.
    if output_format() == OutputFormat::Json || options.update_baseline {
        print_output(&report)?;
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
#[cfg(feature = "eval")]
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
        OracleCommand::Report(report_args) => oracle_report(config, report_args),
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
pub(crate) fn with_oracle_write_lock<T>(
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
        return print_output(&serde_json::json!({
            "outcome": "completed",
            "tool": tool.as_db_str(),
            "tool_version": tool_version,
            "report": report,
        }));
    }

    // No pre-built index: produce the `.scip` with the tool BEFORE acquiring the write lock, so the
    // slow rust-analyzer subprocess doesn't hold the lock and starve the watcher (#82 P3). Only the
    // brief pre-spawn snapshot + the join/write run under the lock.
    //
    // Probe the tool FIRST so a missing/unrunnable tool yields the documented `Blocked` + exit-0
    // UX before anything touches the index (#88 review): opening the index is not guaranteed
    // side-effect free (a stale graph version triggers an edge reindex), and a Blocked probe
    // must not be preempted by an index-open failure.
    if let rag_rat_core::index::oracle::ToolAvailability::Blocked { tool, program, hint } =
        rag_rat_core::index::oracle::probe_oracle_tool(tool)
    {
        eprintln!("oracle: {hint}");
        return print_output(&rag_rat_core::index::oracle::OracleRunOutcome::Blocked {
            tool,
            program,
            hint,
        });
    }

    // Snapshot the indexed shas BEFORE spawning (#83). The query itself is a cheap read, but
    // `open_index` may upgrade a stale graph index (a WRITE — `ensure_graph_index_current`
    // rebuilds `edges`), so the snapshot takes the write lock briefly and releases it before the
    // subprocess spawns (#88 review). The join later requires every verdict's documents to still
    // carry these shas, so a file the watcher reindexes ANYWHERE in the spawn → join window —
    // including DURING the subprocess, which the post-exit `production_sha` snapshot cannot see —
    // is skipped, never mis-joined. A reindex slipping in between this lock release and the spawn
    // is detected by the same gate.
    // Stamp `started_at` INSIDE the same write-lock as the pre-spawn snapshot, so no watcher
    // reindex can land between reading the indexed state and recording the start. Under the lock,
    // started_at corresponds exactly to the indexed state this run covers: ≥ that indexed_at (so a
    // run covering fresh state isn't falsely judged stale even after a long lock wait) yet before
    // any mid-run reindex (so a run that misses one IS judged stale). (#145 + #146 review)
    let (started_at_ms, pre_spawn_sha) = with_oracle_write_lock(config, |db| {
        Ok((crate::now_epoch_ms(), db.oracle_pre_spawn_snapshot()?))
    })?;
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
            print_output(&rag_rat_core::index::oracle::OracleRunOutcome::Blocked {
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
            // The join's content gate revalidates against current disk bytes under the lock;
            // `production_sha` (per-document disk hashes captured the instant the subprocess
            // finished) pins the `.scip` to the content it was built against (#82 TOCTOU); and
            // `pre_spawn_sha` (indexed shas captured before the spawn) extends that pin across
            // the subprocess interior (#83) — together they cover the whole lock-free window, so
            // a file the watcher reindexes anywhere in it is skipped, not mis-joined. Run only
            // the join/write under the lock.
            let report = with_oracle_write_lock(config, |db| {
                db.run_oracle_at(
                    tool,
                    &version,
                    &bytes,
                    rag_rat_core::index::OracleShaSnapshots {
                        production: Some(&production_sha),
                        pre_spawn: Some(&pre_spawn_sha),
                    },
                    started_at_ms,
                )
            })?;
            print_output(&serde_json::json!({
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
    print_output(&entries)
}

/// `rag-rat oracle report --corpus <id>` — run the oracle for a declared corpus and emit its typed
/// C2 [`OracleResolutionReport`] (before/after edge resolution + verdicts + metrics, schema- and
/// profile-stamped) as JSON/TOON. The report is ALWAYS printed (so a Δ glue script can consume it
/// even on a failing run); then the per-corpus health gate runs and, on any violation, the command
/// exits non-zero — catching "scip emitted almost nothing" / "venv didn't resolve deps" / a broken
/// parse even when the underlying oracle command itself succeeded.
///
/// Unlike `oracle run`, a missing/unrunnable tool is a hard ERROR here, not the exit-0 `Blocked`
/// UX: this is a measurement runner over a corpus whose tool CI is expected to have installed, so a
/// silent skip would let a broken environment pass green.
fn oracle_report(config: &Config, args: &OracleReportArgs) -> anyhow::Result<()> {
    use rag_rat_core::index::oracle;

    // Load the corpus profile (defaults to the committed `tools/oracle-corpora.toml`).
    let corpora_path = args
        .corpora
        .clone()
        .unwrap_or_else(|| config.root.join("tools").join("oracle-corpora.toml"));
    let toml_str = fs::read_to_string(&corpora_path).map_err(|err| {
        anyhow::anyhow!("failed to read corpora file {}: {err}", corpora_path.display())
    })?;
    let corpora = oracle::load_corpora(&toml_str)?;
    let profile = oracle::corpus_by_id(&corpora, &args.corpus)
        .ok_or_else(|| {
            anyhow::anyhow!("no corpus `{}` in {}", args.corpus, corpora_path.display())
        })?
        .clone();

    // Map the corpus's declared tool id to an oracle backend.
    let tool = oracle::OracleTool::from_db_str(&profile.tool).ok_or_else(|| {
        anyhow::anyhow!(
            "corpus `{}` names unknown oracle tool `{}`",
            profile.corpus_id,
            profile.tool
        )
    })?;

    // Fail closed if the active checkout's target bindings don't match the corpus profile (Codex on
    // #175). The report stamps this profile's `corpus_profile_hash`, asserting "these numbers are
    // that corpus"; if `rag-rat oracle report --corpus X` is pointed at a checkout indexing a
    // different population (wrong repo/targets), the health gate could pass on the wrong numbers
    // while the report stays *comparable* under the intended hash — a silently-wrong Δ. (Tag-vs-SHA
    // resolution makes an in-command repo/rev check intractable; the runner guarantees those by
    // cloning repo@rev, so we validate the bindings, which independently fix the measured
    // population.)
    ensure_checkout_matches_corpus(config, &profile)?;

    let rag_rat_commit = rag_rat_commit_provenance();

    // Run the oracle + assemble the report + apply the health gate as ONE provisional transaction
    // (`db.run_oracle_report` commits only if healthy, else rolls the whole run back), all under
    // the index write lock so a watcher reindex can't interleave. Producing the `.scip` with
    // the tool happens OUTSIDE the lock (#82 P3): the slow subprocess must not starve the
    // watcher.
    let (report, violations) = if let Some(scip_path) = &args.scip {
        let scip_bytes = fs::read(scip_path).map_err(|err| {
            anyhow::anyhow!("failed to read SCIP index {}: {err}", scip_path.display())
        })?;
        let tool_version = format!(
            "scip-file:{}@{}",
            scip_path.file_name().and_then(|n| n.to_str()).unwrap_or("index.scip"),
            oracle::scip_content_fingerprint(&scip_bytes),
        );
        let provenance = oracle::RunProvenance {
            tool_version,
            rag_rat_commit: rag_rat_commit.clone(),
            // worktree_id filled under the lock (it's the active checkout's, read from the db).
            worktree_id: String::new(),
            production_sha: oracle::scip_content_fingerprint(&scip_bytes),
        };
        with_oracle_write_lock(config, |db| {
            let provenance =
                oracle::RunProvenance { worktree_id: db.active_worktree_id.clone(), ..provenance };
            // A pre-built `--scip` arms neither content-drift gate and has no spawn moment.
            db.run_oracle_report(
                &profile,
                &provenance,
                tool,
                &scip_bytes,
                rag_rat_core::index::OracleShaSnapshots::default(),
                crate::now_epoch_ms(),
            )
        })?
    } else {
        // No pre-built index: probe first so a missing tool fails before touching the index, then
        // snapshot the pre-spawn shas (under the lock) and produce the `.scip` outside it
        // (#82/#83).
        if let oracle::ToolAvailability::Blocked { hint, .. } = oracle::probe_oracle_tool(tool) {
            anyhow::bail!("oracle tool for corpus `{}` unavailable: {hint}", profile.corpus_id);
        }
        let (started_at_ms, pre_spawn_sha) = with_oracle_write_lock(config, |db| {
            Ok((crate::now_epoch_ms(), db.oracle_pre_spawn_snapshot()?))
        })?;
        let scip_output = config
            .database
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("rag-rat-oracle-report-{}.scip", std::process::id()));
        let production = oracle::produce_scip_with_tool(tool, &config.root, &scip_output);
        let _ = fs::remove_file(&scip_output);
        match production? {
            oracle::ScipProduction::Blocked { hint, .. } => {
                anyhow::bail!("oracle tool for corpus `{}` unavailable: {hint}", profile.corpus_id);
            },
            oracle::ScipProduction::Produced { version, bytes, production_sha } => {
                let provenance = oracle::RunProvenance {
                    tool_version: version,
                    rag_rat_commit: rag_rat_commit.clone(),
                    worktree_id: String::new(),
                    production_sha: oracle::scip_content_fingerprint(&bytes),
                };
                with_oracle_write_lock(config, |db| {
                    let provenance = oracle::RunProvenance {
                        worktree_id: db.active_worktree_id.clone(),
                        ..provenance
                    };
                    db.run_oracle_report(
                        &profile,
                        &provenance,
                        tool,
                        &bytes,
                        rag_rat_core::index::OracleShaSnapshots {
                            production: Some(&production_sha),
                            pre_spawn: Some(&pre_spawn_sha),
                        },
                        started_at_ms,
                    )
                })?
            },
        }
    };

    // Emit the report unconditionally — the glue/Δ script consumes it even for a failing run (an
    // unhealthy run was already rolled back whole inside the transaction).
    print_output(&report)?;

    // Health gate: a violated threshold means the run is untrustworthy, so exit non-zero even
    // though the oracle command itself succeeded. Violations go to stderr (the report owns stdout).
    if !violations.is_empty() {
        for violation in &violations {
            eprintln!("corpus health [{}]: {}", violation.check, violation.detail);
        }
        anyhow::bail!(
            "corpus `{}` failed {} health threshold(s)",
            profile.corpus_id,
            violations.len()
        );
    }
    Ok(())
}

/// Fail closed unless the active checkout's targets are EXACTLY the corpus profile's `bindings`
/// rendered through the plain `[target_bindings]` form — same languages, same directories, AND
/// default include/exclude filters. The corpus runner generates the checkout's `rag-rat.toml` from
/// these bindings (the simple form), so that's the invariant; any deviation means
/// `oracle report --corpus X` would stamp X's `corpus_profile_hash` onto a different file
/// population.
///
/// Two layers, both load-bearing:
/// 1. language → directory set equality (catches a wrong repo / extra or missing bindings).
/// 2. default filters per target: a `[[target]]` with the same language+dirs but custom
///    `include`/`exclude` indexes a filtered subset/superset under the same hash (Codex on #175),
///    so reject any target whose filters aren't the simple form's defaults (`include =
///    ["**/*.<ext>", …]`, `exclude = []`).
fn ensure_checkout_matches_corpus(
    config: &Config,
    profile: &rag_rat_core::index::oracle::CorpusProfile,
) -> anyhow::Result<()> {
    use std::collections::{BTreeMap, BTreeSet};

    let mut actual: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for target in &config.targets {
        actual
            .entry(target.language.as_str().to_string())
            .or_default()
            .extend(target.directories.iter().map(|dir| dir.to_string_lossy().into_owned()));
    }
    let expected: BTreeMap<String, BTreeSet<String>> = profile
        .bindings
        .iter()
        .map(|(lang, dirs)| (lang.clone(), dirs.iter().cloned().collect()))
        .collect();

    anyhow::ensure!(
        actual == expected,
        "active checkout target bindings {actual:?} do not match corpus `{}` bindings \
         {expected:?} — run `oracle report` against a checkout indexed with the corpus's bindings \
         (the corpus runner does this) so the report measures the population its profile hash \
         claims",
        profile.corpus_id,
    );

    // The dir set can match while a custom `include`/`exclude` quietly filters the indexed files.
    // The corpus binding has no filter knobs, so a legitimate checkout carries exactly the simple
    // form's defaults; anything else measures a different population under the same hash.
    for target in &config.targets {
        let default_include: BTreeSet<String> =
            target.language.default_include_globs().into_iter().collect();
        let include: BTreeSet<String> = target.include.iter().cloned().collect();
        anyhow::ensure!(
            target.exclude.is_empty() && include == default_include,
            "target `{}` ({}) has custom include/exclude filters (include {:?}, exclude {:?}) — \
             `oracle report --corpus {}` requires the corpus's plain bindings (default filters) \
             so the report's profile hash matches the file population it measured",
            target.name,
            target.language.as_str(),
            target.include,
            target.exclude,
            profile.corpus_id,
        );
    }
    Ok(())
}

/// The `rag_rat_commit` provenance stamp for a resolution report: CI exports `RAG_RAT_COMMIT`
/// (the building checkout's git SHA) so a number traces to an exact engine build; off CI it falls
/// back to the crate version, which is enough to disambiguate published builds.
fn rag_rat_commit_provenance() -> String {
    std::env::var("RAG_RAT_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

pub(crate) fn models(config: &Config, args: &ModelsArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    match &args.command {
        None | Some(ModelsCommand::List) => print_output(&db.list_models()?),
        Some(ModelsCommand::Install { model_id }) => print_output(&db.install_model(model_id)?),
    }
}
pub(crate) fn reconcile(config: &Config, args: &ReconcileArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    if args.plan {
        let plan = db.reconcile_plan()?;
        // `--plan` prints a human summary by default; the global `--json` switches to the
        // structured plan.
        if output_format() == OutputFormat::Json {
            print_output(&plan)?;
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
    print_output(&report)
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
    print_output(&serde_json::json!({
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
        MemoryCommand::Doctor => {
            let db = open_index(config)?;
            let entries = db.memory_doctor()?;
            // Human-readable rebind suggestions by default; the global `--json` emits the
            // structured doctor entries instead.
            if output_format() == OutputFormat::Json {
                print_output(&entries)?;
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
            print_output(&db.memory_rebind(memory_id, bind)?)
        },
        MemoryCommand::List { kind } => {
            let db = open_index(config)?;
            let summaries = db.memory_list(kind.as_deref())?;
            // The global `--json` emits the structured list (a caller parsing stdout gets JSON, not
            // the human lines below).
            if output_format() == OutputFormat::Json {
                return print_output(&summaries);
            }
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
            // The global `--json` emits the structured memory instead of the human view below.
            if output_format() == OutputFormat::Json {
                return print_output(&memory);
            }
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
            print_output(&report)
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
            print_output(&serde_json::json!({
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
            print_output(&serde_json::json!({
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
            print_output(&serde_json::json!({
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
            print_output(&serde_json::json!({
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
            print_output(&serde_json::json!({
                "status": if changed { "uninstalled" } else { "not_installed" },
                "settings_path": path,
            }))
        },
        "status" => {
            let status = claude_settings::hook_status(&settings);
            print_output(&serde_json::json!({
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
        print_output(&serde_json::json!({
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
    let gc_report = db.garbage_collect().ok();
    // Re-anchor repo memories: post-checkout/merge/rewrite/commit are exactly when files move,
    // rename, or change, so relocate symbol/chunk bindings (or flag them) here rather than
    // leaving stale anchors until a manual memory_validate.
    let memory_validation = db.memory_validate().ok();
    let plan = db.reconcile_plan()?;
    print_output(&serde_json::json!({
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
            version_check: Default::default(),
            oracle: Default::default(),
        };
        (root, config)
    }

    #[test]
    fn checkout_bindings_must_match_corpus() {
        use std::collections::BTreeMap;

        use rag_rat_core::index::oracle::{CorpusHealth, CorpusProfile};

        // A target carrying the SAME default filters the simple `[target_bindings]` form renders
        // (`include = ["**/*.rs"]`, no exclude), so the bindings-match check accepts it.
        let config_with = |include: Vec<String>, exclude: Vec<String>| Config {
            root: PathBuf::from("/x"),
            database: PathBuf::from("/x/db"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include,
                exclude,
                kind: TargetKind::Source,
            }],
            local_ai: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
        };
        let config = config_with(vec!["**/*.rs".to_string()], Vec::new());
        let profile = |dirs: &[&str]| {
            let mut bindings = BTreeMap::new();
            bindings.insert("rust".to_string(), dirs.iter().map(|d| d.to_string()).collect());
            CorpusProfile {
                corpus_id: "rust-semver".to_string(),
                tier: "small".to_string(),
                repo: "r".to_string(),
                rev: "v".to_string(),
                tool: "rust-analyzer".to_string(),
                prepare: Vec::new(),
                bindings,
                health: CorpusHealth {
                    expected_min_heuristic_edges: 1,
                    expected_min_oracle_examined: 1,
                    expected_max_skipped_drifted: 0,
                    expected_min_symbols_with_moniker: 1,
                    expected_min_resolved_external: None,
                    timeout_minutes: 1,
                },
            }
        };
        // Exact language+dirs match with default filters → ok.
        assert!(super::ensure_checkout_matches_corpus(&config, &profile(&["src"])).is_ok());
        // Same language, different directory → fail closed (a different population).
        assert!(super::ensure_checkout_matches_corpus(&config, &profile(&["lib"])).is_err());
        // Extra binding the checkout doesn't have → fail closed.
        let mut two_langs = profile(&["src"]);
        two_langs.bindings.insert("python".to_string(), vec!["pkg".to_string()]);
        assert!(super::ensure_checkout_matches_corpus(&config, &two_langs).is_err());
        // Same dirs but a custom `exclude` filters the population → fail closed (Codex #175).
        let excluded =
            config_with(vec!["**/*.rs".to_string()], vec!["**/generated/**".to_string()]);
        assert!(super::ensure_checkout_matches_corpus(&excluded, &profile(&["src"])).is_err());
        // Same dirs but a narrowed `include` → fail closed.
        let narrowed = config_with(vec!["src/lib.rs".to_string()], Vec::new());
        assert!(super::ensure_checkout_matches_corpus(&narrowed, &profile(&["src"])).is_err());
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
