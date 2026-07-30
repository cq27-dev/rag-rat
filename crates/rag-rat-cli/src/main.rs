mod cli;
mod commands;
mod fs_atomic;
mod hooks_support;
mod render;
// The version-string formatter is shared with `build.rs` via `include!`; the crate only needs it
// under test (the runtime reads the baked `RAG_RAT_VERSION`), so compiling it here is test-only.
#[cfg(test)]
mod version_describe;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
pub(crate) use commands::*;
pub(crate) use fs_atomic::*;
pub(crate) use hooks_support::*;
use rag_rat_base::config::Config;
use rag_rat_core::IndexDatabase;
use rag_rat_core::index::IndexProgress;
use rag_rat_core::search::lexical::SearchHit;
pub(crate) use render::*;

use crate::cli::{Cli, Command as Cmd, DoctorArgs, ServeArgs};

mod agent_hook;
mod init;

// Idle-RSS fix: glibc malloc never hands freed pages back to the OS, so a long-lived `rag-rat mcp`
// server keeps its peak RSS (~625 MB observed) after the watcher's heavy index/graph passes.
// jemalloc with a background purge thread returns idle dirty/muzzy pages to the OS instead.
// Excluded on msvc (no jemalloc support) and android: the NDK dropped libgcc in r23+, but
// tikv-jemalloc-sys's link line still references `-lgcc`, so the binary fails to link there.
// Android isn't the long-lived-server case anyway — the system allocator is the right fallback.
#[cfg(all(not(target_env = "msvc"), not(target_os = "android")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Tune jemalloc to give idle heap back to the OS: enable its background purge thread (otherwise
/// decayed pages are only reclaimed on later alloc calls, which a quiet server rarely makes) and
/// tighten the decay window from the 10s default so an idle server releases memory within ~1s.
/// Best-effort — a server that holds slightly more RAM is better than one that won't boot.
///
/// Skipped on macOS: jemalloc there rejects `background_thread` ("currently supports pthread only")
/// and the subsequent `MALLCTL_ARENAS_ALL` (`arena.4096.*`) decay writes SEGFAULT the process at
/// startup — crashing every `rag-rat` invocation before the command even runs. macOS keeps
/// jemalloc as the allocator, just untuned (per the same boot-over-RAM priority above).
#[cfg(all(not(target_env = "msvc"), not(target_os = "android"), not(target_os = "macos")))]
fn configure_jemalloc() {
    use tikv_jemalloc_ctl::{background_thread, raw};
    // Purge decayed pages on a background thread; a quiet server makes few alloc calls, which is
    // otherwise the only time jemalloc reclaims.
    let _ = background_thread::write(true);
    // SAFETY: stable mallctl names with ssize_t (isize) values; the writes are best-effort.
    unsafe {
        // `arena.4096.*` is MALLCTL_ARENAS_ALL — retune EVERY arena that already exists (the main
        // thread's arena is created before `main` runs). `arenas.*` alone only sets the default for
        // arenas created LATER, so the already-live arenas keep the 10s default. Measured: with the
        // all-arenas retune a freed ~490 MB heap returns to ~baseline in ~15s idle; without it, it
        // lingers. Set both so existing and future arenas decay fast.
        let _ = raw::write(b"arena.4096.dirty_decay_ms\0", 1000_isize);
        let _ = raw::write(b"arena.4096.muzzy_decay_ms\0", 1000_isize);
        let _ = raw::write(b"arenas.dirty_decay_ms\0", 1000_isize);
        let _ = raw::write(b"arenas.muzzy_decay_ms\0", 1000_isize);
    }
}

