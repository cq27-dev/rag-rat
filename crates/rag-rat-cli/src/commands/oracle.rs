//! `rag-rat oracle run|status|report` — the oracle subcommand surface (split out of the
//! `commands` god-module). `run` produces/consumes a `.scip` and joins it against the graph
//! under the index write lock (the slow tool runs outside the lock); `report` is the CI
//! measurement runner that fails closed. See the load-bearing lock / TOCTOU / gate invariants
//! captured as repo memories on `with_oracle_write_lock` / `oracle_report`.
use std::fs;
use std::path::Path;

use rag_rat_core::{Config, IndexDatabase};

use crate::cli::{OracleArgs, OracleCommand, OracleReportArgs, OracleRunArgs, OracleStatusArgs};
use crate::{open_index, print_output};

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
    let lock_repo = rag_rat_core::locks::write_lock_repo_id(config);
    let _lock = rag_rat_core::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
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
                    // Fold the pinned-toolchain fingerprint into the probed `--version` so a
                    // lockfile bump that changes the indexer's output breaks Δ
                    // comparability (#185/#197).
                    tool_version: with_oracle_tool_version_suffix(version),
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

/// Fold the oracle TOOL-ENVIRONMENT fingerprint into a probed `tool_version`. CI exports
/// `RAG_RAT_ORACLE_TOOL_VERSION_SUFFIX` (the pinned npm toolchain lockfile hash) for the npm-based
/// backends, whose `--version` alone is insufficient: scip-typescript's bundled `typescript`
/// compiler can move under a lockfile bump while `scip-typescript --version` stays `0.4.0`. Folding
/// the lockfile hash into `tool_version` makes that bump yield a DIFFERENT `tool_version`, so the
/// Δ-vs-main report (which keys comparability on `tool_version`) refuses to compare rather than
/// mis-attributing the toolchain-output change to rag-rat (#185 / Codex on #197). Unset (local
/// runs, non-npm backends) leaves the version unchanged.
fn with_oracle_tool_version_suffix(version: String) -> String {
    join_tool_version_suffix(version, std::env::var("RAG_RAT_ORACLE_TOOL_VERSION_SUFFIX").ok())
}

fn join_tool_version_suffix(version: String, suffix: Option<String>) -> String {
    match suffix.map(|value| value.trim().to_string()).filter(|value| !value.is_empty()) {
        Some(suffix) => format!("{version}+{suffix}"),
        None => version,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use rag_rat_core::config::{ResolvedTarget, TargetKind};
    use rag_rat_core::language::Language;
    use rag_rat_core::locks::{FileLock, write_lock_path, write_lock_repo_id};
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
            repo_id_override: None,
            database_key_pinned: true,
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
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
        };
        (root, config)
    }

    #[test]
    fn tool_version_suffix_folds_in_the_toolchain_fingerprint() {
        // No suffix (local run / non-npm backend) → version unchanged.
        assert_eq!(super::join_tool_version_suffix("0.4.0".into(), None), "0.4.0");
        assert_eq!(super::join_tool_version_suffix("0.4.0".into(), Some("  ".into())), "0.4.0");
        // A toolchain fingerprint makes a lockfile bump a DIFFERENT tool_version (breaks Δ
        // comparability instead of mis-attributing the change), #185/#197.
        assert_eq!(
            super::join_tool_version_suffix("0.4.0".into(), Some("toolchain:abc123".into())),
            "0.4.0+toolchain:abc123"
        );
        assert_ne!(
            super::join_tool_version_suffix("0.4.0".into(), Some("toolchain:aaa".into())),
            super::join_tool_version_suffix("0.4.0".into(), Some("toolchain:bbb".into())),
        );
    }

    #[test]
    fn checkout_bindings_must_match_corpus() {
        use std::collections::BTreeMap;

        use rag_rat_core::index::oracle::{CorpusHealth, CorpusProfile};

        // A target carrying the SAME default filters the simple `[target_bindings]` form renders
        // (`include = ["**/*.rs"]`, no exclude), so the bindings-match check accepts it.
        let config_with = |include: Vec<String>, exclude: Vec<String>| Config {
            repo_id_override: None,
            database_key_pinned: true,
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
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
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

        // Hold the write lock the run must contend for (per-repo, A6).
        let lock = FileLock::acquire_blocking(&write_lock_path(
            &config.database,
            &write_lock_repo_id(&config),
        ))
        .unwrap();

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
        let lock =
            FileLock::try_acquire(&write_lock_path(&config.database, &write_lock_repo_id(&config)))
                .unwrap();
        assert!(lock.is_some(), "oracle run must release the write lock when it returns");

        let _ = std::fs::remove_dir_all(&root);
    }
}
