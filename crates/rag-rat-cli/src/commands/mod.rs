use std::sync::OnceLock;

use rag_rat_core::OutputFormat;

use super::*;
#[cfg(feature = "eval")]
use crate::cli::EvalArgs;
use crate::cli::{
    BriefArgs, ClonesArgs, ClonesForArgs, ClustersArgs, DreamArgs, GithubArgs, GithubCommand,
    HookAction, HooksArgs, ImportantSymbolsArgs, IndexArgs, MaintenanceArgs, MemoryArgs,
    MemoryCommand, ModelsArgs, ModelsCommand, QueryArgs, ReconcileArgs,
};

mod oracle;
pub(crate) use oracle::{oracle, with_oracle_write_lock};

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
    let _lock = rag_rat_core::locks::WriteLock::acquire_blocking(&config.database)?;
    // `--worktree`: index a linked worktree's branch overlay on top of the existing base index
    // (#219). A distinct mode — the delta vs the base, not a base (re)build — so handle it before
    // the full/discover/changed branches.
    if let Some(worktree) = &args.worktree {
        let mut db = open_index(config)?;
        let mut progress = render_index_progress;
        // Index the overlay with the LINKED worktree's OWN target set (its branch `rag-rat.toml`),
        // not the launching process's base targets — a branch that adds/narrows targets must be
        // indexed by its own config or its overlay rows are filtered/pruned (#219 review).
        let overlay_config = config.for_linked_worktree_overlay(worktree);
        let report = db.index_worktree_overlay(&overlay_config, worktree, &mut progress)?;
        if report.worktree_id.is_empty() {
            anyhow::bail!(
                "{} is not a linked worktree of {} — nothing indexed",
                worktree.display(),
                config.root.display()
            );
        }
        eprintln!(
            "worktree overlay [{}]: {} indexed, {} tombstoned, {} pruned",
            report.worktree_id, report.indexed, report.tombstoned, report.pruned
        );
        return Ok(());
    }
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

/// Dream-mode worklist (#122): run the deterministic memory-maintenance pass (coverage gaps +
/// stale references), sync it into `dream_findings`, and render the open worklist. Writes ONLY to
/// `dream_findings` — never mutates a memory.
pub(crate) fn dream(config: &Config, args: &DreamArgs) -> anyhow::Result<()> {
    // `dream` WRITES dream_findings — serialize with the watcher/index like every other write
    // command (index/maintenance/oracle); WriteLock is reentrant so the open-time migrate is safe.
    let _lock = rag_rat_core::locks::WriteLock::acquire_blocking(&config.database)?;
    let db = open_index(config)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let report = db.dream_run(rag_rat_core::dream::DreamOptions {
        now_ms,
        limit: args.limit.unwrap_or(20) as usize,
    })?;
    print_output(&report)
}

pub(crate) fn clones(config: &Config, args: &ClonesArgs) -> anyhow::Result<()> {
    // `--precompute`: the WRITER path — build/refresh the persisted clone-edge graph (#286) under a
    // write lock (mirroring `maintenance`), then print the build report instead of a clone listing.
    if args.precompute {
        let _lock = rag_rat_core::locks::WriteLock::acquire_blocking(&config.database)?;
        let db = open_index(config)?;
        let report: rag_rat_core::index::CloneEdgeReport =
            db.precompute_clone_graph(args.max_seconds)?;
        return print_output(&report);
    }

    let db = open_index(config)?;

    // `--recall-symbols`: the UNCAPPED symbol-level recall set (#282 follow-up) — via the dedicated
    // pipeline, NOT find_clones (whose per-class member list is capped). One ref per line, sorted.
    if args.recall_symbols {
        for r in db.clone_symbol_refs(args.min_similarity, args.min_copies)? {
            println!("{r}");
        }
        return Ok(());
    }

    let result = db.find_clones(rag_rat_core::index::FindClonesOptions {
        min_similarity: args.min_similarity,
        min_copies: args.min_copies,
        // A recall signature must be COMPLETE (every class), so never clamp it to a class limit.
        limit: if args.recall_signature { None } else { args.limit },
    })?;

    // `--recall-signature`: a canonical, cross-build-stable recall dump for the #279 harness,
    // instead of the listing/explain.
    if args.recall_signature {
        print!("{}", recall_signature(&result));
        return Ok(());
    }

    // `--explain <CLASS_KEY>`: print a human-readable refinement breakdown for one class from the
    // SAME result set (so the explained class went through the same refine pass as the listing),
    // instead of the JSON/TOON listing.
    if let Some(key) = &args.explain {
        let Some(class) = result.classes.iter().find(|c| &c.class_key == key) else {
            anyhow::bail!("no clone class with key `{key}` in results");
        };
        print_clone_explain(class);
        return Ok(());
    }

    print_output(&result)
}

