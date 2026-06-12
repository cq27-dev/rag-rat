//! Declarative command-line surface (clap derive). The parser owns `--help`/`-h`,
//! `--version`/`-V`, per-subcommand help, and flag validation — `main.rs` only dispatches on
//! the typed result. The global `--config` defaults to `rag-rat.toml` and may appear before or
//! after the subcommand.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "rag-rat",
    version,
    about = "Local repo-intelligence index, graph, history, and memory — CLI + MCP server.",
    propagate_version = true
)]
pub(crate) struct Cli {
    /// Path to the rag-rat.toml config (relative to the current directory).
    #[arg(long, global = true, default_value = "rag-rat.toml")]
    pub config: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Scan the repository and write a starter rag-rat.toml (interactive).
    Init(InitArgs),

    /// Internal: Claude Code hook entrypoint (reads a JSON event on stdin).
    #[command(hide = true)]
    ClaudeHook,

    /// Index the repository (default: changed files only).
    Index(IndexArgs),

    /// Report schema, storage, discovery, targets, and index health as JSON.
    Doctor,

    /// Apply or check pending schema migrations.
    Migrate(MigrateArgs),

    /// Search the index (lexical + semantic).
    Query(QueryArgs),

    /// Repo orientation brief (spine / churn / god-modules / ownership).
    Brief(BriefArgs),

    /// Ownership / co-change clusters.
    Clusters(ClustersArgs),

    /// Run the stdio MCP server.
    Mcp,

    /// Inspect and re-anchor source-anchored repo memories.
    Memory(MemoryArgs),

    /// GitHub papertrail sync.
    Github(GithubArgs),

    /// Install / uninstall / inspect git hooks and Claude Code hooks.
    Hooks(HooksArgs),

    /// Bounded post-git-operation index maintenance (invoked by hooks).
    Maintenance(MaintenanceArgs),

    /// List or install on-device embedding models.
    Models(ModelsArgs),

    /// Compute or refresh embeddings for indexed chunks.
    Reconcile(ReconcileArgs),

    /// Garbage-collect index rows for dead git contexts.
    Gc,

    /// Run the search-quality eval suite.
    Eval(EvalArgs),

    /// SCIP-oracle pass: compiler-grade edge resolution from a language indexer.
    Oracle(OracleArgs),

    /// Print the resolved configuration as JSON.
    DumpConfig,
}

#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    /// Print the rendered config to stdout without writing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Accept all defaults non-interactively.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Overwrite an existing config without prompting.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub(crate) struct IndexArgs {
    /// Full rebuild from scratch.
    #[arg(long)]
    pub full: bool,
    /// Re-discover all target files (additive), then index changed ones.
    #[arg(long)]
    pub discover: bool,
    /// Index only changed files (the default).
    #[arg(long)]
    pub changed: bool,
    /// Run the background file watcher in the foreground until interrupted.
    #[arg(long)]
    pub watch: bool,
}

#[derive(Debug, Args)]
pub(crate) struct MigrateArgs {
    /// Check migration state without applying (non-zero exit if incompatible).
    #[arg(long)]
    pub check: bool,
}

