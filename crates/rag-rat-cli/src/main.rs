mod cli;
mod commands;
mod fs_atomic;
mod hooks_support;
mod render;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
pub(crate) use commands::*;
pub(crate) use fs_atomic::*;
pub(crate) use hooks_support::*;
use rag_rat_core::index::IndexProgress;
use rag_rat_core::index::github::GitHubSyncAction;
use rag_rat_core::search::lexical::SearchHit;
use rag_rat_core::{Config, IndexDatabase};
pub(crate) use render::*;

use crate::cli::{Cli, Command as Cmd};

mod agent_hook;
mod claude_settings;
mod init;

// Idle-RSS fix: glibc malloc never hands freed pages back to the OS, so a long-lived `rag-rat mcp`
// server keeps its peak RSS (~625 MB observed) after the watcher's heavy index/graph passes.
// jemalloc with a background purge thread returns idle dirty/muzzy pages to the OS instead.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Tune jemalloc to give idle heap back to the OS: enable its background purge thread (otherwise
/// decayed pages are only reclaimed on later alloc calls, which a quiet server rarely makes) and
/// tighten the decay window from the 10s default so an idle server releases memory within ~1s.
/// Best-effort — a server that holds slightly more RAM is better than one that won't boot.
#[cfg(not(target_env = "msvc"))]
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
    #[cfg(not(target_env = "msvc"))]
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

    // These commands must work without a config file present — `init` creates one, and the
    // Claude Code hook entrypoint reads its event from stdin. Everything else needs a config.
    match &cli.command {
        Cmd::Init(args) => return init::run(args, cli.config.as_deref().unwrap_or("rag-rat.toml")),
        Cmd::AgentHook => return agent_hook::run(),
        _ => {},
    }

    let config = load_config_or_hint(cli.config.as_deref())?;
    apply_embedding_runtime_env(&config.llm.embedding.runtime);

    // Debug logging (off unless `[log] enabled` or `RAG_RAT_LOG`). Writes are blocking (synchronous
    // to the file), so nothing is lost on exit or on the `Cmd::Mcp` hot-upgrade `exec()`.
    // `Cmd::Init` / `Cmd::AgentHook` returned above (no config, and agent-hook fires
    // per-tool-call — logging it would flood the per-process dir and evict the mcp/maintenance
    // signal).
    let _log = rag_rat_core::logging::init_logging(&config, log_role(&cli.command));

    match cli.command {
        Cmd::Init(_) | Cmd::AgentHook => unreachable!("handled before the config load above"),
        Cmd::Index(args) => index(&config, &args)?,
        Cmd::Doctor => doctor(&config)?,
        Cmd::Query(args) => query(&config, &args)?,
        Cmd::Brief(args) => brief(&config, &args)?,
        Cmd::Clusters(args) => clusters(&config, &args)?,
        Cmd::ImportantSymbols(args) => important_symbols(&config, &args)?,
        Cmd::Clones(args) => clones(&config, &args)?,
        Cmd::ClonesFor(args) => clones_for(&config, &args)?,
        Cmd::Mcp => {
            // Refresh the crates.io version cache out of band (a detached thread on this long-lived
            // server) when stale + opted-in, so `index_status` and the next SessionStart digest
            // read a fresh result without ever blocking startup or a request. Fail-open
            // (#version-check).
            spawn_detached_version_refresh(&config);
            // Keep SCIP-grade ranking self-maintaining: a heavily-throttled detached thread runs
            // the oracle when the active checkout's index is stale AND quiet (opt-in
            // via `[oracle] auto_run`, default OFF). Mirrors the version-refresh thread
            // — fail-open, dies with the process. No-op unless enabled.
            spawn_detached_oracle_auto_run(&config);
            // The MCP server is an stdio JSON-RPC loop (one client, mostly serial) plus a SIGUSR1
            // task; the file watcher runs on its own OS thread and CPU-heavy indexing is rayon, not
            // tokio. The default runtime's ~num_cpus workers are therefore idle overhead, so cap it
            // small (issue #63, facet 3). Stay multi_thread (not current_thread) so a blocking tool
            // handler can't stall the serve loop or the upgrade-signal task.
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?
                .block_on(rag_rat_mcp::server::run_stdio(
                    config,
                    if cli.json {
                        rag_rat_core::OutputFormat::Json
                    } else {
                        rag_rat_core::OutputFormat::Toon
                    },
                ))?;
        },
        Cmd::Memory(args) => memory(&config, &args)?,
        Cmd::Dream(args) => dream(&config, &args)?,
        Cmd::Github(args) => github(&config, &args)?,
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
            let lock_repo = rag_rat_core::locks::write_lock_repo_id(&config);
            let _lock =
                rag_rat_core::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
            let db = open_index(&config)?;
            print_output(&db.garbage_collect()?)?;
        },
        #[cfg(feature = "eval")]
        Cmd::Eval(args) => eval(&config, &args)?,
        #[cfg(feature = "eval")]
        Cmd::BenchmarkEmbedding(args) => benchmark_embedding(&config, &args)?,
        Cmd::Oracle(args) => oracle(&config, &args)?,
        Cmd::Consolidate => consolidate(&config)?,
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
fn spawn_detached_version_refresh(config: &rag_rat_core::Config) {
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
fn spawn_detached_oracle_auto_run(config: &rag_rat_core::Config) {
    use rag_rat_core::index::oracle::{self, AutoRunDecision, AutoRunInputs, OracleTool};
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
        config: &rag_rat_core::Config,
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
        let configured_languages: std::collections::HashSet<&str> =
            config.targets.iter().map(|target| target.language.as_str()).collect();
        for &tool in OracleTool::ALL {
            // Skip a backend whose language this checkout doesn't index — never auto-run it here
            // (the status registry stays broad; only background runs are gated).
            let manifest = oracle::ToolManifest::for_tool(tool);
            if !manifest.languages.iter().any(|lang| configured_languages.contains(lang)) {
                continue;
            }
            // Cheap probe before any decision: an uninstalled tool can never run, so don't even
            // read its run history.
            if matches!(oracle::probe_oracle_tool(tool), oracle::ToolAvailability::Blocked { .. }) {
                continue;
            }
            let last_run_ms = {
                let db = open_index(config)?;
                db.latest_oracle_run_started_at(tool)?
            };
            let decision = oracle::auto_run_decision(AutoRunInputs {
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
        config: &rag_rat_core::Config,
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
        let production = oracle::produce_scip_with_tool(tool, &config.root, &scip_output);
        let _ = fs::remove_file(&scip_output);
        match production? {
            oracle::ScipProduction::Blocked { .. } => Ok(()),
            oracle::ScipProduction::Produced { version, bytes, production_sha } => {
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
/// need a configured repo.
///
/// `explicit` is the user's `--config` value, taken literally; `None` resolves through
/// [`rag_rat_core::config::discover_config_path`] — the discovery side of the governing seam, so
/// a linked worktree with no branch-local `rag-rat.toml` (the state `init`'s refusal leaves)
/// finds the MAIN worktree's config instead of dying at the existence check, and the hint in a
/// truly config-less repo names the path where the config BELONGS (main's, in a linked checkout).
pub(crate) fn load_config_or_hint(explicit: Option<&str>) -> anyhow::Result<Config> {
    let path = match explicit {
        Some(path) => std::path::PathBuf::from(path),
        None => rag_rat_core::config::discover_config_path(Path::new(".")),
    };
    if !path.exists() {
        anyhow::bail!(
            "No rag-rat config found at `{}`.\nRun `rag-rat init` to create one, or pass --config \
             <path>.",
            path.display()
        );
    }
    Ok(Config::load(path)?)
}

/// Map the invoked subcommand to a debug-log [`Role`](rag_rat_core::logging::Role) (drives the log
/// file name + startup event). The git hooks invoke `rag-rat maintenance --trigger post-*`, so a
/// maintenance pass with a git-origin trigger is the `hook` role — the reconcile/embedding path we
/// most want to trace. A manual `maintenance` and every other command are `cli:<name>`.
fn log_role(cmd: &Cmd) -> rag_rat_core::logging::Role {
    use rag_rat_core::logging::Role;
    match cmd {
        Cmd::Mcp => Role::Mcp,
        Cmd::Maintenance(args) if is_git_hook_trigger(args.trigger.as_deref()) => Role::Hook,
        Cmd::Maintenance(_) => Role::Cli("maintenance".to_string()),
        Cmd::Reconcile(_) => Role::Cli("reconcile".to_string()),
        Cmd::Index(_) => Role::Cli("index".to_string()),
        Cmd::Doctor => Role::Cli("doctor".to_string()),
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
pub(crate) fn open_index(config: &Config) -> anyhow::Result<IndexDatabase> {
    if !config.database.exists() {
        anyhow::bail!(
            "No index found at {}.\nRun `rag-rat index` to build it first.",
            config.database.display()
        );
    }
    IndexDatabase::open_config(config)
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
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{load_config_or_hint, progress_percent};

    static TMP: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn progress_percent_is_capped() {
        assert_eq!(progress_percent(0, 0), 100);
        assert_eq!(progress_percent(50, 100), 50);
        assert_eq!(progress_percent(17_024, 11_998), 100);
    }

    #[test]
    fn missing_config_yields_friendly_init_hint() {
        let n = TMP.fetch_add(1, Ordering::Relaxed);
        let missing =
            std::env::temp_dir().join(format!("rag-rat-no-config-{}-{n}.toml", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        let err = load_config_or_hint(Some(missing.to_str().unwrap())).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("rag-rat init"), "expected init hint, got: {message}");
    }
}