/// A canonical, cross-build-STABLE recall signature of the clone classes — one line per class
/// (`<member_count>\t<comma-joined sorted member refs>`), lines sorted, after a `#`-comment
/// summary; trailing newline included. Stable because it keys on member REFS (`path::symbol`), not
/// rowids, so two builds (before/after a candidate-pruning change like #271's hot-token cap) diff
/// with plain `diff`: a removed or shrunk line is a recall regression. The recall half of #279.
/// `member_count` is the FULL class size (the returned member list may be member-capped, but the
/// count plus the deterministic capped subset still pin the class, so a class that vanishes /
/// splits / shrinks always changes its line). Pure (returns the text) so it is unit-testable.
fn recall_signature(result: &rag_rat_core::index::FindClonesResult) -> String {
    let mut lines: Vec<String> = result
        .classes
        .iter()
        .map(|c| {
            let mut refs: Vec<&str> = c.members.iter().map(|m| m.r#ref.as_str()).collect();
            refs.sort_unstable();
            format!("{}\t{}", c.member_count, refs.join(","))
        })
        .collect();
    lines.sort_unstable();
    let total_members: usize = result.classes.iter().map(|c| c.member_count).sum();
    let mut out = format!(
        "# clone recall signature — {} classes, {total_members} clone members\n",
        result.classes.len(),
    );
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Render a human-readable explanation of a refined clone class: the anti-unification template,
/// its variation points (with per-member values), and the proposed extracted-helper signature.
/// Reads the parsed `variation_points` / `proposed_signature` JSON values surfaced on the class
/// (Plan 4b); un-refined classes simply print the header with `n/a` fields.
fn print_clone_explain(class: &rag_rat_core::index::CandidateCloneClass) {
    println!("Clone class: {}", class.class_key);
    println!(
        "  {} members, confidence: {}, coverage: {:.2}",
        class.member_count,
        class.confidence.as_deref().unwrap_or("n/a"),
        class.anti_unify_coverage.unwrap_or(0.0),
    );
    println!();

    if let Some(template) = &class.template {
        println!("Template:");
        println!("{template}");
        println!();
    }

    if let Some(arr) = class.variation_points.as_ref().and_then(|v| v.as_array())
        && !arr.is_empty()
    {
        // `per_member_values` is ordinal-aligned to `canonical_member_refs` (canonical
        // `(struct_hash, path, start_byte)` order) — NOT to the `r#ref`-sorted `members` field.
        // Pair each value with its member ref so a printed value maps to the member it came
        // from. Falls back to the bare `value | value` join when the canonical refs are
        // unavailable (un-refined / legacy class).
        let canon_refs = class.canonical_member_refs.as_deref();
        println!("Variation points ({}):", arr.len());
        for vp in arr {
            let id = vp["metavar_id"].as_str().unwrap_or("?");
            let role = vp["extraction_role"].as_str().unwrap_or("?");
            let conf = vp["confidence"].as_str().unwrap_or("?");
            print!("  {id} ({role}, {conf})");
            if let Some(vals) = vp["per_member_values"].as_array() {
                let rendered: Vec<String> = match canon_refs {
                    // Zip value↔member when the canonical refs line up (same arity).
                    Some(refs) if refs.len() == vals.len() => vals
                        .iter()
                        .zip(refs.iter())
                        .map(|(v, r)| {
                            let val = v.as_str().unwrap_or("");
                            // The gap sentinel is the empty string — render it explicitly.
                            let shown = if val.is_empty() { "<gap>" } else { val };
                            format!("{r}={shown}")
                        })
                        .collect(),
                    // No refs / arity mismatch → bare values (still useful, just unlabeled).
                    _ => vals.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect(),
                };
                print!(": {}", rendered.join(" | "));
            }
            println!();
        }
        println!();
    }

    if let Some(sig) = &class.proposed_signature {
        let typedness = sig["typedness"].as_str().unwrap_or("unknown");
        println!("Proposed signature (typedness: {typedness}):");
        // `ProposedSignature` serializes a pre-rendered `text` (e.g. `fn extracted(arg0: i32)`);
        // fall back to assembling the params array if a legacy row lacks it.
        if let Some(text) = sig["text"].as_str() {
            println!("  {text}");
        } else if let Some(params) = sig["params"].as_array() {
            let param_strs: Vec<String> = params
                .iter()
                .map(|p| {
                    let name = p["name"].as_str().unwrap_or("_");
                    match p["type_text"].as_str() {
                        Some(t) => format!("{name}: {t}"),
                        None => name.to_string(),
                    }
                })
                .collect();
            println!("  fn extracted({}) {{ ... }}", param_strs.join(", "));
        }
    }
}

pub(crate) fn clones_for(config: &Config, args: &ClonesForArgs) -> anyhow::Result<()> {
    use rag_rat_core::index::CloneSymbolSelector;

    let db = open_index(config)?;

    // Validate selector: positional SYMBOL xor --path+--line; both/neither → handler error.
    let selector = match (&args.symbol, &args.path, &args.line) {
        (Some(sym), None, None) =>
        // Treat as Id ONLY when there is no `::` (which signals a qualified-name ref like
        // `sym_utils.rs::load_user`) AND the token parses as a valid sym_<hex> handle. A
        // file named `sym_*` with a `::` separator must route to Ref, not Id, so it resolves
        // by qualified name instead of failing `parse_sym_handle` and returning unresolved.
            if !sym.contains("::") && rag_rat_core::serde_big_id::parse_sym_handle(sym).is_some() {
                CloneSymbolSelector::Id(sym.clone())
            } else {
                CloneSymbolSelector::Ref(sym.clone())
            },
        (None, Some(path), Some(line)) =>
            CloneSymbolSelector::PathLine { path: path.clone(), line: *line },
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            anyhow::bail!(
                "clones-for: SYMBOL and --path/--line are mutually exclusive — use one or the \
                 other"
            );
        },
        (None, Some(_), None) | (None, None, Some(_)) => {
            anyhow::bail!("clones-for: --path and --line must be used together");
        },
        (None, None, None) => {
            anyhow::bail!("clones-for: requires a SYMBOL argument or --path <PATH> --line <N>");
        },
    };

    // The result always carries eligibility flags + completeness; a miss serializes with
    // `class: null` (symbol unique, not eligible, or unresolved) — never an error.
    let result = db.clones_for_symbol(selector)?;
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
    // Parent-state replay is a distinct flow: it scores each case against its own freshly-built
    // parent index (no shared HEAD db, no static suite / oracle baseline), so it has its own entry
    // point and report shape rather than threading through `run`.
    if args.replay_parent_state {
        let report = rag_rat_core::eval::run_replay_parent_state(
            config,
            &rag_rat_core::eval::ReplayOptions {
                max_cases: args.replay_max_cases,
                max_files: args.replay_max_files,
            },
        )?;
        print_output(&report)?;
        return Ok(());
    }
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
        replay: args.replay.then_some(rag_rat_core::eval::ReplayOptions {
            max_cases: args.replay_max_cases,
            max_files: args.replay_max_files,
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
    let (index, discovery, storage, clone_fingerprints) =
        if schema.state == rag_rat_core::index::schema::SchemaState::Compatible {
            let db = IndexDatabase::open_config(config)?;
            let mut index_status = serde_json::to_value(db.status(&config.database)?)?;
            // Schema (incl. the migrations list) is reported once at the top level from
            // `migration_check`; drop the duplicate copy nested in `index` so `doctor` doesn't list
            // migrations twice.
            if let Some(object) = index_status.as_object_mut() {
                object.remove("schema");
            }
            (
                Some(index_status),
                Some(serde_json::to_value(db.discovery_status(config)?)?),
                Some(serde_json::to_value(db.storage_status()?)?),
                Some(serde_json::to_value(db.clone_fingerprint_health()?)?),
            )
        } else {
            (None, None, None, None)
        };
    print_output(&serde_json::json!({
        "config_root": config.root,
        "database": config.database,
        "schema": schema,
        "storage": storage,
        "discovery": discovery,
        "clone_fingerprints": clone_fingerprints,
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
    let branch_checkout = args.branch_checkout.clone();
    let old_head = args.old_head.clone();
    let new_head = args.new_head.clone();

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

    // Single-flight coalescing (#267): a single amend/merge/rebase fires several git hooks
    // (post-commit + post-rewrite, post-merge + post-commit, post-rewrite + post-checkouts), each
    // backgrounding `rag-rat maintenance`. Without coalescing they serialize on the write lock and
    // each runs a full discover pass — doubling work and widening the SQLITE_BUSY window for MCP
    // reads (#220). The first trigger to take the maintenance lock runs; concurrent triggers set a
    // "rerun pending" marker and exit immediately; the runner re-checks the marker after its pass
    // and runs once more to cover a change that arrived mid-pass. The pass still takes the write
    // lock internally, so serialization with the watcher is unchanged.
    let pending = rag_rat_core::locks::maintenance_pending_path(&config.database);
    let lock_path = rag_rat_core::locks::maintenance_lock_path(&config.database);
    let Some(_maint) = rag_rat_core::locks::FileLock::try_acquire(&lock_path)? else {
        let _ = fs::File::create(&pending);
        return print_output(&serde_json::json!({
            "trigger": trigger,
            "status": "skipped",
            "reason": "another maintenance pass is in flight (coalesced, #267)",
            "old_head": old_head,
            "new_head": new_head,
        }));
    };

    let mut report;
    loop {
        // This pass covers the current state, so clear any prior rerun request first; a trigger
        // that fires after this point re-sets it and earns the rerun below.
        let _ = fs::remove_file(&pending);
        report = run_maintenance_pass(config, args, &trigger)?;
        if !pending.exists() {
            break;
        }
    }
    print_output(&report)
}

/// One maintenance pass: discover-index under the write lock, refresh every live linked-worktree
/// overlay, run the budgeted embedding reconcile, GC dead git contexts, and re-validate repo-memory
/// anchors. Returns the report object — the caller prints it, after a coalesced rerun if one was
/// requested mid-pass (see [`maintenance`]).
fn run_maintenance_pass(
    config: &Config,
    args: &MaintenanceArgs,
    trigger: &str,
) -> anyhow::Result<serde_json::Value> {
    let max_seconds = args.max_seconds.unwrap_or(DEFAULT_MAINTENANCE_SECONDS);
    let started = Instant::now();

    // Serialize with the background watcher (and other writers). The hook backgrounds this command,
    // so blocking here never holds up the git operation; busy_timeout backstops the query-path
    // heal.
    let _lock = rag_rat_core::locks::WriteLock::acquire_blocking(&config.database)?;

    let mut db = IndexDatabase::index_discover_with_progress(config, render_index_progress)?;
    // ONE time budget for the whole pass — the per-overlay embedding reconciles AND the base
    // reconcile below — measured from `started` so discovery already counts against it. Without a
    // shared budget each overlay (each call starts its own `max_seconds` timer) plus the base could
    // spend the full `--max-seconds`, holding the write lock (N+1)× past the advertised limit (#219
    // review). A `0` cap means the caller asked to skip embedding work entirely.
    let budget = (max_seconds > 0).then(|| {
        rag_rat_core::watch::ReconcileBudget::new(
            rag_rat_core::index::ai::ReconcileOptions {
                limit: None,
                batch_size: Some(config.local_ai.embedding.runtime.batch_size),
                force: false,
                until_clean: false,
                changed_first: true,
                max_seconds: Some(max_seconds),
                max_embedding_chars: config.local_ai.embedding.runtime.max_embedding_chars,
                intra_threads: config.local_ai.embedding.runtime.ort_threads.map(|n| n as usize),
            },
            started,
        )
    });
    // Keep every live linked worktree's branch overlay fresh (#219). The git hooks run THIS command
    // (not the foreground watcher), so without this a commit/checkout/merge in a linked worktree
    // would index the base `config.root` but leave that worktree's overlay stale until a watcher
    // pass or a manual `index --worktree`. Delta-only + idle-safe, like the watcher's pass; a
    // CHANGED overlay's embeddings are reconciled INLINE (while scoped to it) so worktree queries
    // aren't BM25-only for branch content. It restores the base scope afterward so the base
    // reconcile/gc/memory-validate below run unscoped.
    rag_rat_core::watch::refresh_worktree_overlays(&mut db, config, budget.as_ref());
    // The base reconcile gets whatever budget the overlays left; `None` → exhausted (or no cap left
    // at all), so skip it rather than start a fresh full-budget reconcile.
    let reconcile_report =
        match budget.as_ref().and_then(rag_rat_core::watch::ReconcileBudget::next_options) {
            Some(options) =>
                Some(db.reconcile_with_options_progress(options, render_reconcile_progress)?),
            None => None,
        };
    // Prune index rows for git contexts that are no longer live (worktree-safe; keeps every
    // live worktree's HEAD). Cheap and bounded, so it runs every maintenance pass.
    let gc_report = db.garbage_collect().ok();
    // Clone-edge graph (#286): refresh the persisted graph when absent/stale with whatever budget
    // the embedding reconcile left (shared so the pass can't overrun), so the git-hook
    // maintenance keeps the graph warm too — not just the foreground watcher. Best-effort +
    // resumable across passes.
    let clone_graph_report = if db.pending_clone_graph().unwrap_or(false) {
        match budget.as_ref().and_then(rag_rat_core::watch::ReconcileBudget::next_options) {
            Some(options) => db.reconcile_clone_edges_with_budget(options.max_seconds).ok(),
            None => None,
        }
    } else {
        None
    };
    // Re-anchor repo memories: post-checkout/merge/rewrite/commit are exactly when files move,
    // rename, or change, so relocate symbol/chunk bindings (or flag them) here rather than
    // leaving stale anchors until a manual memory_validate.
    let memory_validation = db.memory_validate().ok();
    let plan = db.reconcile_plan()?;
    Ok(serde_json::json!({
        "trigger": trigger,
        "status": "complete",
        "old_head": args.old_head,
        "new_head": args.new_head,
        "branch_checkout": args.branch_checkout,
        "max_seconds": max_seconds,
        "elapsed_seconds": started.elapsed().as_secs_f64(),
        "reconcile": reconcile_report,
        "clone_graph": clone_graph_report,
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

    use rag_rat_core::config::{ResolvedTarget, TargetKind};
    use rag_rat_core::language::Language;
    use rag_rat_core::{Config, IndexDatabase};

    use crate::cli::ClonesArgs;

    static N: AtomicU64 = AtomicU64::new(0);

    /// Fix E: a qualified name like `sym_utils.rs::load_user` (a file literally named `sym_*`)
    /// must route to `Ref`, not `Id`. The old `starts_with("sym_")` guard misrouted it to `Id`,
    /// which fails `parse_sym_handle` and returns unresolved instead of trying Ref. Fix: treat as
    /// Id ONLY when there is no `::` AND `parse_sym_handle` succeeds.
    #[test]
    fn clones_for_sym_prefixed_ref_routes_to_ref_not_id() {
        use rag_rat_core::index::CloneSymbolSelector;
        use rag_rat_core::serde_big_id::parse_sym_handle;

        fn classify(sym: &str) -> &'static str {
            if !sym.contains("::") && parse_sym_handle(sym).is_some() { "Id" } else { "Ref" }
        }

        // A valid opaque handle (no `::`, valid hex suffix) → Id.
        let valid_handle = rag_rat_core::serde_big_id::format_sym_handle(42i64);
        assert_eq!(classify(&valid_handle), "Id", "a valid sym_<hex> handle must route to Id");

        // A file named `sym_*` with a `::` separator → Ref (the bug case).
        assert_eq!(classify("sym_utils.rs::load_user"), "Ref");
        assert_eq!(classify("sym_something::fn_name"), "Ref");

        // An ordinary qualified name → Ref.
        assert_eq!(classify("src/foo.rs::my_fn"), "Ref");

        // Confirm the actual `clones_for` handler uses the same logic by checking that a
        // `sym_utils.rs::load_user`-style arg produces a Ref selector (not an Id selector that
        // would silently fail). We test the routing branch directly since we can't easily plant
        // a `sym_*`-named file in a live DB within a unit test.
        //
        // The match arm in `clones_for` is now:
        //   if !sym.contains("::") && parse_sym_handle(sym).is_some() { Id } else { Ref }
        // which is what `classify` above mirrors. The assertions above cover it.
        let _ = CloneSymbolSelector::Ref("sym_utils.rs::load_user".to_string());
    }

    #[test]
    fn clones_handler_returns_class_for_planted_pair() {
        // Plant two identical functions in separate files → struct_hash fast path produces a clone
        // class. Validates that the `clones` command handler wires find_clones and prints output
        // without panicking.
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-clones-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let clone_body =
            "pub fn cloned_helper(x: i32, y: i32) -> i32 {\n    x + y + 42\n}\n".to_string();
        std::fs::write(root.join("src/lib.rs"), format!("{clone_body}pub mod a;\npub mod b;\n"))
            .unwrap();
        std::fs::write(root.join("src/a.rs"), &clone_body).unwrap();
        std::fs::write(root.join("src/b.rs"), &clone_body).unwrap();

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
        IndexDatabase::rebuild(&config).unwrap();

        let args = ClonesArgs {
            min_similarity: None,
            min_copies: Some(2),
            limit: None,
            explain: None,
            recall_signature: false,
            recall_symbols: false,
            precompute: false,
            max_seconds: None,
        };
        // The handler must not error.
        super::clones(&config, &args).unwrap_or_else(|err| panic!("clones handler failed: {err}"));

        // Query the DB directly to assert at least one class was found.
        let db = IndexDatabase::open_config(&config).unwrap();
        let result = db
            .find_clones(rag_rat_core::index::FindClonesOptions {
                min_similarity: None,
                min_copies: Some(2),
                limit: None,
            })
            .unwrap();
        assert!(
            result.classes.iter().any(|c| c.member_count >= 2),
            "expected at least one clone class with >=2 members for the planted pair: {:?}",
            result.classes
        );

        // #279 recall harness: the canonical signature is a sorted, ref-keyed dump — the planted
        // 3-way clone surfaces as one `3\t<sorted refs>` line under the `#` summary header, keyed
        // on stable `path::symbol` refs (so two builds diff with plain `diff`).
        let sig = super::recall_signature(&result);
        assert!(sig.starts_with("# clone recall signature —"), "signature header missing:\n{sig}");
        let clone_line = sig
            .lines()
            .find(|l| l.starts_with("3\t"))
            .unwrap_or_else(|| panic!("no 3-member class line in signature:\n{sig}"));
        for member in
            ["src/lib.rs::cloned_helper", "src/a.rs::cloned_helper", "src/b.rs::cloned_helper"]
        {
            assert!(clone_line.contains(member), "signature line missing {member}: {clone_line}");
        }
        // Refs are sorted WITHIN a line (a.rs < b.rs < lib.rs) — the cross-build-stable ordering.
        assert!(
            clone_line.find("src/a.rs") < clone_line.find("src/b.rs"),
            "member refs must be sorted within a class line: {clone_line}"
        );

        // #282 follow-up: clone_symbol_refs is the UNCAPPED symbol-level recall set — the 3 planted
        // clone-symbols, sorted, one per ref (no member cap).
        let syms = db.clone_symbol_refs(None, Some(2)).unwrap();
        for member in
            ["src/a.rs::cloned_helper", "src/b.rs::cloned_helper", "src/lib.rs::cloned_helper"]
        {
            assert!(
                syms.iter().any(|s| s == member),
                "clone_symbol_refs missing {member}: {syms:?}"
            );
        }
        assert!(syms.windows(2).all(|w| w[0] < w[1]), "clone_symbol_refs must be sorted+unique");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn maintenance_command_refreshes_a_linked_worktree_overlay() {
        // #219 review: the git hooks invoke `rag-rat maintenance` (NOT the foreground watcher), so
        // this command — not just `watch::maintenance_pass` — must refresh every live linked
        // worktree's branch overlay. Without it, a commit/checkout/merge in a linked worktree
        // indexes the base `config.root` but leaves the worktree overlay stale.
        let git = |dir: &std::path::Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-maint-overlay-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let main = root.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "base"]);
        let config = Config {
            root: main.clone(),
            database: main.join(".rag-rat/index.sqlite"),
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
        IndexDatabase::rebuild(&config).unwrap();

        let linked = root.join("wt");
        git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
        std::fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
        git(&linked, &["add", "-A"]);
        git(&linked, &["commit", "-qm", "branch"]);

        // Run the actual CLI maintenance command (the hook entry point).
        let args = super::MaintenanceArgs {
            trigger: Some("post-merge".to_string()),
            max_seconds: Some(0), // skip the embedding reconcile; we only assert the overlay
            branch_checkout: None,
            old_head: None,
            new_head: None,
        };
        super::maintenance(&config, &args).unwrap();

        // The worktree-scoped query now sees the branch version, populated by the maintenance pass.
        let mut db = IndexDatabase::open_config(&config).unwrap();
        db.use_worktree_scope(&config.root, Some(&linked)).unwrap();
        let names: Vec<String> =
            db.symbols("linked_fn", None, 10).unwrap().into_iter().map(|h| h.name).collect();
        assert!(
            names.contains(&"linked_fn".to_string()),
            "the maintenance command must populate the worktree overlay: {names:?}",
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn maintenance_coalesces_a_concurrent_trigger() {
        use rag_rat_core::locks::{FileLock, maintenance_lock_path, maintenance_pending_path};

        // #267: a single amend/merge/rebase fires several git hooks, each backgrounding
        // `rag-rat maintenance`. A concurrent trigger must coalesce — skip its pass and set the
        // rerun marker — rather than queue a redundant discover that widens the SQLITE_BUSY window.
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-maint-coalesce-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
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
        IndexDatabase::rebuild(&config).unwrap();

        let pending = maintenance_pending_path(&config.database);
        let args = super::MaintenanceArgs {
            trigger: Some("post-rewrite".to_string()),
            max_seconds: Some(0), // skip the embedding reconcile; we only assert coalescing
            branch_checkout: None,
            old_head: None,
            new_head: None,
        };

        // Hold the coordination lock to simulate an in-flight maintenance pass.
        let held =
            FileLock::try_acquire(&maintenance_lock_path(&config.database)).unwrap().unwrap();
        assert!(!pending.exists());
        // A concurrent trigger coalesces: it does NOT run a pass; it sets the rerun marker.
        super::maintenance(&config, &args).unwrap();
        assert!(pending.exists(), "a coalesced trigger sets the rerun-pending marker");
        drop(held);

        // With the lock free, maintenance runs a pass and clears the marker (the rerun covers the
        // change the coalesced trigger requested).
        super::maintenance(&config, &args).unwrap();
        assert!(!pending.exists(), "the runner clears the rerun marker after its pass");

        let _ = std::fs::remove_dir_all(&root);
    }
}