fn main() -> anyhow::Result<()> {
    // Record this binary's git-stamped version (#585) so migration provenance and the stranded-
    // binary refusal name a dev build (`0.16.0+g<hash>`) distinctly from a release (`0.16.0`).
    rag_rat_base::version::set_binary_version(env!("RAG_RAT_VERSION"));
    #[cfg(all(not(target_env = "msvc"), not(target_os = "android"), not(target_os = "macos")))]
    configure_jemalloc();
    let cli = Cli::parse();

    // Pin the process-wide output format from the global flag before any command runs, so
    // `print_output` renders TOON by default and JSON under `--json` without threading the format
    // through every command signature.
    set_output_format(if cli.json {
        rag_rat_core::OutputFormat::Json
    } else {
        rag_rat_core::OutputFormat::Toon
    });

    // These commands must tolerate the ABSENCE of a config: `init` creates one; `agent-hook`
    // reads its event from stdin; `mcp` serves a dormant server so a globally-registered MCP stays
    // alive outside a rag-rat repo (#603); `doctor` reports the machine-global store; `rm` targets
    // the global store by path and must work even when the checkout it removes was the only
    // configured one. Everything else needs a resolved config and fails with a friendly hint when
    // there isn't one.
    match &cli.command {
        Cmd::Init(args) => return init::run(args, cli.config.as_deref().unwrap_or("rag-rat.toml")),
        Cmd::AgentHook(args) => return agent_hook::run(args.harness),
        // The detached edit-reindex runner discovers its own config from the hook's cwd (a
        // not-a-rag-rat-repo cwd is a silent no-op), so it tolerates config absence like
        // agent-hook.
        Cmd::EditReindex(args) => return agent_hook::edit_reindex::run(&args.cwd, &args.paths),
        Cmd::Mcp => return run_mcp(cli.config.as_deref(), cli.json),
        Cmd::Doctor(args) => return run_doctor(args, cli.config.as_deref()),
        // `status` is a cross-repo view of the consolidated global store, so — like `doctor` — it
        // tolerates config absence: outside a rag-rat repo it still reports the machine-global
        // store.
        Cmd::Status => return run_status(cli.config.as_deref()),
        // `rm` is a global-store writer keyed by the path argument; it must tolerate the absence of
        // a live config so a deleted/moved last checkout can still be purged.
        Cmd::Rm(args) => return run_rm(args, cli.config.as_deref()),
        _ => {},
    }

    let config = load_config_or_hint(cli.config.as_deref())?;
    apply_embedding_runtime_env(&config.llm.embedding.runtime);

    // Debug logging (off unless `[log] enabled` or `RAG_RAT_LOG`). Writes are blocking (synchronous
    // to the file), so nothing is lost on exit or on the `Cmd::Mcp` hot-upgrade `exec()`.
    // `Cmd::Init` / `Cmd::AgentHook` returned above (no config, and agent-hook fires
    // per-tool-call — logging it would flood the per-process dir and evict the mcp/maintenance
    // signal).
    let _log = rag_rat_base::logging::init_logging(&config, log_role(&cli.command));

    match cli.command {
        Cmd::Init(_)
        | Cmd::AgentHook(_)
        | Cmd::EditReindex(_)
        | Cmd::Mcp
        | Cmd::Doctor(_)
        | Cmd::Status
        | Cmd::Rm(_) => unreachable!("handled before the config load above"),
        Cmd::Index(args) => index(&config, &args)?,
        Cmd::Query(args) => query(&config, &args)?,
        Cmd::Brief(args) => brief(&config, &args)?,
        Cmd::Clusters(args) => clusters(&config, &args)?,
        Cmd::ImportantSymbols(args) => important_symbols(&config, &args)?,
        Cmd::Clones(args) => clones(&config, &args)?,
        Cmd::ClonesFor(args) => clones_for(&config, &args)?,
        Cmd::Serve(args) => serve_http(config, &args)?,
        Cmd::Memory(args) => memory(&config, &args)?,
        Cmd::Sync(args) => sync(&config, &args)?,
        Cmd::Dream(args) => dream(&config, &args)?,
        Cmd::Papertrail(args) => papertrail(&config, &args)?,
        Cmd::Distill(args) => distill(&config, &args)?,
        Cmd::Hooks(args) => hooks(&config, &args)?,
        Cmd::Maintenance(args) => maintenance(&config, &args)?,
        Cmd::Models(args) => models(&config, &args)?,
        Cmd::Reconcile(args) => reconcile(&config, &args)?,
        Cmd::Gc => {
            // gc is a WRITER (it cascade-deletes dead generations and dead contexts), so it takes
            // the per-repo write flock like every other CLI writer. That flock is exactly what
            // makes gc's `generation != live` deadness predicate safe (batch 5):
            // holding it proves no rebuild is mid-flight, so an ABOVE-live staging is
            // abandoned, not in-progress — and every rebuild entry now takes the flock
            // too (batch 6), so a gc racing a mid-flight rebuild serializes with it
            // instead of sweeping the staged generation out from under it.
            let _lock = acquire_cli_write_lock(&config, "gc")?;
            report_pending_schema_upgrade("gc", &config);
            let db = open_index(&config)?;
            print_output(&db.garbage_collect()?)?;
        },
        #[cfg(feature = "eval")]
        Cmd::Eval(args) => eval(&config, &args)?,
        #[cfg(feature = "eval")]
        Cmd::BenchmarkEmbedding(args) => benchmark_embedding(&config, &args)?,
        #[cfg(feature = "eval")]
        Cmd::DumpVerifyPacks(args) => dump_verify_packs(&config, &args)?,
        #[cfg(feature = "eval")]
        Cmd::DumpMemoryInputHashes(args) => dump_memory_input_hashes(&config, &args)?,
        Cmd::Oracle(args) => oracle(&config, &args)?,
        Cmd::Consolidate => {
            let config_path = cli
                .config
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| rag_rat_base::config::discover_config_path(Path::new(".")));
            consolidate(&config, &config_path)?;
        },
        Cmd::DumpConfig => dump_config(&config)?,
        Cmd::VersionCheck => version_check(&config)?,
    }

    Ok(())
}