#[derive(Debug, Args)]
pub(crate) struct QueryArgs {
    /// Show the ranking explanation instead of JSON results.
    #[arg(long)]
    pub explain: bool,
    /// The search string (multiple words are joined).
    #[arg(required = true, num_args = 1.., value_name = "QUERY")]
    pub query: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct BriefArgs {
    /// Brief mode: spine, churn, god_modules, ownership.
    #[arg(long)]
    pub mode: Option<String>,
    /// Max rows to return.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Include generated files.
    #[arg(long)]
    pub include_generated: bool,
    /// Omit drive-by repo memories.
    #[arg(long)]
    pub no_memories: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ClustersArgs {
    /// Max clusters to return.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Minimum cluster size.
    #[arg(long)]
    pub min_cluster_size: Option<u32>,
    /// Include generated files.
    #[arg(long)]
    pub include_generated: bool,
    /// Omit drive-by repo memories.
    #[arg(long)]
    pub no_memories: bool,
}

#[derive(Debug, Args)]
pub(crate) struct MaintenanceArgs {
    /// What triggered this pass (manual, post-checkout, post-merge, ...).
    #[arg(long)]
    pub trigger: Option<String>,
    /// Soft time budget for the reconcile phase, in seconds.
    #[arg(long)]
    pub max_seconds: Option<u64>,
    /// git post-checkout flag: 1 = branch checkout, 0 = file checkout.
    #[arg(long)]
    pub branch_checkout: Option<String>,
    /// git post-checkout: previous HEAD.
    #[arg(long)]
    pub old_head: Option<String>,
    /// git post-checkout: new HEAD.
    #[arg(long)]
    pub new_head: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ReconcileArgs {
    /// Report the reconcile plan without computing embeddings.
    #[arg(long)]
    pub plan: bool,
    /// Emit JSON (implied for --plan when set together).
    #[arg(long)]
    pub json: bool,
    /// Cap on chunks to embed this pass.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Embedding batch size.
    #[arg(long)]
    pub batch_size: Option<u32>,
    /// Recompute even up-to-date embeddings.
    #[arg(long)]
    pub force: bool,
    /// Keep going until no backlog remains.
    #[arg(long)]
    pub until_clean: bool,
    /// Embed changed files first.
    #[arg(long)]
    pub changed_first: bool,
    /// Soft time budget in seconds.
    #[arg(long)]
    pub max_seconds: Option<u64>,
    /// Truncate chunk text to this many chars before embedding.
    #[arg(long)]
    pub max_embedding_chars: Option<usize>,
}

#[derive(Debug, Args)]
pub(crate) struct EvalArgs {
    /// Path to the queries TOML (defaults to <root>/evals/queries.toml).
    #[arg(long)]
    pub queries: Option<PathBuf>,
    /// Path to the expected-hits TOML (defaults to <root>/evals/expected_hits.toml).
    #[arg(long)]
    pub expected: Option<PathBuf>,
    /// Rewrite the baseline from this run's results.
    #[arg(long)]
    pub update_baseline: bool,
    /// Optional pre-built `.scip` index to drive SCIP-oracle precision/recall metrics (#68).
    /// Defaults to <root>/evals/oracle.scip when present; absent → oracle metrics skipped.
    #[arg(long)]
    pub scip: Option<PathBuf>,
    /// Emit JSON instead of the summary.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct OracleArgs {
    #[command(subcommand)]
    pub command: OracleCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum OracleCommand {
    /// Run an oracle pass: invoke the indexer (or consume a pre-built `.scip`) and write verdicts.
    Run(OracleRunArgs),
    /// Report oracle verdict counts + whether the indexer tool is installed.
    Status(OracleStatusArgs),
}

#[derive(Debug, Args)]
pub(crate) struct OracleRunArgs {
    /// The oracle tool to use (default: rust-analyzer).
    #[arg(long, value_enum, default_value_t = OracleToolArg::RustAnalyzer)]
    pub tool: OracleToolArg,
    /// Consume a pre-built `.scip` index instead of invoking the tool. Deterministic; the tool
    /// need not be installed.
    #[arg(long)]
    pub scip: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct OracleStatusArgs {
    /// The oracle tool to report on (default: rust-analyzer).
    #[arg(long, value_enum, default_value_t = OracleToolArg::RustAnalyzer)]
    pub tool: OracleToolArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum OracleToolArg {
    #[value(name = "rust-analyzer")]
    RustAnalyzer,
}

impl OracleToolArg {
    pub(crate) fn core(self) -> rag_rat_core::index::oracle::OracleTool {
        match self {
            OracleToolArg::RustAnalyzer => rag_rat_core::index::oracle::OracleTool::RustAnalyzer,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct MemoryArgs {
    #[command(subcommand)]
    pub command: MemoryCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum MemoryCommand {
    /// List memories (optionally filtered by kind).
    List {
        #[arg(long)]
        kind: Option<String>,
    },
    /// Show one memory by id.
    Show { memory_id: String },
    /// Report non-current anchors with rebind suggestions.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Re-anchor a memory to a symbol, path, or chunk.
    Rebind {
        memory_id: String,
        /// Symbol name (substring-matched); cfg-split groups resolve to one. Ambiguous names list
        /// `--symbol-id` choices — prefer `--symbol-path` for an exact qualified name.
        #[arg(long)]
        symbol: Option<String>,
        /// Exact qualified name (`path::name`) — what `memory doctor` suggests; cfg-split safe.
        #[arg(long)]
        symbol_path: Option<String>,
        /// Exact symbol id — the escape hatch when same-name symbols can't be told apart.
        #[arg(long)]
        symbol_id: Option<i64>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        chunk: Option<i64>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct GithubArgs {
    #[command(subcommand)]
    pub command: GithubCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum GithubCommand {
    /// Sync issues/PRs into the papertrail.
    Sync {
        /// Sync only refs already mentioned in indexed source/commits.
        #[arg(long)]
        from_refs: bool,
        /// Sync a single issue/PR (owner/repo#number).
        #[arg(long)]
        issue: Option<String>,
        /// Do not hit the network; use cached evidence only.
        #[arg(long)]
        offline: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct HooksArgs {
    /// install, uninstall, or status.
    #[arg(value_enum)]
    pub action: HookAction,
    /// Operate on Claude Code hooks (settings.json) instead of git hooks.
    #[arg(long)]
    pub claude: bool,
    /// With --claude: target ~/.claude/settings.json instead of ./.claude.
    #[arg(long)]
    pub global: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum HookAction {
    Install,
    Uninstall,
    Status,
}

impl HookAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            HookAction::Install => "install",
            HookAction::Uninstall => "uninstall",
            HookAction::Status => "status",
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct ModelsArgs {
    #[command(subcommand)]
    pub command: Option<ModelsCommand>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ModelsCommand {
    /// List models and their install state (the default).
    List,
    /// Download and install a model by id.
    Install { model_id: String },
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_global_config_after_subcommand() {
        let cli = Cli::try_parse_from(["rag-rat", "query", "--config", "x.toml", "foo", "bar"])
            .expect("parse");
        assert_eq!(cli.config, "x.toml");
        match cli.command {
            Command::Query(args) => {
                assert_eq!(args.query, vec!["foo", "bar"]);
                assert!(!args.explain);
            },
            other => panic!("expected query, got {other:?}"),
        }
    }

    #[test]
    fn config_defaults_to_rag_rat_toml() {
        let cli = Cli::try_parse_from(["rag-rat", "gc"]).expect("parse");
        assert_eq!(cli.config, "rag-rat.toml");
    }

    #[test]
    fn version_flag_short_circuits() {
        let err = Cli::try_parse_from(["rag-rat", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn help_flag_short_circuits() {
        let err = Cli::try_parse_from(["rag-rat", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn nested_memory_rebind_parses() {
        let cli = Cli::try_parse_from(["rag-rat", "memory", "rebind", "mem_1", "--symbol", "foo"])
            .expect("parse");
        match cli.command {
            Command::Memory(MemoryArgs {
                command: MemoryCommand::Rebind { memory_id, symbol, .. },
            }) => {
                assert_eq!(memory_id, "mem_1");
                assert_eq!(symbol.as_deref(), Some("foo"));
            },
            other => panic!("expected memory rebind, got {other:?}"),
        }
    }

    #[test]
    fn memory_rebind_symbol_id_and_path_parse() {
        let cli =
            Cli::try_parse_from(["rag-rat", "memory", "rebind", "mem_2", "--symbol-id", "42"])
                .expect("parse");
        match cli.command {
            Command::Memory(MemoryArgs {
                command: MemoryCommand::Rebind { symbol_id, symbol_path, symbol, .. },
            }) => {
                assert_eq!(symbol_id, Some(42));
                assert_eq!(symbol_path, None);
                assert_eq!(symbol, None);
            },
            other => panic!("expected memory rebind, got {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "rag-rat",
            "memory",
            "rebind",
            "mem_3",
            "--symbol-path",
            "src/a.rs::foo",
        ])
        .expect("parse");
        match cli.command {
            Command::Memory(MemoryArgs { command: MemoryCommand::Rebind { symbol_path, .. } }) => {
                assert_eq!(symbol_path.as_deref(), Some("src/a.rs::foo"))
            },
            other => panic!("expected memory rebind, got {other:?}"),
        }
    }

    #[test]
    fn hooks_action_and_flags_parse() {
        let cli = Cli::try_parse_from(["rag-rat", "hooks", "install", "--claude", "--global"])
            .expect("parse");
        match cli.command {
            Command::Hooks(args) => {
                assert_eq!(args.action, HookAction::Install);
                assert!(args.claude && args.global);
            },
            other => panic!("expected hooks, got {other:?}"),
        }
    }

    #[test]
    fn oracle_run_defaults_to_rust_analyzer() {
        let cli = Cli::try_parse_from(["rag-rat", "oracle", "run"]).expect("parse");
        match cli.command {
            Command::Oracle(OracleArgs { command: OracleCommand::Run(args) }) => {
                assert_eq!(args.tool, OracleToolArg::RustAnalyzer);
                assert!(args.scip.is_none());
            },
            other => panic!("expected oracle run, got {other:?}"),
        }
    }

    #[test]
    fn oracle_run_accepts_scip_path() {
        let cli = Cli::try_parse_from(["rag-rat", "oracle", "run", "--scip", "/tmp/x.scip"])
            .expect("parse");
        match cli.command {
            Command::Oracle(OracleArgs { command: OracleCommand::Run(args) }) => {
                assert_eq!(args.scip.as_deref(), Some(std::path::Path::new("/tmp/x.scip")));
            },
            other => panic!("expected oracle run, got {other:?}"),
        }
    }

    #[test]
    fn oracle_status_parses() {
        let cli = Cli::try_parse_from(["rag-rat", "oracle", "status"]).expect("parse");
        assert!(matches!(
            cli.command,
            Command::Oracle(OracleArgs { command: OracleCommand::Status(_) })
        ));
    }
}
