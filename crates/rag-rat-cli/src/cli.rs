//! Declarative command-line surface (clap derive). The parser owns `--help`/`-h`,
//! `--version`/`-V`, per-subcommand help, and flag validation — `main.rs` only dispatches on
//! the typed result. The global `--config` defaults to governing-path discovery and may appear
//! before or after the subcommand.

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "rag-rat",
    version = env!("RAG_RAT_VERSION"),
    about = "Local repo-intelligence index, graph, history, and memory — CLI + MCP server.",
    propagate_version = true
)]
pub(crate) struct Cli {
    /// Path to the rag-rat.toml config (relative to the current directory). When omitted, the
    /// config is DISCOVERED through the checkout's governing path: `rag-rat.toml` here, or — in a
    /// linked git worktree with no local file — the main worktree's `rag-rat.toml`. An explicit
    /// path is taken literally (no redirect).
    #[arg(long, global = true)]
    pub config: Option<String>,

    /// Emit JSON instead of the default TOON (Token-Oriented Object Notation). TOON is denser for
    /// LLM consumers; pass --json when a JSON parser must read the output. For commands that print
    /// a human summary by default (`reconcile --plan`, `eval`, `memory doctor`), --json also
    /// selects their structured output.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Scan the repository and write a starter rag-rat.toml (interactive).
    Init(InitArgs),

    /// Internal: coding-agent hook entrypoint (reads a JSON hook event on stdin).
    #[command(hide = true)]
    AgentHook(AgentHookArgs),

    /// Internal: the detached edit-driven reindex runner (#661), spawned by the PostToolUse edit
    /// hook. Coalesces a burst of edits via the single-flight and runs one scoped structural
    /// reindex of the supplied paths. Not for direct use — the hook backgrounds it.
    #[command(hide = true)]
    EditReindex(EditReindexArgs),

    /// Index the repository (default: changed files only).
    Index(IndexArgs),

    /// Report schema, storage, discovery, targets, and index health as JSON.
    Doctor(DoctorArgs),

    /// Cross-repo inventory of the consolidated global store: every registered repo with its index
    /// freshness, worktree overlays, memory / papertrail / content counts, and the whole-file
    /// health rollup. Read-only; complements `doctor` (one repo, deep). Pass the global `--json`
    /// for the structured form.
    Status,

    /// Search the index (lexical + semantic).
    Query(QueryArgs),

    /// Repo orientation brief (spine / churn / god-modules / ownership).
    Brief(BriefArgs),

    /// Ownership / co-change clusters.
    Clusters(ClustersArgs),

    /// Rank the most load-bearing symbols by weighted PageRank over the edge graph.
    ImportantSymbols(ImportantSymbolsArgs),

    /// List candidate clone classes ranked by refactor ROI.
    Clones(ClonesArgs),

    /// Reverse-lookup: show the clone class containing a given symbol (if any).
    ClonesFor(ClonesForArgs),

    /// Run the stdio MCP server.
    Mcp,

    /// Serve the authenticated editor Lens HTTP API.
    Serve(ServeArgs),

    /// Inspect and re-anchor source-anchored repo memories.
    Memory(MemoryArgs),

    /// Configure local memory-stream authoring. This does not configure transport or peers.
    Sync(SyncArgs),

    /// Dream-mode memory-maintenance worklist (#122): deterministic coverage-gap + stale-reference
    /// findings written to `dream_findings`. Surfaces findings ABOUT memories; never mutates them.
    Dream(DreamArgs),

    /// Tracker papertrail sync.
    Papertrail(PapertrailArgs),

    /// Deterministic distillation of tracker threads into records (#703). Model-free: builds the
    /// skeleton records + fixing-commit / coalesce / anchor substrate and the work queue; the LLM
    /// pass that fills root-cause/decision/outcome runs separately.
    Distill(DistillArgs),

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

    /// Run the search-quality eval suite (CI gate; requires the `eval` build feature).
    #[cfg(feature = "eval")]
    Eval(EvalArgs),

    /// Benchmark ephemeral remote embedding throughput across concurrency candidates, emitting
    /// per-candidate texts/s as JSON (requires the `eval` build feature). Provisions an ephemeral
    /// cookbook box, runs the sweep, and tears it down.
    #[cfg(feature = "eval")]
    BenchmarkEmbedding(BenchmarkEmbeddingArgs),

    /// Regenerate the memory-compaction eval's `verify-packs` corpus (#695): insert the eval
    /// memories from a spec into the configured (throwaway) index and dump each memory's
    /// `render_pack(evidence_pack(...))`. Requires the `eval` build feature.
    #[cfg(feature = "eval")]
    #[command(hide = true)]
    DumpVerifyPacks(DumpVerifyPacksArgs),

    /// Recompute production verifier-input hashes for existing memories. Eval fixture helper.
    #[cfg(feature = "eval")]
    #[command(hide = true)]
    DumpMemoryInputHashes(DumpMemoryInputHashesArgs),

    /// SCIP-oracle pass: compiler-grade edge resolution from a language indexer.
    Oracle(OracleArgs),

    /// Import this repo's legacy per-repo index into the consolidated global database, then rename
    /// the legacy `.rag-rat/index.sqlite` so the repo uses the global store from then on.
    Consolidate,

    /// Remove a repo from the consolidated global index: purge all of its rows, then delete its
    /// `rag-rat.toml` and uninstall its git hooks. Returning the repo needs `rag-rat init`.
    Rm(RmArgs),

