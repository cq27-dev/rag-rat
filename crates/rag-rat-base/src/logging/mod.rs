//! Process-level debug logging. Off by default; enabled via `[log]` or `RAG_RAT_LOG`.
//!
//! Each process writes its OWN file (`<role>-<pid>-<start_ms>.log`) so concurrent processes (an MCP
//! server, git-hook maintenance passes, sibling worktrees) never contend on one file. Correlation
//! is the filename plus a one-shot startup event: the subscriber is process-global, so events from
//! worker threads (e.g. the embedder's scoped threads) land in the same file — we do NOT rely on
//! span context propagating across threads/tasks. Growth is bounded by [`retention`] at init (age,
//! count, size); size-rolling a live file within one long session is a deferred extension.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{Config, LogFormat};

mod retention;

/// Which kind of process is logging — drives the file name and (for merged-timeline reads) the
/// `role` field on the startup event.
#[derive(Debug, Clone)]
pub enum Role {
    Mcp,
    Hook,
    Cli(String),
}

impl Role {
    /// Human-facing role tag (`mcp` | `hook` | `cli:<subcommand>`).
    pub fn as_str(&self) -> String {
        match self {
            Role::Mcp => "mcp".to_string(),
            Role::Hook => "hook".to_string(),
            Role::Cli(sub) => format!("cli:{sub}"),
        }
    }

    /// Filesystem-safe file-name stem (no `:` — `cli-<subcommand>`).
    fn file_stem(&self) -> String {
        match self {
            Role::Mcp => "mcp".to_string(),
            Role::Hook => "hook".to_string(),
            Role::Cli(sub) => format!("cli-{sub}"),
        }
    }
}

/// Returned by [`init_logging`] and held by the caller (`main`). Writes are BLOCKING (synchronous
/// to the file), so there is no buffered tail to flush — nothing is lost on a normal exit, a
/// short-lived hook/CLI process, or the MCP hot-upgrade `exec()` (which never runs destructors).
/// This handle is just a marker today; keeping it lets the writer strategy change without touching
/// call sites.
pub struct LogHandle {
    _private: (),
}

/// A global `tracing` subscriber may be installed only once per process.
static INIT: OnceLock<()> = OnceLock::new();

/// Install the process-global debug-log subscriber. No-op (returns an inert handle) when logging is
/// disabled, already initialized, or init fails — logging never affects correctness.
pub fn init_logging(config: &Config, role: Role) -> LogHandle {
    let env = std::env::var("RAG_RAT_LOG").ok().filter(|s| !s.trim().is_empty());
    if !config.log.enabled && env.is_none() {
        return LogHandle { _private: () };
    }
    if INIT.set(()).is_err() {
        return LogHandle { _private: () };
    }
    match try_init(config, &role, env) {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("rag-rat: debug logging disabled (init failed: {err})");
            LogHandle { _private: () }
        },
    }
}

fn try_init(config: &Config, role: &Role, env: Option<String>) -> anyhow::Result<LogHandle> {
    let log = &config.log;
    std::fs::create_dir_all(&log.dir)?;
    retention::sweep_retention(&log.dir, log.retention_days, log.max_files, log.max_file_bytes);

    // Filter precedence: RAG_RAT_LOG (if set) overrides the config level+filter; an invalid value
    // falls back to the config level rather than aborting.
    let directives = env.unwrap_or_else(|| match &log.filter {
        Some(filter) => format!("{},{}", log.level.as_filter_str(), filter),
        None => log.level.as_filter_str().to_string(),
    });
    let filter = EnvFilter::try_new(&directives)
        .unwrap_or_else(|_| EnvFilter::new(log.level.as_filter_str()));

    let pid = std::process::id();
    let start_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let file_name = format!("{}-{}-{}.log", role.file_stem(), pid, start_ms);

    // One file per process (no date suffix). BLOCKING writes (the appender is its own `MakeWriter`,
    // no `non_blocking` worker): every record is synchronously on disk, so nothing is buffered to
    // lose on a normal exit, a short-lived process, or the MCP hot-upgrade `exec()` (which never
    // runs destructors). Debug logging is off by default, so the per-event write cost is only
    // paid when a user has deliberately turned it on to diagnose.
    let appender = tracing_appender::rolling::never(&log.dir, &file_name);

    let registry = tracing_subscriber::registry().with(filter);
    let fmt_layer = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(appender);
    match log.format {
        LogFormat::Json => registry.with(fmt_layer.json()).try_init()?,
        LogFormat::Text => registry.with(fmt_layer).try_init()?,
    }

    // The startup event carries the full correlation metadata once (the filename carries role+pid).
    tracing::info!(
        target: "rag_rat_core::logging",
        role = %role.as_str(),
        pid,
        root = %config.root.display(),
        version = env!("CARGO_PKG_VERSION"),
        "rag-rat logging started"
    );
    Ok(LogHandle { _private: () })
}

#[cfg(test)]
mod tests {
    use super::{Role, init_logging};
    use crate::config::{Config, LogConfig};

    fn test_config(dir: &std::path::Path, enabled: bool) -> Config {
        Config {
            trackers: Vec::new(),
            papertrail: Default::default(),
            sync: Default::default(),
            repo_id_override: None,
            database_key_pinned: true,
            root: dir.to_path_buf(),
            database: dir.join(".rag-rat/index.sqlite"),
            targets: Vec::new(),
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            memory: Default::default(),
            log: LogConfig { enabled, dir: dir.join("logs"), ..LogConfig::default() },
            source_root_reanchored_from: None,
            allow_empty: false,
        }
    }

    #[test]
    fn role_as_str() {
        assert_eq!(Role::Mcp.as_str(), "mcp");
        assert_eq!(Role::Hook.as_str(), "hook");
        assert_eq!(Role::Cli("reconcile".into()).as_str(), "cli:reconcile");
    }

    #[test]
    fn disabled_config_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path(), false);
        let _handle = init_logging(&config, Role::Cli("gc".into()));
        let empty = !config.log.dir.exists()
            || std::fs::read_dir(&config.log.dir).unwrap().next().is_none();
        assert!(empty, "disabled logging must not create any file");
    }

    // NOTE: `init_logging` installs a PROCESS-GLOBAL subscriber guarded by `INIT` (a `OnceLock`),
    // so there can be only ONE enabled-logging test per test binary — a second would get a
    // guard-less no-op handle and see no file. Keep this the sole enabled case in this module.
    #[test]
    fn enabled_config_writes_a_per_process_file_with_startup_event() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path(), true);
        {
            let _handle = init_logging(&config, Role::Hook);
            tracing::info!(target: "rag_rat_core::probe", "hello");
        } // blocking writer: both records are already on disk
        let files: Vec<_> =
            std::fs::read_dir(&config.log.dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(files.len(), 1, "exactly one per-process file");
        let name = files[0].file_name().into_string().unwrap();
        assert!(name.starts_with("hook-"), "role-prefixed file name, got {name}");
        let body = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(body.contains("rag-rat logging started"), "startup event present");
        assert!(body.contains("hello"), "subsequent event present");
    }
}