/// Background thread that keeps the crates.io version cache fresh for the long-lived MCP server:
/// refresh-if-stale now, then poll on a sub-TTL cadence so a release that lands while the server
/// stays up is picked up within `DEFAULT_TTL_MS` (not only at restart) — `index_status` and the
/// SessionStart digest read that cache. Best-effort + non-blocking: the actual crates.io call runs
/// at most once per TTL (gated by `needs_refresh`), fail-open; the thread dies with the process (a
/// hot-upgrade re-exec re-spawns it). No-op when version checking is disabled.
fn spawn_detached_version_refresh(config: &rag_rat_base::config::Config) {
    use rag_rat_core::version_check;
    /// Poll cadence — well under the TTL so the once-per-day network refresh actually fires on a
    /// server that outlives the TTL, while the cache read in between is trivially cheap.
    const POLL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
    if !config.version_check.enabled {
        return;
    }
    let database = config.database.clone();
    std::thread::spawn(move || {
        loop {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if version_check::needs_refresh(
                version_check::read_cache(&database).as_ref(),
                now,
                version_check::DEFAULT_TTL_MS,
            ) {
                let _ = version_check::refresh(&database);
            }
            std::thread::sleep(POLL);
        }
    });
}

/// Background thread that keeps SCIP-grade ranking fresh for the long-lived MCP server without a
/// manual `oracle run`: poll on a sub-quiet-period cadence and, when the active checkout's index is
/// stale AND has been quiet long enough AND the min-interval floor has elapsed, run the oracle for
/// each known tool. Opt-in (`[oracle] auto_run`, default OFF) — returns immediately when disabled,
/// so short-lived CLI/hook commands (which never reach `Cmd::Mcp`) never spawn it.
///
/// Mirrors [`spawn_detached_version_refresh`]: detached, best-effort, fail-open (`let _ = …`), dies
/// with the process (a hot-upgrade re-exec re-spawns it). Each run uses the SAME lock-free
/// production path as `oracle run` — `produce_scip_with_tool` OUTSIDE the write lock, only the
/// pre-spawn snapshot + the join briefly serialized (#82/#83) — so the watcher is never starved
/// through the minutes-long subprocess, and the #82/#83 TOCTOU gates stay armed. A `Blocked`
/// outcome (tool not installed) is a no-op: the loop simply sleeps to the next tick rather than
/// spinning on it.
fn spawn_detached_oracle_auto_run(config: &rag_rat_base::config::Config) {
    use rag_rat_oracle::{AutoRunDecision, AutoRunInputs, OracleTool};
    if !config.oracle.auto_run {
        return;
    }
    // Poll well under the quiet period so a checkout that goes quiet is picked up within roughly
    // one quiet window, while the between-tick cost (a cheap meta + `oracle_runs` read, gated
    // by the pure decision before any subprocess) stays negligible. Floor the cadence so a tiny
    // configured quiet period can't busy-loop.
    let quiet_secs = config.oracle.auto_run_quiet_period_secs;
    let poll = std::time::Duration::from_secs((quiet_secs / 4).max(60));
    let quiet_period_ms = saturating_secs_to_ms(quiet_secs);
    let min_interval_ms = saturating_secs_to_ms(config.oracle.auto_run_min_interval_secs);
    let config = config.clone();
    std::thread::spawn(move || {
        loop {
            // Sleep BEFORE the first decision. This thread is spawned just before `run_stdio`
            // starts the file watcher, so an immediate first tick could run the oracle against the
            // pre-watcher index — missing any unindexed working-tree changes, recording them as
            // skipped/drifted documents, and then letting the min-interval gate block a corrected
            // run for hours. One poll interval lets the watcher's initial maintenance pass index
            // those changes first. (#142 review)
            std::thread::sleep(poll);
            // Each tick re-opens the index so a fresh `(commit_sha, worktree_id)` checkout (the
            // server outlives branch switches) and the latest `indexed_at_ms` are read anew. All
            // fail-open: any error just waits for the next tick.
            let _ = maybe_run_oracle_once(&config, quiet_period_ms, min_interval_ms);
        }
    });

    /// One throttled pass: read the staleness inputs for each tool, ask the pure gate, and on `Run`
    /// take the lock-free production path. Returns `Ok(())` even when nothing ran — the caller only
    /// uses it to swallow errors fail-open.
    fn maybe_run_oracle_once(
        config: &rag_rat_base::config::Config,
        quiet_period_ms: i64,
        min_interval_ms: i64,
    ) -> anyhow::Result<()> {
        let now_ms = now_epoch_ms();
        // `indexed_at_ms` is the active checkout's last index-change clock; without it we can't
        // judge staleness, so skip this tick.
        let last_index_change_ms = {
            let db = open_index(config)?;
            match db.status(&config.database)?.indexed_at_ms {
                Some(ms) => ms,
                None => return Ok(()),
            }
        };
        // The languages this checkout actually indexes. Gating background runs to these (#176)
        // stops the auto-run loop from invoking a backend whose language isn't present —
        // e.g. scip-python installed but no Python target: it would index nothing, fail,
        // the error would be swallowed with no `oracle_runs` row recorded, and the loop
        // would retry the doomed run every poll.
        let configured_languages: std::collections::HashSet<_> =
            config.targets.iter().map(|target| target.language).collect();
        for &tool in OracleTool::ALL {
            // Live-only backends (`ra-lsp`) are driven by the watcher, never by the batch
            // auto-run loop (#534).
            if !tool.batch_capable() {
                continue;
            }
            // Skip a backend whose language this checkout doesn't index — never auto-run it here
            // (the status registry stays broad; only background runs are gated).
            let manifest = rag_rat_oracle::ToolManifest::for_tool(tool);
            if !manifest.languages.iter().any(|lang| configured_languages.contains(lang)) {
                continue;
            }
            // Cheap probe before any decision: an uninstalled tool can never run, so don't even
            // read its run history.
            if matches!(
                rag_rat_oracle::probe_oracle_tool(tool),
                rag_rat_oracle::ToolAvailability::Blocked { .. }
            ) {
                continue;
            }
            let last_run_ms = {
                let db = open_index(config)?;
                db.latest_oracle_run_started_at(tool)?
            };
            let decision = rag_rat_oracle::auto_run_decision(AutoRunInputs {
                enabled: true,
                now_ms,
                last_index_change_ms,
                last_run_ms,
                quiet_period_ms,
                min_interval_ms,
            });
            if decision == AutoRunDecision::Run {
                let _ = run_oracle_tool_background(config, tool);
            }
        }
        Ok(())
    }

    /// The lock-free `oracle run` body for one tool, sans CLI output. Mirrors
    /// `commands::oracle_run`: snapshot the pre-spawn shas under the write lock, produce the
    /// `.scip` OUTSIDE the lock, then run only the join/write under the lock. A `Blocked`
    /// production is a no-op (returns `Ok`).
    fn run_oracle_tool_background(
        config: &rag_rat_base::config::Config,
        tool: OracleTool,
    ) -> anyhow::Result<()> {
        // Stamp the start INSIDE the same write-lock as the pre-spawn snapshot so no watcher
        // reindex can interleave between reading the indexed state and recording the start; under
        // the lock, started_at matches the indexed state this run covers (#145 + #146 review).
        let (started_at_ms, pre_spawn_sha) = with_oracle_write_lock(config, |db| {
            Ok((now_epoch_ms(), db.oracle_pre_spawn_snapshot()?))
        })?;
        let scip_output = config
            .database
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("rag-rat-oracle-auto-{}.scip", std::process::id()));
        let production = rag_rat_oracle::produce_scip_with_tool(tool, &config.root, &scip_output);
        let _ = fs::remove_file(&scip_output);
        match production? {
            rag_rat_oracle::ScipProduction::Blocked { .. } => Ok(()),
            rag_rat_oracle::ScipProduction::Produced { version, bytes, production_sha } => {
                with_oracle_write_lock(config, |db| {
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
                Ok(())
            },
        }
    }

    /// Saturating seconds → ms for the throttle inputs (a wild config value can't overflow `i64`).
    fn saturating_secs_to_ms(secs: u64) -> i64 {
        i64::try_from(secs).unwrap_or(i64::MAX).saturating_mul(1000)
    }
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Load the config, mapping a missing file to a friendly hint instead of a raw IO error.
/// `init`/`--help`/`--version` never reach here, so this only guards commands that genuinely
/// need a configured repo. `mcp`/`doctor` deliberately do NOT — they degrade gracefully via
/// [`discover_config_optional`] instead (dormant server / global-store report).
pub(crate) fn load_config_or_hint(explicit: Option<&str>) -> anyhow::Result<Config> {
    match discover_config_optional(explicit)? {
        Some(config) => Ok(config),
        None => anyhow::bail!(
            "No rag-rat config found at `{}`.\nRun `rag-rat init` to create one, or pass --config \
             <path>.",
            rag_rat_base::config::discover_config_path(Path::new(".")).display()
        ),
    }
}

/// Resolve a config WITHOUT the fail-fast hint, distinguishing "absent" from "broken":
///  * explicit `--config` given → taken literally; a MISSING file is a loud error (a user override
///    that can't be honored), an invalid file propagates its parse error.
///  * no `--config` → discover from cwd via [`rag_rat_base::config::discover_config_path`] (the
///    governing seam: local file, else linked-worktree main, else the ancestor walk). Absent ⇒
///    `Ok(None)` so the caller can degrade gracefully (dormant `mcp`, global-store `doctor`);
///    present-but-invalid ⇒ the parse error — a real repo with a broken config must NOT silently
///    degrade to dormancy.
fn discover_config_optional(explicit: Option<&str>) -> anyhow::Result<Option<Config>> {
    match explicit {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.exists() {
                anyhow::bail!(
                    "No rag-rat config found at `{}`.\nRun `rag-rat init` to create one, or pass \
                     --config <path>.",
                    path.display()
                );
            }
            Ok(Some(Config::load(path)?))
        },
        None => {
            let path = rag_rat_base::config::discover_config_path(Path::new("."));
            // Only a GENUINELY ABSENT path is "no config" (graceful). A path that EXISTS but is not
            // a readable config file (e.g. a directory named `rag-rat.toml`) is
            // present-but-invalid: let `Config::load` surface the error rather than
            // silently going dormant.
            if !path.exists() {
                return Ok(None);
            }
            Ok(Some(Config::load(path)?))
        },
    }
}

/// Serve the stdio MCP server, tolerating the ABSENCE of a config. A globally-registered
/// `rag-rat mcp` is spawned in EVERY project, so outside a rag-rat repo it must serve a DORMANT
/// server (stays alive; every tool call returns a "no index here" notice) instead of exiting and
/// taking repo intelligence down for the whole session (#603). A config that is PRESENT but invalid
/// — or an explicit `--config` path that is missing — is still a loud error.
fn run_mcp(explicit: Option<&str>, json: bool) -> anyhow::Result<()> {
    // FIRST, before any other startup work: this process's environ already advertises it as a
    // hot-upgrade target, and the fleet trigger reads environs. Everything below — config
    // discovery, logging, the Tokio runtime the real handler needs — is time in which an
    // unhandled SIGUSR1 would kill the server outright instead of upgrading it.
    #[cfg(unix)]
    rag_rat_mcp::upgrade::suppress_sigusr1_until_armed();
    let output_format =
        if json { rag_rat_core::OutputFormat::Json } else { rag_rat_core::OutputFormat::Toon };
    let config = discover_config_optional(explicit)?;
    // Repo-specific setup only when a config actually resolved. `_log` holds the tracing guard for
    // the server's lifetime; a dormant server writes no log (there is no repo to anchor it to).
    let _log = config.as_ref().map(|config| {
        apply_embedding_runtime_env(&config.llm.embedding.runtime);
        rag_rat_base::logging::init_logging(config, rag_rat_base::logging::Role::Mcp)
    });
    if let Some(config) = &config {
        // Detached, fail-open, dies with the process — no-ops unless opted in. Never spawned for a
        // dormant server (no repo to refresh or rank).
        spawn_detached_version_refresh(config);
        spawn_detached_oracle_auto_run(config);
    }
    // Small worker pool: the stdio JSON-RPC loop is mostly serial and CPU-heavy indexing is rayon,
    // not tokio; stay multi_thread so a blocking tool handler can't stall the serve/upgrade tasks
    // (issue #63, facet 3).
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    // A resolved config → the active server; none → the dormant one. Kept as two calls so the
    // published `run_stdio(Config, …)` entry point stays source-compatible (#603).
    match config {
        Some(config) => runtime.block_on(rag_rat_mcp::server::run_stdio(config, output_format))?,
        None => runtime.block_on(rag_rat_mcp::server::run_stdio_dormant(output_format))?,
    }
    Ok(())
}

fn serve_http(config: Config, args: &ServeArgs) -> anyhow::Result<()> {
    if !args.bind.is_loopback() && (args.token_env.is_none() || args.allow_origin.is_empty()) {
        anyhow::bail!(
            "non-loopback `rag-rat serve` requires --token-env and at least one --allow-origin"
        );
    }
    let token = match args.token_env.as_deref() {
        Some(name) => std::env::var(name)
            .map_err(|_| anyhow::anyhow!("--token-env variable `{name}` is missing"))?
            .trim()
            .to_string(),
        None => rag_rat_mcp::lens_server::ownership_token()?,
    };
    anyhow::ensure!(!token.is_empty(), "lens bearer token must not be empty");
    // Election BEFORE side effects: a second `rag-rat serve` on the same worktree must fail
    // fast without healing the index or spawning a watcher it never uses.
    let workspace_root = rag_rat_mcp::lens_server::workspace_root(&config);
    let election_lock = rag_rat_base::locks::FileLock::try_acquire(
        &rag_rat_base::locks::lens_server_lock_path_for(&config, &workspace_root),
    )?
    .ok_or_else(|| anyhow::anyhow!("a lens server already owns this worktree"))?;
    drop(IndexDatabase::open_config(&config)?);
    let _watcher = rag_rat_core::watch::Watcher::spawn(config.clone());
    let address = std::net::SocketAddr::new(args.bind, args.port);
    let allowed_origins = args.allow_origin.clone();
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(async move {
        rag_rat_mcp::lens_server::serve_standalone(
            config,
            workspace_root,
            address,
            token,
            allowed_origins,
            election_lock,
            shutdown_signal(),
        )
        .await
    })
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Run `doctor`, tolerating the ABSENCE of a config: with no `rag-rat.toml` at or above the cwd,
/// report the machine-global store (`$XDG_DATA_HOME/rag-rat/rag-rat.sqlite`) instead of erroring —
/// "no local config" is not "no data". `--vacuum` rewrites a specific repo's index, so it still
/// requires a config. Only when this platform resolves no data dir at all does the config-less
/// report fall back to the friendly "run rag-rat init" hint.
fn run_doctor(args: &DoctorArgs, explicit: Option<&str>) -> anyhow::Result<()> {
    if let Some(config) = discover_config_optional(explicit)? {
        let _log = rag_rat_base::logging::init_logging(
            &config,
            rag_rat_base::logging::Role::Cli("doctor".to_string()),
        );
        return doctor(&config, args);
    }
    // No rag-rat.toml at or above the cwd. `--vacuum` needs a repo (it rewrites that repo's index),
    // so it can't run against the config-less global-store report.
    if args.vacuum {
        anyhow::bail!(
            "`rag-rat doctor --vacuum` needs a rag-rat repo (it rewrites that repo's index).\nRun \
             it from the repo checkout, or pass --config <path>."
        );
    }
    // A plain config-less `doctor` reports the machine-global store instead of erroring.
    match rag_rat_base::data_dir::global_database_path() {
        Some(database) => doctor_global_store(&database),
        None => anyhow::bail!(
            "No rag-rat config found at `{}`, and this platform has no data directory for a \
             machine-global store.\nRun `rag-rat init` to create a config, or pass --config \
             <path>.",
            rag_rat_base::config::discover_config_path(Path::new(".")).display()
        ),
    }
}

/// Run `status`, tolerating the ABSENCE of a config. `status` is a cross-repo inventory of the
/// consolidated store, so it targets the STORE, not one repo: prefer the discovered config's
/// `database` (a consolidated repo's index path IS the global store), and fall back to the
/// machine-global store path when there is no `rag-rat.toml` at or above the cwd — "no local
/// config" is not "no data", the same posture as config-less `doctor`. Only when this platform
/// resolves no data dir at all does it fall back to the friendly "run rag-rat init" hint.
fn run_status(explicit: Option<&str>) -> anyhow::Result<()> {
    let database = match discover_config_optional(explicit)? {
        Some(config) => config.database,
        None => match rag_rat_base::data_dir::global_database_path() {
            Some(database) => database,
            None => anyhow::bail!(
                "No rag-rat config found at `{}`, and this platform has no data directory for a \
                 machine-global store.\nRun `rag-rat init` to create a config, or pass --config \
                 <path>.",
                rag_rat_base::config::discover_config_path(Path::new(".")).display()
            ),
        },
    };
    status(&database)
}

/// Run `rm`, tolerating the ABSENCE of a live config: `rm` targets the consolidated global store by
/// the path argument, so a deleted/moved checkout that was the only configured one can still be
/// purged. Config precedence is explicit `--config`, then the cwd's governing config, then the
/// EXISTING target checkout's governing config (so `rm /repos/foo` from `/tmp` honors foo's custom
/// `[index] database`), then the machine-global store. A gone target cannot safely discover upward
/// — it may cross into an unrelated parent checkout — so it goes directly to the global fallback.
/// Only when this platform resolves no data dir at all do we fall back to the friendly "run
/// `rag-rat init`" hint.
fn run_rm(args: &crate::cli::RmArgs, explicit: Option<&str>) -> anyhow::Result<()> {
    let config = match discover_config_optional(explicit)? {
        Some(config) => config,
        None if args.path.is_dir() => match discover_target_config_optional(&args.path)? {
            Some(config) => config,
            None => configless_rm_config()?,
        },
        None => configless_rm_config()?,
    };

    apply_embedding_runtime_env(&config.llm.embedding.runtime);
    let _log = rag_rat_base::logging::init_logging(
        &config,
        rag_rat_base::logging::Role::Cli("rm".to_string()),
    );

    rm(&config, args)
}

fn discover_target_config_optional(target: &Path) -> anyhow::Result<Option<Config>> {
    let config_path = rag_rat_base::config::discover_config_path(target);
    if !config_path.exists() {
        return Ok(None);
    }
    Ok(Some(Config::load(config_path)?))
}

fn configless_rm_config() -> anyhow::Result<Config> {
    let database = rag_rat_base::data_dir::global_database_path().ok_or_else(|| {
        anyhow::anyhow!(
            "No rag-rat config found at `{}`, and this platform has no data directory for a \
             machine-global store.\nRun `rag-rat init` to create a config, or pass --config \
             <path>.",
            rag_rat_base::config::discover_config_path(Path::new(".")).display()
        )
    })?;
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Ok(Config::minimal_for_database(database, root))
}

/// Map the invoked subcommand to a debug-log [`Role`](rag_rat_base::logging::Role) (drives the log
/// file name + startup event). The git hooks invoke `rag-rat maintenance --trigger post-*`, so a
/// maintenance pass with a git-origin trigger is the `hook` role — the reconcile/embedding path we
/// most want to trace. A manual `maintenance` and every other command are `cli:<name>`.
fn log_role(cmd: &Cmd) -> rag_rat_base::logging::Role {
    use rag_rat_base::logging::Role;
    match cmd {
        Cmd::Mcp => Role::Mcp,
        Cmd::Serve(_) => Role::Cli("serve".to_string()),
        Cmd::Maintenance(args) if is_git_hook_trigger(args.trigger.as_deref()) => Role::Hook,
        Cmd::Maintenance(_) => Role::Cli("maintenance".to_string()),
        Cmd::Reconcile(_) => Role::Cli("reconcile".to_string()),
        Cmd::Index(_) => Role::Cli("index".to_string()),
        Cmd::Doctor(_) => Role::Cli("doctor".to_string()),
        Cmd::Gc => Role::Cli("gc".to_string()),
        _ => Role::Cli("cmd".to_string()),
    }
}

/// The git-hook `--trigger` values that mark a maintenance pass as hook-originated (vs a manual
/// run).
fn is_git_hook_trigger(trigger: Option<&str>) -> bool {
    matches!(trigger, Some("post-commit" | "post-checkout" | "post-merge" | "post-rewrite"))
}

/// Open the index for a read command, mapping a not-yet-built index to a friendly hint instead
/// of an empty auto-created SQLite file. Commands that build the index (`index`, `maintenance`)
/// or tolerate a missing schema (`doctor`, `migrate`) deliberately do not go through this.
/// The friendly missing-index guard, shared by [`open_index`] and the commands that reach the
/// database through their own open path (manual papertrail sync goes through the flight lock).
pub(crate) fn ensure_index_exists(config: &Config) -> anyhow::Result<()> {
    if !config.database.exists() {
        anyhow::bail!(
            "No index found at {}.\nRun `rag-rat index` to build it first.",
            config.database.display()
        );
    }
    Ok(())
}

pub(crate) fn open_index(config: &Config) -> anyhow::Result<IndexDatabase> {
    ensure_index_exists(config)?;
    IndexDatabase::open_config(config)
}

/// Tell an interactive CLI user about the potentially long, one-time work that otherwise happens
/// before a command can emit its normal progress. Best-effort: the real open still owns validation
/// and error reporting, so a diagnostic probe must never make an otherwise-valid command fail.
pub(crate) fn report_pending_schema_upgrade(operation: &str, config: &Config) {
    // `migration_check` opens read-write and would create an absent DB. Leave first-time `index`
    // creation to the rebuild path, which owns its empty-index checks and lock ordering.
    if !config.database.exists() {
        return;
    }
    let Ok(status) = IndexDatabase::migration_check(&config.database) else {
        return;
    };
    if status.state == rag_rat_db::schema::SchemaState::Older {
        eprintln!(
            "{operation}: shared index schema upgrade v{} -> v{} required (may wait for another \
             upgrader; large indexes may take several minutes)",
            status.current_version, status.latest_version
        );
    }
}

/// Acquire a CLI writer lock without looking hung when a watcher or another command owns it.
pub(crate) fn acquire_cli_write_lock(
    config: &Config,
    operation: &str,
) -> anyhow::Result<rag_rat_base::locks::WriteLock> {
    let lock_repo = rag_rat_base::locks::write_lock_repo_id(config);
    if let Some(lock) = rag_rat_base::locks::WriteLock::acquire_timeout(
        &config.database,
        &lock_repo,
        std::time::Duration::from_millis(250),
    )? {
        return Ok(lock);
    }
    eprintln!("{operation}: waiting for another rag-rat writer to finish");
    rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)
}

pub(crate) const MANAGED_HOOKS: &[&str] =
    &["post-checkout", "post-merge", "post-rewrite", "post-commit"];
const HOOK_MARKER: &str = "# Generated by rag-rat.";
const DEFAULT_MAINTENANCE_SECONDS: u64 = 30;

#[derive(Debug)]
pub(crate) struct GitPaths {
    worktree_root: PathBuf,
    git_dir: PathBuf,
    git_common_dir: PathBuf,
    pub(crate) hooks_dir: PathBuf,
}

#[cfg(test)]
mod tests {

    use super::{discover_target_config_optional, load_config_or_hint, progress_percent};

    #[test]
    fn progress_percent_is_capped() {
        assert_eq!(progress_percent(0, 0), 100);
        assert_eq!(progress_percent(50, 100), 50);
        assert_eq!(progress_percent(17_024, 11_998), 100);
    }

    #[test]
    fn missing_config_yields_friendly_init_hint() {
        let scratch = rag_rat_base::test_scratch::ScratchDir::new("no-config");
        let missing = scratch.join("missing.toml");
        let err = load_config_or_hint(Some(missing.to_str().unwrap())).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("rag-rat init"), "expected init hint, got: {message}");
    }

    #[test]
    fn rm_target_config_discovery_honors_its_custom_database() {
        let root = rag_rat_base::test_scratch::ScratchDir::new("rm-target-config");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \"custom/index.sqlite\"\nrepo_id = \
             \"target-config-test\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();

        let config = discover_target_config_optional(&root)
            .unwrap()
            .expect("the existing target checkout's governing config should load");
        // Config::load resolves the relative `database` against the CANONICALIZED config dir
        // (normalize_existing_dir → paths::canonicalize): /var→/private/var on macOS, 8.3 expansion
        // on Windows. Canonicalize the base and join components so the expectation matches
        // natively.
        let root = rag_rat_base::paths::canonicalize(root).unwrap();
        assert_eq!(config.database, root.join("custom").join("index.sqlite"));
    }
}