    /// Print the resolved configuration as JSON.
    DumpConfig,

    /// Check crates.io for a newer published rag-rat, refresh the cache, and print current vs
    /// latest.
    VersionCheck,
}

#[derive(Debug, Args)]
pub(crate) struct RmArgs {
    /// The repo to remove, by path: `.`, a relative path, or an absolute path. Resolved to a
    /// registered repo by identity → recorded root → sole; an exact recorded-root match also
    /// resolves a repo whose working tree was deleted or moved.
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,
    /// Skip the confirmation prompt (non-interactive).
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Preview what would be deleted (per-table row counts) and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub(crate) struct AgentHookArgs {
    /// Input/output contract to normalize. Auto preserves the shared Claude/Codex plugin
    /// entrypoint and detects Cursor's lower-camel lifecycle names.
    #[arg(value_enum, default_value_t = AgentHookHarnessArg::Auto)]
    pub harness: AgentHookHarnessArg,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum AgentHookHarnessArg {
    #[default]
    Auto,
    Cursor,
    Vscode,
}

#[derive(Debug, Args)]
pub(crate) struct ServeArgs {
    /// Address to bind. Non-loopback interfaces require --token-env and --allow-origin.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: IpAddr,
    /// TCP port to bind. Use 0 to let the operating system choose an available port.
    #[arg(long, default_value_t = 18120)]
    pub port: u16,
    /// Environment variable containing the bearer token. Loopback serving generates a token when
    /// omitted; non-loopback serving requires an explicit token variable.
    #[arg(long, value_name = "ENV_VAR")]
    pub token_env: Option<String>,
    /// Exact browser Origin allowed to call the API (`scheme://host[:port]`, no path). Repeat
    /// for multiple trusted origins.
    #[arg(long, value_name = "ORIGIN", value_parser = parse_lens_origin)]
    pub allow_origin: Vec<String>,
}

/// clap `value_parser` for `--allow-origin`: an Origin header is `scheme://host[:port]` with no
/// path, query, or fragment. A trailing slash is normalized away; anything else that would
/// silently never match the header is rejected at parse time.
fn parse_lens_origin(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let parsed = url::Url::parse(trimmed)
        .map_err(|_| format!("origin `{raw}` must look like scheme://host[:port]"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("origin scheme must be http or https: `{raw}`"));
    }
    if parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(format!(
            "origin must not contain credentials, a path, query, or fragment: `{raw}`"
        ));
    }
    Ok(parsed.origin().ascii_serialization())
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
    /// Skip installing the git maintenance hooks. `--yes` installs them by default; pass this to
    /// leave `.git/hooks` untouched (e.g. an agent that asks for hook consent separately).
    #[arg(long)]
    pub no_hooks: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SyncArgs {
    #[command(subcommand)]
    pub command: SyncCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SyncCommand {
    /// Permanently enable sealed authoring for subsequent local memory changes.
    Enable,
    /// Re-wrap existing live keys to an effective device without rotating them.
    #[command(long_about = "The target device must already be enrolled and effective. Re-wraps \
                            existing live keys without rotating them; this does not enroll the \
                            target or perform transport.")]
    CatchUp {
        /// Exact 64-hex-character fingerprint of a device that is already enrolled and effective.
        #[arg(value_name = "DEVICE_FINGERPRINT")]
        target: rag_rat_oplog::DeviceFingerprint,
    },
    /// Run a headless store-and-forward peer: bind the sync endpoint over the configured relay and
    /// exchange this account's op-log with authorized peers.
    #[command(long_about = "Binds an iroh endpoint pinned to the configured relay ([sync] \
                            relay_url, default relay.cq27.dev; RAG_RAT_SYNC_RELAY overrides), \
                            then accepts connections and replicates this account's op log with \
                            peers authorized against the account roster. Runs until interrupted; \
                            --once serves a single connection and exits.")]
    Serve {
        /// Serve exactly one connection, then exit (useful for scripted checks).
        #[arg(long)]
        once: bool,
    },
    /// Mint a one-time enrollment invite on THIS (owner) device and host the pairing exchange.
    #[command(long_about = "Owner-side pairing. Mints a one-time, role-fixed invite ticket, \
                            prints it, then binds the sync endpoint and hosts enrollment plus \
                            the joiner's follow-up account + content restore until interrupted \
                            (Ctrl-C). Share the printed ticket with the joining device, which \
                            redeems it with `rag-rat sync join <ticket>`. Must run on the \
                            account's FOUNDER device (the one that ran `rag-rat sync enable`); \
                            hosting enrollment from a granted owner is not yet supported.")]
    Init {
        /// Role granted to the joining device, fixed at mint time (default: least-privilege
        /// read-only). `read-only` reads/syncs; `member` also authors content; `owner` also holds
        /// full control authority (granting an owner does not yet let it host enrollment — only
        /// the founder can run `sync init`).
        #[arg(long, value_enum, default_value_t = InviteRole::ReadOnly)]
        role: InviteRole,
        /// Optional human label recorded for the joining device (e.g. "laptop").
        #[arg(long, value_name = "NAME")]
        label: Option<String>,
        /// Invite lifetime in seconds before it expires (default 900 = 15 minutes).
        #[arg(long, value_name = "SECS", default_value_t = 900)]
        ttl_secs: u64,
    },
    /// Enroll THIS device into an account with an invite ticket, then restore its state.
    #[command(long_about = "Joiner-side pairing. Decodes the invite ticket printed by `rag-rat \
                            sync init` on an owner device, dials that owner, enrolls this device \
                            into the account roster, then restores the account log and its \
                            content. The inviting owner must be online (running `rag-rat sync \
                            init` or `rag-rat sync serve`).")]
    Join {
        /// The invite ticket string printed by `rag-rat sync init` on the owner device.
        #[arg(value_name = "TICKET")]
        ticket: String,
    },
}

/// Roster role an operator grants a joining device at `sync init` time. Mirrors
/// [`rag_rat_oplog::DeviceRole`] with CLI-friendly token names; fixed into the invite at mint time
/// so a joiner cannot escalate.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum InviteRole {
    /// Reads and syncs only — the default, least-privilege role.
    #[value(name = "read-only")]
    ReadOnly,
    /// Reads, syncs, and authors content.
    Member,
    /// Reads, syncs, authors content, and holds full control authority. (Hosting enrollment via
    /// `sync init` is currently limited to the account's founder device.)
    Owner,
}

impl InviteRole {
    pub(crate) fn to_device_role(self) -> rag_rat_oplog::DeviceRole {
        match self {
            InviteRole::ReadOnly => rag_rat_oplog::DeviceRole::ReadOnly,
            InviteRole::Member => rag_rat_oplog::DeviceRole::Member,
            InviteRole::Owner => rag_rat_oplog::DeviceRole::Owner,
        }
    }
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
    /// Index a LINKED git worktree's branch overlay on top of the existing base index, so queries
    /// scoped to it (`--worktree` / the MCP `worktree` arg) see that branch's changes. Indexes
    /// only the delta vs the base; does not rebuild the base.
    #[arg(long, value_name = "PATH")]
    pub worktree: Option<std::path::PathBuf>,
    /// Reconcile exactly these paths (the edit-driven-reindex substrate): index the supplied files
    /// (content-hash decides staleness), tombstone any that no longer exist, and leave everything
    /// else untouched. Ignored / out-of-target / unchanged paths are no-ops; an edit under a
    /// linked worktree is routed to that worktree's overlay. Unlike the default changed-file
    /// mode this also sees committed changes, since the caller — not git status — names the
    /// paths.
    #[arg(
        long,
        value_name = "PATH",
        num_args = 1..,
        conflicts_with_all = ["full", "discover", "changed", "worktree", "watch"]
    )]
    pub paths: Vec<std::path::PathBuf>,
    /// Run the background file watcher in the foreground until interrupted.
    #[arg(long)]
    pub watch: bool,
    /// Allow `index` to complete when zero files are discovered (e.g. no `[target_bindings]`),
    /// registering the repo with empty content instead of erroring. Default: refuse (#427).
    #[arg(long)]
    pub allow_empty: bool,
}

#[derive(Debug, Args)]
pub(crate) struct EditReindexArgs {
    /// The edited absolute path(s) to reconcile (the PostToolUse hook supplies these).
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub paths: Vec<std::path::PathBuf>,
    /// The session working directory to discover the repo config from (the hook's `cwd`, so a
    /// linked-worktree edit resolves the same config the hook gated on).
    #[arg(long, value_name = "DIR")]
    pub cwd: std::path::PathBuf,
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
pub(crate) struct ImportantSymbolsArgs {
    /// Max load-bearing symbols to return.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Symbols to bias importance toward (the symbols you're working on) — names, refs
    /// (path::name), or sym_<hex> handles, comma-separated or repeated. A sym_<hex> handle
    /// resolves to its logical symbol's members; otherwise the entry is resolved by ref then
    /// name (ambiguous/missing entries are skipped). Raw numeric symbol ids are NOT accepted —
    /// they are reindex-churned rowids (#149). Empty = global importance (the CLI is
    /// global-by-default — it never auto-seeds from the git diff).
    #[arg(long, value_delimiter = ',')]
    pub personalize: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct DreamArgs {
    /// Max coverage-gap findings to surface (stale-reference findings are always all reported).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Run the memory verification pass (dream v2). Emits `memory_unverifiable` findings
    /// deterministically; with `[llm.dream] enabled = true` it also runs the out-of-process
    /// model verdict pass, writing `memory_reality` verdicts and `memory_divergence` findings.
    /// Off by default, so plain `rag-rat dream` stays byte-identical.
    #[arg(long)]
    pub verify: bool,
    /// Run the memory compaction pass (dream v2 pass 2). With `[llm.dream] enabled = true` it
    /// runs the out-of-process model compaction pass, rewriting un-summarized memories into 3–4
    /// sentence summaries in `memory_summaries`. Off by default; independent of `--verify`, so
    /// plain `rag-rat dream` stays byte-identical.
    #[arg(long)]
    pub compact: bool,
    /// Max memories each model pass may process in one run (the budget the churn-skip queues are
    /// capped at). Defaults to 20. Only meaningful with `--verify` / `--compact` and an enabled
    /// model.
    #[arg(long)]
    pub max_memories: Option<u32>,
    /// A dream finding id (or unambiguous prefix, git-style) to REVIEW instead of running the
    /// worklist. Pair with exactly one of `--accept` / `--dismiss` / `--reset`; the verdict is
    /// preserved across future runs.
    #[arg(value_name = "FINDING_ID")]
    pub finding: Option<String>,
    /// Review verdict: mark the finding ACCEPTED (acknowledged, action pending). Requires
    /// `<FINDING_ID>`.
    #[arg(long, conflicts_with_all = ["dismiss", "reset"])]
    pub accept: bool,
    /// Review verdict: DISMISS the finding (false positive / won't-fix) — hidden from the default
    /// worklist. Requires `<FINDING_ID>`.
    #[arg(long, conflicts_with_all = ["accept", "reset"])]
    pub dismiss: bool,
    /// Review verdict: RESET a previously accepted/dismissed finding back to open. Requires
    /// `<FINDING_ID>`.
    #[arg(long, conflicts_with_all = ["accept", "dismiss"])]
    pub reset: bool,
    /// In the worklist listing, ALSO show human-reviewed findings (accepted / dismissed), not just
    /// open — so you can see (and `--reset`) what you previously reviewed.
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ClonesArgs {
    /// Minimum pairwise overlap/max_len similarity. Valid range [0.5, 1.0] (default: 0.7, the θ
    /// threshold); out-of-range values are rejected.
    #[arg(long)]
    pub min_similarity: Option<f64>,
    /// Minimum number of copies for a class to be returned (default: 2).
    #[arg(long)]
    pub min_copies: Option<usize>,
    /// Maximum number of clone classes to return, sorted by ROI descending. A supplied limit is
    /// capped at the refine budget (currently 50): --limit N returns at most 50 classes, all
    /// refined. Omit the flag to retrieve all classes (only the top 50 refined).
    #[arg(long)]
    pub limit: Option<usize>,
    /// Print a human-readable explanation (template + variation points + proposed signature) for
    /// the refined class with this key (from the `class_key` field of a prior `clones` run)
    /// instead of the JSON/TOON listing.
    #[arg(long, value_name = "CLASS_KEY")]
    pub explain: Option<String>,
    /// Print a canonical, cross-build-stable RECALL signature instead of the listing: one sorted
    /// line per clone class (`<member_count>\t<sorted member refs>`), keyed on `path::symbol` refs
    /// (not rowids). This is the recall half of the clone measurement harness (#279): dump it on
    /// two builds (e.g. before/after a candidate-pruning change like #271's hot-token cap) and
    /// `diff` them — a removed or shrunk line is a recall regression. Forces a complete pass
    /// (ignores `--limit`).
    #[arg(long)]
    pub recall_signature: bool,
    /// Print the SORTED, UNCAPPED set of clone-symbol refs (one `path::symbol` per line) — every
    /// symbol that is in any coherent clone class. The SYMBOL-level recall signal for the #279
    /// harness, and the one to use when a change alters clustering granularity: unlike
    /// `--recall-signature` (class lines, capped at the per-class member limit), this counts every
    /// member of every class, so `diff`-ing two builds catches a symbol that stopped being a clone
    /// without false alarms from the member cap. Ignores `--limit`.
    #[arg(long)]
    pub recall_symbols: bool,
    /// Precompute + persist the clone-edge graph (a background-style writer pass), so subsequent
    /// `find_clones` / `clones-for` queries read the persisted graph instead of recomputing the
    /// super-linear candidate pairs every call — the way the graph scales to large repos (#286).
    /// Runs to completion under a write lock; re-running on unchanged content is a no-op. Prints a
    /// build report instead of the clone listing.
    #[arg(long)]
    pub precompute: bool,
    /// Soft per-pass time budget (seconds) for `--precompute`; the build checkpoints and resumes,
    /// so a bound leaves a partial graph that the next pass continues. Omit to run
    /// uninterrupted.
    #[arg(long, value_name = "SECONDS")]
    pub max_seconds: Option<u64>,
}

/// Selector for `clones-for`: positional `SYMBOL` (a qualified ref or `sym_<hex>` handle), or
/// `--path` + `--line` for a location-based lookup. Exactly one of these forms is required.
#[derive(Debug, Args)]
pub(crate) struct ClonesForArgs {
    /// Qualified symbol reference (`path/to/file.rs::fn_name`) or a `sym_<hex>` handle.
    #[arg(value_name = "SYMBOL")]
    pub symbol: Option<String>,
    /// File path for a PathLine lookup (requires --line).
    #[arg(long, value_name = "PATH")]
    pub path: Option<String>,
    /// Line number for a PathLine lookup (requires --path).
    #[arg(long, value_name = "N")]
    pub line: Option<i64>,
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
pub(crate) struct DoctorArgs {
    /// Reclaim dead space: run a one-off VACUUM to shrink the database file (drops the freelist).
    /// Takes the global schema lock and rewrites the whole file, so run it while agents/watchers
    /// are quiet — a live reader can make it fail with a busy error; stop them and retry.
    #[arg(long)]
    pub vacuum: bool,
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
    /// Force the legacy-f32 → int8 vector re-encode now (#312), ignoring the run-once meta gate.
    /// A format-only conversion (no model inference); idempotent — converts only rows still in
    /// f32. SHORT-CIRCUITS: re-encodes and exits, ignoring the other reconcile flags (no
    /// embeddings are computed). Honors `--max-seconds` (the conversion is bounded and resumes
    /// on a later run).
    #[arg(long)]
    pub reencode_vectors: bool,
}

#[cfg(feature = "eval")]
#[derive(Debug, Args)]
pub(crate) struct DumpVerifyPacksArgs {
    /// Spec JSON: `{ "memories": [{eval_id, kind, title, body, confidence?, binding_path?}],
    /// "dump": [eval_id, ...] }`. Memories are inserted into the configured index; `dump` names
    /// which of them to render packs for.
    #[arg(long)]
    pub spec: PathBuf,
    /// Root label the output packs are keyed by (`<eval_id>|<root_label>`), e.g. `/repo` or
    /// `/repo-drift`.
    #[arg(long)]
    pub root_label: String,
    /// Output JSON path: `{ "<eval_id>|<root_label>": "<pack text>" }`.
    #[arg(long)]
    pub out: PathBuf,
}

#[cfg(feature = "eval")]
#[derive(Debug, Args)]
pub(crate) struct DumpMemoryInputHashesArgs {
    /// Existing index database whose memories should be resolved.
    #[arg(long)]
    pub db: PathBuf,
    /// Repository scope containing the memories.
    #[arg(long)]
    pub repo_id: String,
    /// Input JSON array of existing memory ids.
    #[arg(long)]
    pub memory_ids: PathBuf,
    /// Output JSON object mapping each memory id to its current production input hash.
    #[arg(long)]
    pub out: PathBuf,
}

#[cfg(feature = "eval")]
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
    /// Commit-replay eval (#120): generate cases from indexed git history (commit message = query,
    /// diff's changed paths = recall gold) instead of the static queries TOML.
    #[arg(long)]
    pub replay: bool,
    /// Max recent commits to turn into replay cases.
    #[arg(long, default_value_t = 200)]
    pub replay_max_cases: u32,
    /// Skip bulk/mechanical commits whose changed-file count exceeds this (recall noise).
    #[arg(long, default_value_t = 20)]
    pub replay_max_files: u32,
    /// Leakage-free replay: score each case against an index of its commit's PARENT state (a
    /// throwaway worktree + full reindex per case). Slower; the absolute headline number. Implies
    /// `--replay`.
    #[arg(long)]
    pub replay_parent_state: bool,
    /// Run searches with the graded-git rerank ON (#109): scores the SAME at-head index with
    /// `[search] graded_git_rerank` forced true, for an A/B against the default fuse. Applies to
    /// both the active and the hash-vector-baseline pass. Pair with `--replay` for the inner-loop
    /// dial (`rag-rat eval --replay --rerank`).
    #[arg(long)]
    pub rerank: bool,
    /// How many hits each search returns — the width of the candidate pool scored (#109). Default
    /// 10 (unchanged behavior). `recall@3`/`recall@10` stay FIXED top-3/top-10 cutoffs regardless;
    /// widening this only grows `recall_at_returned`, the candidate-recall ceiling. At 100 it
    /// measures recall@100 ≈ the candidate-generation ceiling — pure measurement, no search
    /// change.
    #[arg(long, default_value_t = 10)]
    pub search_limit: usize,
}

/// `benchmark-embedding` (#346): provision an ephemeral cookbook box and sweep embedding throughput
/// across concurrency candidates, emitting per-candidate texts/s as JSON. The PRIMARY output is
/// JSON regardless of the global render flag (the point of the command is machine-readable
/// backend/concurrency comparison).
#[cfg(feature = "eval")]
#[derive(Debug, Args)]
pub(crate) struct BenchmarkEmbeddingArgs {
    /// The ephemeral cookbook provider spec that provisions the on-demand box (e.g.
    /// `"@rag-rat/cookbook modal"`). Required — this command only benchmarks ephemeral boxes.
    #[arg(long)]
    pub cookbook: String,
    /// Which OpenAI-compatible backend to provision + benchmark (`ollama` | `infinity` | `vllm`).
    #[arg(long, default_value = "ollama", value_parser = parse_remote_backend)]
    pub backend: rag_rat_base::config::RemoteBackend,
    /// The server-side model to serve: an ollama model name (ollama backend) or a HuggingFace id
    /// (infinity/vLLM). Required. Off-registry HF models fall back to a measured dim (one probe
    /// embed) since they have no registry spec.
    #[arg(long)]
    pub model: String,
    /// GPU hint for the recipe (provider-specific, e.g. `A10G`/`T4`). Omit to let the recipe pick.
    #[arg(long)]
    pub gpu: Option<String>,
    /// Concurrency candidates to measure, comma-separated (e.g. `1,2,4,8,16,32`). Omit to sweep
    /// the tuner's default ladder (powers of two up to the config's concurrency cap, plus the
    /// cap).
    #[arg(long, value_delimiter = ',')]
    pub candidates: Vec<u32>,
    /// Total sweep budget in milliseconds (split across candidates). Omit for the tuner default.
    #[arg(long)]
    pub budget_ms: Option<u64>,
    /// Write the JSON report to this path instead of stdout.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// clap `value_parser` for `--backend`: parse the backend selector via the SAME
/// [`RemoteBackend::from_db_str`](rag_rat_base::config::RemoteBackend::from_db_str) the config
/// layer uses, so the CLI and config accept identical spellings.
#[cfg(feature = "eval")]
fn parse_remote_backend(s: &str) -> Result<rag_rat_base::config::RemoteBackend, String> {
    rag_rat_base::config::RemoteBackend::from_db_str(s)
        .ok_or_else(|| format!("unknown backend `{s}` (expected ollama, infinity, or vllm)"))
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
    /// Run the oracle for a declared corpus and emit its typed before/after resolution report
    /// (C2). Applies the corpus health gate: exits non-zero if the run falls outside thresholds.
    Report(OracleReportArgs),
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
    /// Report on one oracle tool only (default: every known tool).
    #[arg(long, value_enum)]
    pub tool: Option<OracleToolArg>,
}

#[derive(Debug, Args)]
pub(crate) struct OracleReportArgs {
    /// The corpus id to report on (must match a `[[corpus]]` entry's `corpus_id`).
    #[arg(long)]
    pub corpus: String,
    /// Path to the corpus profiles file. Defaults to `<root>/tools/oracle-corpora.toml`.
    #[arg(long)]
    pub corpora: Option<PathBuf>,
    /// Consume a pre-built `.scip` instead of invoking the corpus's tool. Deterministic; the tool
    /// need not be installed.
    #[arg(long)]
    pub scip: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum OracleToolArg {
    #[value(name = "rust-analyzer")]
    RustAnalyzer,
    #[value(name = "scip-clang")]
    ScipClang,
    #[value(name = "scip-python")]
    ScipPython,
    #[value(name = "scip-typescript")]
    ScipTypescript,
    #[value(name = "scip-java")]
    ScipJava,
    /// The live watcher oracle for Rust (#534). Selectable for `oracle status`; `oracle run
    /// --tool ra-lsp` degrades to the documented `Blocked` hint (live tools never produce a
    /// `.scip`).
    #[value(name = "ra-lsp")]
    RaLsp,
    /// The live watcher oracle for TypeScript (#536). Same status/run semantics as `ra-lsp`.
    #[value(name = "ts-lsp")]
    TsLsp,
    /// The live watcher oracle for C/C++ (#536). Same status/run semantics as `ra-lsp`.
    #[value(name = "clangd-lsp")]
    ClangdLsp,
}

impl OracleToolArg {
    pub(crate) fn core(self) -> rag_rat_oracle::OracleTool {
        match self {
            OracleToolArg::RustAnalyzer => rag_rat_oracle::OracleTool::RustAnalyzer,
            OracleToolArg::ScipClang => rag_rat_oracle::OracleTool::ScipClang,
            OracleToolArg::ScipPython => rag_rat_oracle::OracleTool::ScipPython,
            OracleToolArg::ScipTypescript => rag_rat_oracle::OracleTool::ScipTypescript,
            OracleToolArg::ScipJava => rag_rat_oracle::OracleTool::ScipJava,
            OracleToolArg::RaLsp => rag_rat_oracle::OracleTool::RaLsp,
            OracleToolArg::TsLsp => rag_rat_oracle::OracleTool::TsLsp,
            OracleToolArg::ClangdLsp => rag_rat_oracle::OracleTool::ClangdLsp,
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
    Doctor,
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
        /// Directory anchor relative to the repo root (`""` for the repo root) — the area-level
        /// binding `dir`-bound memories use.
        #[arg(long)]
        dir: Option<String>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct PapertrailArgs {
    #[command(subcommand)]
    pub command: PapertrailCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum PapertrailCommand {
    /// Sync tracker items (issues / change requests) into the papertrail.
    Sync {
        /// Force a complete historical re-walk to heal cached rows.
        #[arg(long)]
        full: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct DistillArgs {
    #[command(subcommand)]
    pub command: DistillCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum DistillCommand {
    /// Run the deterministic extraction pass over the mirror: skeleton records, fixing commits,
    /// coalesced edges, anchor candidates, and the distill queue (model columns left for #704).
    Extract,
    /// Run deterministic extraction, then drain prepared snapshots through the configured model.
    Drain {
        /// Maximum prepared threads to process in this run.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
    },
}

#[derive(Debug, Args)]
pub(crate) struct HooksArgs {
    /// install, uninstall, or status.
    #[arg(value_enum)]
    pub action: HookAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum HookAction {
    Install,
    Uninstall,
    Status,
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
    /// Download and install a model by id. A `[llm.embedding.remote]` block in `rag-rat.toml`
    /// installs it over Ollama instead; otherwise it's a local install.
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
        assert_eq!(cli.config.as_deref(), Some("x.toml"));
        match cli.command {
            Command::Query(args) => {
                assert_eq!(args.query, vec!["foo", "bar"]);
                assert!(!args.explain);
            },
            other => panic!("expected query, got {other:?}"),
        }
    }

    #[test]
    fn config_defaults_to_discovery() {
        let cli = Cli::try_parse_from(["rag-rat", "gc"]).expect("parse");
        assert_eq!(cli.config, None, "no explicit --config ⇒ governing-path discovery");
    }

    #[test]
    fn json_flag_defaults_off_and_is_global() {
        // Absent → TOON (false). Present after the subcommand (global) → JSON (true).
        let default = Cli::try_parse_from(["rag-rat", "gc"]).expect("parse");
        assert!(!default.json, "--json must default off (TOON is the default render)");

        let flagged = Cli::try_parse_from(["rag-rat", "query", "foo", "--json"]).expect("parse");
        assert!(flagged.json, "--json must be accepted globally, after the subcommand");
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
    fn hooks_action_parses() {
        let cli = Cli::try_parse_from(["rag-rat", "hooks", "install"]).expect("parse");
        match cli.command {
            Command::Hooks(args) => {
                assert_eq!(args.action, HookAction::Install);
            },
            other => panic!("expected hooks, got {other:?}"),
        }
    }

    #[test]
    fn agent_hook_harness_parses_and_defaults_to_auto() {
        let cli = Cli::try_parse_from(["rag-rat", "agent-hook", "cursor"]).expect("parse");
        match cli.command {
            Command::AgentHook(args) => assert_eq!(args.harness, AgentHookHarnessArg::Cursor),
            other => panic!("expected agent-hook, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["rag-rat", "agent-hook"]).expect("parse default");
        match cli.command {
            Command::AgentHook(args) => assert_eq!(args.harness, AgentHookHarnessArg::Auto),
            other => panic!("expected agent-hook, got {other:?}"),
        }
    }

    #[test]
    fn serve_defaults_to_loopback_and_accepts_port_zero() {
        let defaults = Cli::try_parse_from(["rag-rat", "serve"]).expect("parse");
        match defaults.command {
            Command::Serve(args) => {
                assert!(args.bind.is_loopback());
                assert_eq!(args.port, 18120);
                assert!(args.token_env.is_none());
                assert!(args.allow_origin.is_empty());
            },
            other => panic!("expected serve, got {other:?}"),
        }

        let ephemeral = Cli::try_parse_from(["rag-rat", "serve", "--port", "0"]).expect("parse");
        match ephemeral.command {
            Command::Serve(args) => assert_eq!(args.port, 0),
            other => panic!("expected serve, got {other:?}"),
        }

        let hosted = Cli::try_parse_from([
            "rag-rat",
            "serve",
            "--bind",
            "0.0.0.0",
            "--token-env",
            "LENS_TOKEN",
            "--allow-origin",
            "HTTPS://Lens.Example:443/",
        ])
        .expect("parse");
        match hosted.command {
            Command::Serve(args) => {
                assert_eq!(args.token_env.as_deref(), Some("LENS_TOKEN"));
                assert_eq!(args.allow_origin, ["https://lens.example"]);
            },
            other => panic!("expected serve, got {other:?}"),
        }

        assert!(
            Cli::try_parse_from([
                "rag-rat",
                "serve",
                "--allow-origin",
                "https://lens.example/path",
            ])
            .is_err()
        );
    }

    #[test]
    fn distill_drain_limit_defaults_to_twenty_and_accepts_an_override() {
        let default = Cli::try_parse_from(["rag-rat", "distill", "drain"]).expect("parse");
        match default.command {
            Command::Distill(DistillArgs { command: DistillCommand::Drain { limit } }) => {
                assert_eq!(limit, 20);
            },
            other => panic!("expected distill drain, got {other:?}"),
        }

        let overridden =
            Cli::try_parse_from(["rag-rat", "distill", "drain", "--limit", "7"]).expect("parse");
        match overridden.command {
            Command::Distill(DistillArgs { command: DistillCommand::Drain { limit } }) => {
                assert_eq!(limit, 7);
            },
            other => panic!("expected distill drain, got {other:?}"),
        }
    }

    #[test]
    fn distill_drain_rejects_a_zero_limit() {
        let err = Cli::try_parse_from(["rag-rat", "distill", "drain", "--limit", "0"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn clones_parses_min_copies() {
        let cli = Cli::try_parse_from(["rag-rat", "clones", "--min-copies", "3"]).expect("parse");
        match cli.command {
            Command::Clones(args) => {
                assert_eq!(args.min_copies, Some(3));
                assert!(args.min_similarity.is_none());
                assert!(args.limit.is_none());
                assert!(args.explain.is_none());
            },
            other => panic!("expected clones, got {other:?}"),
        }
    }

    #[test]
    fn clones_parses_all_flags() {
        let cli = Cli::try_parse_from([
            "rag-rat",
            "clones",
            "--min-similarity",
            "0.8",
            "--min-copies",
            "3",
            "--limit",
            "10",
            "--explain",
            "deadbeef12345678",
        ])
        .expect("parse");
        match cli.command {
            Command::Clones(args) => {
                assert_eq!(args.min_similarity, Some(0.8));
                assert_eq!(args.min_copies, Some(3));
                assert_eq!(args.limit, Some(10));
                assert_eq!(args.explain.as_deref(), Some("deadbeef12345678"));
            },
            other => panic!("expected clones, got {other:?}"),
        }
    }

    #[test]
    fn clones_for_parses_positional_ref() {
        let cli =
            Cli::try_parse_from(["rag-rat", "clones-for", "src/a.rs::load_user"]).expect("parse");
        match cli.command {
            Command::ClonesFor(args) => {
                assert_eq!(args.symbol.as_deref(), Some("src/a.rs::load_user"));
                assert!(args.path.is_none());
                assert!(args.line.is_none());
            },
            other => panic!("expected clones-for, got {other:?}"),
        }
    }

    #[test]
    fn clones_for_parses_path_line() {
        let cli =
            Cli::try_parse_from(["rag-rat", "clones-for", "--path", "src/a.rs", "--line", "1"])
                .expect("parse");
        match cli.command {
            Command::ClonesFor(args) => {
                assert!(args.symbol.is_none());
                assert_eq!(args.path.as_deref(), Some("src/a.rs"));
                assert_eq!(args.line, Some(1));
            },
            other => panic!("expected clones-for, got {other:?}"),
        }
    }

    #[test]
    fn clones_for_parses_sym_handle() {
        let cli =
            Cli::try_parse_from(["rag-rat", "clones-for", "sym_deadbeef12345678"]).expect("parse");
        match cli.command {
            Command::ClonesFor(args) => {
                assert_eq!(args.symbol.as_deref(), Some("sym_deadbeef12345678"));
            },
            other => panic!("expected clones-for, got {other:?}"),
        }
    }

    #[cfg(feature = "eval")]
    #[test]
    fn benchmark_embedding_parses_candidates_and_backend() {
        let cli = Cli::try_parse_from([
            "rag-rat",
            "benchmark-embedding",
            "--cookbook",
            "@rag-rat/cookbook modal",
            "--backend",
            "infinity",
            "--model",
            "sentence-transformers/all-MiniLM-L6-v2",
            "--candidates",
            "1,2,4",
            "--budget-ms",
            "30000",
            "--gpu",
            "A10G",
        ])
        .expect("parse");
        match cli.command {
            Command::BenchmarkEmbedding(args) => {
                assert_eq!(args.cookbook, "@rag-rat/cookbook modal");
                assert_eq!(args.backend, rag_rat_base::config::RemoteBackend::Infinity);
                assert_eq!(args.model, "sentence-transformers/all-MiniLM-L6-v2");
                assert_eq!(args.candidates, vec![1, 2, 4]);
                assert_eq!(args.budget_ms, Some(30_000));
                assert_eq!(args.gpu.as_deref(), Some("A10G"));
                assert!(args.output.is_none());
            },
            other => panic!("expected benchmark-embedding, got {other:?}"),
        }
    }

    #[cfg(feature = "eval")]
    #[test]
    fn benchmark_embedding_defaults_backend_ollama_and_omits_candidates() {
        // `--cookbook` + `--model` are the only required flags; backend defaults to ollama and the
        // candidate list is empty (→ the handler uses the default ladder).
        let cli = Cli::try_parse_from([
            "rag-rat",
            "benchmark-embedding",
            "--cookbook",
            "@rag-rat/cookbook modal",
            "--model",
            "all-minilm",
        ])
        .expect("parse");
        match cli.command {
            Command::BenchmarkEmbedding(args) => {
                assert_eq!(args.backend, rag_rat_base::config::RemoteBackend::Ollama);
                assert!(args.candidates.is_empty());
                assert!(args.budget_ms.is_none());
            },
            other => panic!("expected benchmark-embedding, got {other:?}"),
        }
        // An unknown backend is rejected by the value_parser.
        assert!(
            Cli::try_parse_from([
                "rag-rat",
                "benchmark-embedding",
                "--cookbook",
                "cb",
                "--model",
                "m",
                "--backend",
                "bogus",
            ])
            .is_err(),
            "an unknown --backend must be rejected"
        );
        // `--cookbook` and `--model` are both required.
        assert!(Cli::try_parse_from(["rag-rat", "benchmark-embedding", "--model", "m"]).is_err());
        assert!(
            Cli::try_parse_from(["rag-rat", "benchmark-embedding", "--cookbook", "cb"]).is_err()
        );
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

    #[test]
    fn oracle_status_accepts_every_live_tool_selector() {
        // `oracle status --tool <live>` must parse: the status selector includes the live
        // backends; `oracle run --tool <live>` is gated to Blocked downstream instead.
        for tool in rag_rat_oracle::OracleTool::ALL.iter().filter(|tool| !tool.batch_capable()) {
            let cli =
                Cli::try_parse_from(["rag-rat", "oracle", "status", "--tool", tool.as_db_str()])
                    .unwrap_or_else(|err| panic!("{} must parse: {err}", tool.as_db_str()));
            match cli.command {
                Command::Oracle(OracleArgs { command: OracleCommand::Status(args) }) => {
                    assert_eq!(args.tool.map(OracleToolArg::core), Some(*tool));
                },
                other => panic!("expected oracle status, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_tool_selector_covers_every_registered_oracle_tool() {
        // The CLI selector and the tool registry drift silently otherwise: a new backend would be
        // indexed and surfaced but unselectable from `oracle status`/`oracle run`, and its
        // `--tool` name could disagree with the id its rows are persisted under.
        use clap::ValueEnum as _;
        let selectable: Vec<rag_rat_oracle::OracleTool> =
            OracleToolArg::value_variants().iter().map(|arg| arg.core()).collect();
        assert_eq!(
            selectable,
            rag_rat_oracle::OracleTool::ALL.to_vec(),
            "OracleToolArg must mirror OracleTool::ALL, in order",
        );
        for arg in OracleToolArg::value_variants() {
            let name = arg
                .to_possible_value()
                .expect("every selector variant is selectable")
                .get_name()
                .to_string();
            assert_eq!(
                name,
                arg.core().as_db_str(),
                "the --tool name must match the persisted tool id",
            );
        }
    }

    #[test]
    fn oracle_report_requires_corpus_and_takes_optional_paths() {
        // `--corpus` is mandatory; a bare `oracle report` must not parse.
        assert!(Cli::try_parse_from(["rag-rat", "oracle", "report"]).is_err());
        let cli = Cli::try_parse_from([
            "rag-rat",
            "oracle",
            "report",
            "--corpus",
            "py-requests",
            "--corpora",
            "/tmp/corpora.toml",
            "--scip",
            "/tmp/x.scip",
        ])
        .expect("parse");
        match cli.command {
            Command::Oracle(OracleArgs { command: OracleCommand::Report(args) }) => {
                assert_eq!(args.corpus, "py-requests");
                assert_eq!(
                    args.corpora.as_deref(),
                    Some(std::path::Path::new("/tmp/corpora.toml"))
                );
                assert_eq!(args.scip.as_deref(), Some(std::path::Path::new("/tmp/x.scip")));
            },
            other => panic!("expected oracle report, got {other:?}"),
        }
    }
}
