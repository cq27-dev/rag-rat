mod cli;
mod commands;
mod fs_atomic;
mod hooks_support;
mod render;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use std::{env, fs};

use clap::Parser;
pub(crate) use commands::*;
pub(crate) use fs_atomic::*;
pub(crate) use hooks_support::*;
use rag_rat_core::config::EmbeddingRuntimeConfig;
use rag_rat_core::index::IndexProgress;
use rag_rat_core::index::github::GitHubSyncAction;
use rag_rat_core::search::lexical::SearchHit;
use rag_rat_core::{Config, IndexDatabase};
pub(crate) use render::*;

use crate::cli::{Cli, Command as Cmd};

mod claude_hook;
mod claude_settings;
mod init;

fn main() -> anyhow::Result<()> {
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
        Cmd::Init(args) => return init::run(args, &cli.config),
        Cmd::ClaudeHook => return claude_hook::run(),
        _ => {},
    }

    let config = load_config_or_hint(&cli.config)?;
    apply_embedding_runtime_env(&config.local_ai.embedding.runtime);

    match cli.command {
        Cmd::Init(_) | Cmd::ClaudeHook => unreachable!("handled before the config load above"),
        Cmd::Index(args) => index(&config, &args)?,
        Cmd::Doctor => doctor(&config)?,
        Cmd::Query(args) => query(&config, &args)?,
        Cmd::Brief(args) => brief(&config, &args)?,
        Cmd::Clusters(args) => clusters(&config, &args)?,
        Cmd::ImportantSymbols(args) => important_symbols(&config, &args)?,
        Cmd::Mcp => {
            // Refresh the crates.io version cache out of band (a detached thread on this long-lived
            // server) when stale + opted-in, so `index_status` and the next SessionStart digest
            // read a fresh result without ever blocking startup or a request. Fail-open
            // (#version-check).
            spawn_detached_version_refresh(&config);
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
        Cmd::Github(args) => github(&config, &args)?,
        Cmd::Hooks(args) => hooks(&config, &args)?,
        Cmd::Maintenance(args) => maintenance(&config, &args)?,
        Cmd::Models(args) => models(&config, &args)?,
        Cmd::Reconcile(args) => reconcile(&config, &args)?,
        Cmd::Gc => {
            let db = open_index(&config)?;
            print_output(&db.gc()?)?;
        },
        Cmd::Eval(args) => eval(&config, &args)?,
        Cmd::Oracle(args) => oracle(&config, &args)?,
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

/// Load the config, mapping a missing file to a friendly hint instead of a raw IO error.
/// `init`/`--help`/`--version` never reach here, so this only guards commands that genuinely
/// need a configured repo.
pub(crate) fn load_config_or_hint(path: &str) -> anyhow::Result<Config> {
    if !Path::new(path).exists() {
        anyhow::bail!(
            "No rag-rat config found at `{path}`.\nRun `rag-rat init` to create one, or pass \
             --config <path>."
        );
    }
    Ok(Config::load(path)?)
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
        let err = load_config_or_hint(missing.to_str().unwrap()).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("rag-rat init"), "expected init hint, got: {message}");
    }
}
