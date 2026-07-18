use std::sync::Arc;

use rag_rat_base::config::Config;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
#[cfg(not(unix))]
use rmcp::transport::stdio;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Map, Value};
use tokio::sync::Semaphore;

use crate::blocking::{self, ToolTimeoutPolicy};

/// How often the stale-anchor rebind nudge may ride a tool result across the fleet (#752): once per
/// 30 minutes, unless a `memory_create`/`memory_update` forces it. Keeps the nudge off the vast
/// majority of tool calls so it stops inflating per-call tokens.
const NUDGE_TTL_MS: i64 = 30 * 60 * 1000;

#[derive(Clone)]
pub struct RagRatService {
    /// `None` ⇒ DORMANT: the server was launched outside any rag-rat repo (no `rag-rat.toml` at or
    /// above cwd). It still speaks MCP so a globally-registered server stays alive, but every tool
    /// call returns the dormant notice ([`dormant_tool_result`]) instead of touching an index.
    /// `Some` ⇒ the active server bound to a resolved repo config.
    config: Option<Config>,
    /// Output format for tool results. Defaults to TOON (denser for the LLM that reads them); set
    /// to JSON when the server was launched as `rag-rat mcp --json` — MCP has no per-call flag, so
    /// the choice is made once at launch (the CLI `--json` flag flows in via `run_stdio`).
    output_format: rag_rat_core::OutputFormat,
    /// In-flight tool-call counter, observed by the hot-upgrade teardown so it drains at a request
    /// boundary before `exec`. Present only on Unix, where hot-upgrade is supported.
    #[cfg(unix)]
    inflight: std::sync::Arc<crate::upgrade::Inflight>,
    tool_workers: Arc<Semaphore>,
    /// Per-agent (= per process) record of meta this session already saw — drive-by memories and
    /// static caveats — so re-surfacing them can be trimmed (#752). An `Arc` so every `Clone` of
    /// the service (each tool call clones it) shares one seen-set for the whole session.
    agent_seen: Arc<crate::output_trim::AgentSeen>,
}

impl RagRatService {
    /// The ACTIVE server bound to a resolved repo config. Published constructor — its
    /// `new(Config, …)` signature is preserved for source compatibility; the config-less DORMANT
    /// server is built via [`RagRatService::new_dormant`] instead of widening this to `Option`.
    pub fn new(config: Config, output_format: rag_rat_core::OutputFormat) -> Self {
        Self::with_optional_config(Some(config), output_format)
    }

    /// The DORMANT server (launched outside any rag-rat repo): no config, so every tool call
    /// returns [`RagRatService::dormant_tool_result`]. Separate from [`RagRatService::new`] so
    /// that constructor's published signature stays source-compatible (#603).
    pub(crate) fn new_dormant(output_format: rag_rat_core::OutputFormat) -> Self {
        Self::with_optional_config(None, output_format)
    }

    fn with_optional_config(
        config: Option<Config>,
        output_format: rag_rat_core::OutputFormat,
    ) -> Self {
        Self {
            config,
            output_format,
            #[cfg(unix)]
            inflight: crate::upgrade::Inflight::new(),
            tool_workers: Arc::new(Semaphore::new(blocking::tool_workers())),
            agent_seen: Arc::new(crate::output_trim::AgentSeen::default()),
        }
    }

    /// Shared in-flight counter, so the hot-upgrade signal task can wait for tool calls to drain.
    #[cfg(unix)]
    pub fn inflight(&self) -> std::sync::Arc<crate::upgrade::Inflight> {
        std::sync::Arc::clone(&self.inflight)
    }

    fn call(&self, name: &str, value: Value) -> Result<CallToolResult, ErrorData> {
        // Dormant (launched outside a repo): return the notice. We deliberately do NOT re-discover
        // and serve a config that appears mid-session — a server without the active lifecycle
        // (watcher, git-hook freshness) could return results NOT validated against current source,
        // breaking rag-rat's core guarantee. Dormancy is binary: dormant, or fully active from
        // launch. The notice tells the user to restart the MCP server after `init` + `index`
        // (#603).
        let Some(config) = &self.config else {
            // A dormant server still rejects an UNKNOWN tool name exactly like an active one, so a
            // typo or stale tool surfaces as an error instead of being masked as `no_index`. Only a
            // KNOWN (advertised) tool earns the dormant notice.
            if !crate::tools::is_known_tool(name) {
                return Err(ErrorData::internal_error(format!("unknown tool `{name}`"), None));
            }
            return Ok(self.dormant_tool_result());
        };
        let mut value = crate::tools::call_tool_for_config(config, name, value)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        // Trim repeated meta to cut per-call tokens (#752): dedup drive-by memories this agent
        // already saw (to a tiny stub), throttle the static caveats, drop redundant per-edge flags.
        // TOON-only — it is a lossy transform (stubs a memory, drops default-valued fields), so it
        // stays off `--json` mode, whose whole purpose is stable, complete shapes for programmatic
        // clients (same reason the prose nudge is JSON-suppressed). And NEVER on the explicit
        // `memory_*` tools — there the agent asked for the memory, so it gets it in full.
        if self.output_format != rag_rat_core::OutputFormat::Json && !name.starts_with("memory_") {
            crate::output_trim::trim_result(&mut value, &self.agent_seen);
        }
        // MCP tool results are text content read by an LLM, so render TOON by default — it is
        // materially denser than JSON on the uniform-row payloads that dominate these tools, and
        // ties JSON on nested ones. `render` falls back to compact JSON on a TOON encode error, so
        // a tool result is never lost. JSON is reachable by launching `rag-rat mcp --json`
        // (the format is chosen once at launch; MCP has no per-call flag).
        let text = rag_rat_core::render(&value, self.output_format);
        let mut content = vec![Content::text(text)];
        if let Some(nudge) = self.stale_memory_nudge(name) {
            content.push(Content::text(nudge));
        }
        Ok(CallToolResult::success(content))
    }

    async fn call_async(&self, name: String, value: Value) -> Result<CallToolResult, ErrorData> {
        let service = self.clone();
        let timeout = blocking::tool_timeout();
        let worker_name = name.clone();
        let timeout_policy = ToolTimeoutPolicy::for_tool(&name);
        let workers = Arc::clone(&self.tool_workers);
        // All tools funnel through this async chokepoint. Acquire the hot-upgrade in-flight guard
        // before queuing blocking work, then move it into the worker so a timed-out/detached read
        // still keeps the process from hot-execing until the blocking closure actually exits.
        #[cfg(unix)]
        let inflight = self.inflight.guard();
        blocking::run_blocking_tool(name, timeout, timeout_policy, workers, move || {
            #[cfg(unix)]
            let _inflight = inflight;
            service.call(&worker_name, value)
        })
        .await
    }

    /// Surface drifted repo-memory anchors to the AGENT as a second tool-result content block.
    /// Claude Code's agent is pull-based — MCP server notifications (`notifications/message`) reach
    /// the UI/logs but NOT the model's context (anthropics/claude-code#3174) — so a tool result is
    /// the one MCP-native channel that puts an actionable signal in front of the model. The nudge
    /// self-limits: once the agent runs `memory_rebind`, the count drops to 0 and it stops showing.
    ///
    /// THROTTLED (#752) to cut per-call tokens: it rides at most once per [`NUDGE_TTL_MS`] across
    /// the fleet (a shared last-shown timestamp in the sidecar store, claimed atomically so
    /// concurrent sessions don't both show), EXCEPT right after a `memory_create` /
    /// `memory_update` — the agent just touched memory, so a freshly-needed rebind is worth
    /// surfacing on that call regardless.
    ///
    /// Suppressed in `--json` mode: that mode exists for clients that parse the tool text AS JSON
    /// (or concatenate all text blocks), and a prose block would break them. The nudge is
    /// agent-directed prose, meaningful only in the default TOON (LLM-facing) mode.
    fn stale_memory_nudge(&self, tool: &str) -> Option<String> {
        // No nudge in dormant mode (no index to read) or in `--json` mode (prose breaks JSON
        // clients).
        let config = self.config.as_ref()?;
        if self.output_format == rag_rat_core::OutputFormat::Json {
            return None;
        }
        let n = rag_rat_core::memory_attention_count(&config.database);
        if n == 0 {
            return None;
        }
        // A memory create/update forces the nudge (and resets the window); everything else is gated
        // on the throttle. The slot is claimed atomically — a `false` return means throttled or
        // another session just claimed it, so this call stays silent. Keyed by `repo_id` (the same
        // identity the per-repo write lock uses) so repos sharing a global DB don't mute each other
        // (#753 review); resolving it here, gated behind `n > 0`, keeps it off the common path.
        let force = matches!(tool, "memory_create" | "memory_update");
        let repo_id = rag_rat_base::locks::write_lock_repo_id(config);
        if !rag_rat_core::sidecar_state::take_memory_nudge_slot(
            &config.database,
            &repo_id,
            rag_rat_base::time::now_ms(),
            NUDGE_TTL_MS,
            force,
        ) {
            return None;
        }
        let noun = if n == 1 { "memory" } else { "memories" };
        Some(format!(
            "rag-rat: {n} active repo {noun} have stale/gone anchors. Call `memory_doctor` to \
             list them with suggested re-anchor targets, then `memory_rebind` to fix — so \
             source-anchored memory stays trustworthy for the next agent."
        ))
    }

    /// The result every tool call returns while the server is DORMANT (launched outside any rag-rat
    /// repo, and cwd STILL has no config). Rendered through the SAME output-format path as every
    /// tool result, so `--json` mode yields a directly-parseable JSON block (the JSON contract
    /// holds in dormant mode too) while the default TOON mode stays LLM-friendly. NON-error:
    /// the agent reads it as a normal response explaining how to enable an index.
    fn dormant_tool_result(&self) -> CallToolResult {
        let payload = serde_json::json!({
            "status": "no_index",
            "message": "This rag-rat MCP server was started outside an indexed rag-rat repository, \
                        so it has no index to serve here.",
            "remedy": "Run the `init-rag-rat` skill to set this repo up conversationally, or run \
                       `rag-rat init` then `rag-rat index` in the repository root yourself. Either \
                       way, restart (reconnect) the rag-rat MCP server afterward so it activates \
                       against the new index.",
        });
        let text = rag_rat_core::render(&payload, self.output_format);
        CallToolResult::success(vec![Content::text(text)])
    }
}

impl ServerHandler for RagRatService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rag-rat", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Read-only-source repo intelligence. Index and auto-heal writes are confined to \
                 the configured SQLite database.",
            )
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        // The catalog (TOOL_NAMES / schema / description) is the single source of truth advertised
        // by `list_tools`, and `call_tool_for_config` (via `call`) is the by-name dispatcher.
        // Forward the raw request arguments straight to the `call()` chokepoint: no per-tool
        // `#[tool]` forwarder and no `Parameters<T>` serialize->deserialize round-trip — the client
        // JSON reaches exactly one deserialize, in `call_tool_with_db`.
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);
        self.call_async(request.name.to_string(), args).await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = crate::tools::TOOL_NAMES
            .iter()
            .map(|name| {
                let input_schema = match crate::tools::schema(name) {
                    Value::Object(map) => map,
                    _ => Map::new(),
                };
                Tool::new((*name).to_string(), crate::tools::description(name), input_schema)
            })
            .collect();
        Ok(ListToolsResult { tools, meta: None, next_cursor: None })
    }
}

/// Serve the stdio MCP server for a RESOLVED repo config — the ACTIVE server: keeps the index fresh
/// (watcher), serves the grep-hook listener, and (Unix) arms the SIGUSR1 hot-upgrade. Published
/// entry point; its `run_stdio(Config, …)` signature is preserved for source compatibility. The
/// config-less DORMANT server is [`run_stdio_dormant`].
pub async fn run_stdio(
    config: Config,
    output_format: rag_rat_core::OutputFormat,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        run_stdio_unix(config, output_format).await
    }
    #[cfg(not(unix))]
    {
        // Keep the index fresh while a session is connected; dropping the watcher on shutdown
        // runs a final timeout-skip pass. (Hot-upgrade is Unix-only.)
        let _watcher = rag_rat_core::watch::Watcher::spawn(config.clone());
        let service = RagRatService::new(config, output_format).serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    }
}

/// The DORMANT server (launched outside any rag-rat repo, so no config resolved): no watcher, no
/// hook listener, no hot-upgrade — nothing to keep fresh. It just serves the MCP protocol until the
/// client disconnects; `tools/list` still advertises the full catalog, but every `tools/call`
/// returns [`RagRatService::dormant_tool_result`]. Split from [`run_stdio`] so that published
/// `run_stdio(Config)` signature stays source-compatible, while a globally-registered `rag-rat mcp`
/// can still stay alive in a non-rag-rat project instead of dying (#603).
pub async fn run_stdio_dormant(output_format: rag_rat_core::OutputFormat) -> anyhow::Result<()> {
    let running = RagRatService::new_dormant(output_format).serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// Aborts a spawned task when dropped. Used to tear down the hook listener on normal EOF
/// shutdown so its socket + election lock release promptly (a hot-`exec` replaces the process
/// image instead, so the task vanishes and the successor re-elects).
#[cfg(unix)]
struct AbortOnDrop(tokio::task::JoinHandle<()>);

#[cfg(unix)]
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Unix `run_stdio`: serves over a [`crate::upgrade::GatedStdin`] so a `SIGUSR1` can hot-`exec`
/// the newly installed binary in place, and resumes (skipping `initialize`) when handed off to.
#[cfg(unix)]
async fn run_stdio_unix(
    config: Config,
    output_format: rag_rat_core::OutputFormat,
) -> anyhow::Result<()> {
    use std::sync::Arc;

    use rmcp::service::serve_directly;
    use tokio::signal::unix::{SignalKind, signal};

    use crate::upgrade::{self, GatedStdin, Upgrade, UpgradeGate};

    let gate = UpgradeGate::new();
    let service = RagRatService::new(config.clone(), output_format);
    let inflight = service.inflight();

    let transport = (GatedStdin::new(tokio::io::stdin(), Arc::clone(&gate)), tokio::io::stdout());

    // Resume (skip `initialize`) iff a predecessor handed off a session we can still honor.
    let running = match upgrade::take_handoff() {
        Some(handoff) if upgrade::protocol_supported(&handoff.negotiated_protocol_version) =>
            serve_directly(service, transport, Some(handoff.peer_info)),
        Some(handoff) => {
            // The new binary can't honor the negotiated protocol — exit cleanly so the client
            // reconnects and renegotiates rather than resuming on a mismatch.
            eprintln!(
                "hot-upgrade: protocol {} unsupported by this binary; exiting for clean reconnect",
                handoff.negotiated_protocol_version
            );
            return Ok(());
        },
        None => service.serve(transport).await?,
    };

    // Keep the index fresh while connected. Its lock fds are CLOEXEC, so a hot-`exec` releases
    // them automatically; on normal EOF the drop runs a final timeout-skip pass. When hot-upgrade
    // is armed, the elected watcher also watches the binary dir to drive the fleet trigger.
    let install_path = upgrade::install_path();
    let _watcher =
        rag_rat_core::watch::Watcher::spawn_with_fleet(config.clone(), install_path.clone());

    // Grep-augment hook listener: one elected listener per worktree. Spawned after the `running`
    // match so it covers both cold start and hot-upgrade resume. On normal EOF shutdown the guard
    // aborts the task so the socket + election lock release promptly; on a hot-`exec` the process
    // image is replaced (task vanishes) and the new process re-elects.
    let _hook_listener = AbortOnDrop(crate::agent_hook::spawn_listener(config.clone()));

    // Arm the SIGUSR1 hot-upgrade handler only when an install target is configured.
    if let Some(install_path) = install_path {
        // rmcp 1.8 changed `peer_info()` from `Option<&_>` to `Option<Arc<_>>`; deref-and-clone to
        // the owned `InitializeRequestParams` the `Upgrade`/handoff want. `*p` derefs either form,
        // so this compiles on 1.7 and 1.8 alike.
        let peer_info = running.peer().peer_info().map(|p| (*p).clone()).unwrap_or_default();
        let negotiated_protocol_version = peer_info.protocol_version.as_str().to_string();
        let handoff_dir = config
            .database
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let upgrade = Upgrade {
            gate: Arc::clone(&gate),
            inflight,
            install_path,
            handoff_dir,
            peer_info,
            negotiated_protocol_version,
        };
        tokio::spawn(async move {
            let Ok(mut sigusr1) = signal(SignalKind::user_defined1()) else {
                eprintln!("hot-upgrade: could not install SIGUSR1 handler; disabled");
                return;
            };
            // On success `run()` never returns (it `exec`s or exits); on a drain-timeout abort it
            // returns and we wait for the next signal.
            while sigusr1.recv().await.is_some() {
                upgrade.run().await;
            }
        });
    }

    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use rag_rat_base::config::{Config, ResolvedTarget, TargetKind};
    use rag_rat_base::language::Language;
    use rag_rat_core::{IndexDatabase, OutputFormat};
    use serde_json::json;

    use super::*;
    use crate::blocking;

    static N: AtomicU64 = AtomicU64::new(0);

    fn config_over_temp_repo() -> (PathBuf, Config) {
        let root = std::env::temp_dir().join(format!(
            "rag-rat-server-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn open_database() {}\n").unwrap();
        let config = Config {
            trackers: Vec::new(),
            papertrail: Default::default(),
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["**/*.rs".to_string()],
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
        IndexDatabase::rebuild(&config).unwrap();
        (root, config)
    }

    fn service_over_temp_repo() -> (PathBuf, RagRatService) {
        let (root, config) = config_over_temp_repo();
        (root, RagRatService::new(config, OutputFormat::Toon))
    }

    fn ok_result() -> CallToolResult {
        CallToolResult::success(vec![Content::text("ok")])
    }

    fn test_tool_workers(permits: usize) -> Arc<Semaphore> {
        Arc::new(Semaphore::new(permits))
    }

    #[test]
    fn timeout_and_worker_env_ignore_blank_zero_and_invalid_values() {
        assert_eq!(blocking::parse_tool_timeout("5"), Some(Duration::from_secs(5)));
        assert_eq!(blocking::parse_tool_timeout(" 9 "), Some(Duration::from_secs(9)));
        assert_eq!(blocking::parse_tool_timeout(""), None);
        assert_eq!(blocking::parse_tool_timeout("0"), None);
        assert_eq!(blocking::parse_tool_timeout("nope"), None);
        assert_eq!(blocking::parse_tool_workers("3"), Some(3));
        assert_eq!(blocking::parse_tool_workers(" 4 "), Some(4));
        assert_eq!(blocking::parse_tool_workers(""), None);
        assert_eq!(blocking::parse_tool_workers("0"), None);
        assert_eq!(blocking::parse_tool_workers("nope"), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn call_async_dispatches_through_blocking_chokepoint() {
        let (root, svc) = service_over_temp_repo();
        let result = svc.call_async("index_status".to_string(), json!({})).await.unwrap();

        assert!(!result.content.is_empty(), "async tool dispatch returned no content");
        let err = svc
            .call_async("definitely_not_a_tool".to_string(), json!({}))
            .await
            .expect_err("unknown tool must surface as an MCP error");
        assert!(
            err.message.contains("unknown tool"),
            "unknown-tool error should preserve the dispatcher message, got: {}",
            err.message
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn call_async_counts_queued_blocking_work_as_inflight() {
        let (root, mut svc) = service_over_temp_repo();
        svc.tool_workers = test_tool_workers(1);
        let held_worker = Arc::clone(&svc.tool_workers).acquire_owned().await.unwrap();
        let inflight = svc.inflight();

        let pending = tokio::spawn({
            let svc = svc.clone();
            async move { svc.call_async("index_status".to_string(), json!({})).await }
        });
        for _ in 0..20 {
            if inflight.count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(inflight.count(), 1, "queued blocking work must count as in-flight");

        drop(held_worker);
        pending.await.unwrap().unwrap();
        assert_eq!(inflight.count(), 0, "in-flight guard must drop after the worker exits");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_tool_work_does_not_starve_the_runtime() {
        let workers = test_tool_workers(2);
        let slow = tokio::spawn(blocking::run_blocking_tool(
            "slow_test_tool".to_string(),
            Duration::from_secs(1),
            ToolTimeoutPolicy::ReturnTimeout,
            Arc::clone(&workers),
            || {
                std::thread::sleep(Duration::from_millis(200));
                Ok(ok_result())
            },
        ));

        tokio::time::sleep(Duration::from_millis(10)).await;
        let quick = tokio::time::timeout(
            Duration::from_millis(100),
            blocking::run_blocking_tool(
                "quick_test_tool".to_string(),
                Duration::from_secs(1),
                ToolTimeoutPolicy::ReturnTimeout,
                workers,
                || Ok(ok_result()),
            ),
        )
        .await;
        assert!(quick.is_ok(), "quick tool call should not wait behind blocking work");
        quick.unwrap().unwrap();
        slow.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_tool_work_returns_timeout_error() {
        let err = blocking::run_blocking_tool(
            "timeout_test_tool".to_string(),
            Duration::from_millis(10),
            ToolTimeoutPolicy::ReturnTimeout,
            test_tool_workers(1),
            || {
                std::thread::sleep(Duration::from_millis(100));
                Ok(ok_result())
            },
        )
        .await
        .expect_err("slow blocking work must time out");

        assert!(
            err.message.contains("timed out"),
            "timeout error should be actionable, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_tool_work_applies_worker_limit() {
        let workers = test_tool_workers(1);
        let slow = tokio::spawn(blocking::run_blocking_tool(
            "slow_test_tool".to_string(),
            Duration::from_secs(1),
            ToolTimeoutPolicy::ReturnTimeout,
            Arc::clone(&workers),
            || {
                std::thread::sleep(Duration::from_millis(100));
                Ok(ok_result())
            },
        ));
        tokio::time::sleep(Duration::from_millis(10)).await;

        let err = blocking::run_blocking_tool(
            "queued_test_tool".to_string(),
            Duration::from_millis(20),
            ToolTimeoutPolicy::ReturnTimeout,
            workers,
            || Ok(ok_result()),
        )
        .await
        .expect_err("a queued tool must respect the shared worker limit and timeout");
        assert!(
            err.message.contains("timed out"),
            "queued timeout error should be actionable, got: {}",
            err.message
        );
        slow.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn write_tool_deadline_waits_for_blocking_work_to_finish() {
        let started = Instant::now();
        let result = blocking::run_blocking_tool(
            "memory_create".to_string(),
            Duration::from_millis(10),
            ToolTimeoutPolicy::WaitForCompletion,
            test_tool_workers(1),
            || {
                std::thread::sleep(Duration::from_millis(60));
                Ok(ok_result())
            },
        )
        .await
        .expect("write-classified tools must not detach and return timeout");

        assert!(!result.content.is_empty());
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "write tool returned before the blocking worker actually stopped"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_tool_work_propagates_tool_errors() {
        let err = blocking::run_blocking_tool(
            "error_test_tool".to_string(),
            Duration::from_secs(1),
            ToolTimeoutPolicy::ReturnTimeout,
            test_tool_workers(1),
            || Err(ErrorData::internal_error("intentional tool failure".to_string(), None)),
        )
        .await
        .expect_err("tool errors must not be converted into successes");

        assert!(
            err.message.contains("intentional tool failure"),
            "tool error should keep its original message, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_tool_work_reports_panics_as_mcp_errors() {
        let err = blocking::run_blocking_tool(
            "panic_test_tool".to_string(),
            Duration::from_secs(1),
            ToolTimeoutPolicy::ReturnTimeout,
            test_tool_workers(1),
            || panic!("intentional panic from blocking tool"),
        )
        .await
        .expect_err("blocking worker panic must be converted into an MCP error");

        assert!(
            err.message.contains("panic_test_tool") && err.message.contains("panicked"),
            "panic error should name the tool and failure mode, got: {}",
            err.message
        );
    }

    /// The staleness nudge (#160) rides a TOON tool result as a second content block, SUPPRESSED in
    /// `--json` mode (Codex #160 review), and THROTTLED (#752) to at most once per window across
    /// the fleet — except a `memory_create`/`memory_update` forces it (and resets the window).
    #[test]
    fn stale_memory_nudge_throttles_forces_and_suppresses_json() {
        let (root, config) = config_over_temp_repo();
        // A memory bound to an unindexed/absent path resolves `gone` → drift the nudge reports.
        let toon = RagRatService::new(config.clone(), OutputFormat::Toon);
        toon.call(
            "memory_create",
            json!({
                "kind": "Invariant",
                "title": "drift",
                "body": "b",
                "confidence": "high",
                "bind": {"path": "does/not/exist.rs"}
            }),
        )
        .unwrap();
        // A path binding is created `current`; validation flips the absent path to `gone` —
        // after TWO passes, per the #492 downgrade hysteresis (the first only arms the marker).
        toon.call("memory_validate", json!({})).unwrap();
        toon.call("memory_validate", json!({})).unwrap();

        // JSON mode NEVER shows the prose nudge (would break JSON-parsing clients).
        let json_svc = RagRatService::new(config, OutputFormat::Json);
        assert!(json_svc.stale_memory_nudge("index_status").is_none(), "JSON suppresses the nudge");

        // A memory_create FORCES the nudge regardless of the throttle window, and proves it rides a
        // real TOON tool result as a second content block. (n stays > 0 — this new binding is
        // `current`, the original one is still `gone`.)
        let forced = toon
            .call(
                "memory_create",
                json!({
                    "kind": "Decision", "title": "another", "body": "b", "confidence": "high",
                    "bind": {"path": "also/absent.rs"}
                }),
            )
            .unwrap();
        assert_eq!(forced.content.len(), 2, "a forced nudge rides the tool result as a 2nd block");

        // Immediately after — within the window — a plain read tool is THROTTLED (nudge
        // suppressed).
        assert!(
            toon.stale_memory_nudge("semantic_search").is_none(),
            "throttled inside the window",
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// #752 end-to-end: in TOON (LLM-facing) mode a repo memory riding a DRIVE-BY result
    /// (symbol_lookup) is full on first surface and STUBBED on re-show within the session, while an
    /// EXPLICIT `memory_*` tool is never trimmed. JSON mode is UNTRIMMED even on repeat, so
    /// programmatic clients keep stable, complete shapes (Codex #752 review).
    #[test]
    fn drive_by_memories_dedup_in_toon_but_json_mode_is_untrimmed() {
        fn text_of(result: &CallToolResult) -> String {
            result.content[0].as_text().unwrap().text.clone()
        }
        fn call_json(svc: &RagRatService, tool: &str, args: Value) -> Value {
            serde_json::from_str(&text_of(&svc.call(tool, args).unwrap())).unwrap()
        }

        let (root, config) = config_over_temp_repo(); // indexes `open_database`
        let lookup_args = json!({"symbol": "open_database", "allow_ambiguous": true});
        const ELIDED: &str = "already surfaced this session";

        // Setup on a JSON service (results parse cleanly): resolve the symbol, bind a memory to it
        // so it rides drive-by results. The anchor is `current` (real indexed symbol) → no nudge.
        let json_svc = RagRatService::new(config.clone(), OutputFormat::Json);
        let lookup = call_json(&json_svc, "symbol_lookup", lookup_args.clone());
        let sym_id = lookup["candidates"][0]["id"].as_str().unwrap().to_string();
        let created = call_json(
            &json_svc,
            "memory_create",
            json!({
                "kind": "Invariant", "title": "db open invariant",
                "body": "a distinctive memory body", "confidence": "high",
                "bind": {"id": sym_id.clone()}
            }),
        );
        let mem_id = created["memory"]["memory_id"].as_str().unwrap().to_string();

        // TOON: first surface carries the full memory (no stub marker); the re-show is stubbed —
        // the `elided` marker appears, id + title stay. (A fresh service = a fresh per-agent
        // seen-set.)
        let toon = RagRatService::new(config.clone(), OutputFormat::Toon);
        let first = text_of(&toon.call("symbol_lookup", lookup_args.clone()).unwrap());
        assert!(first.contains(&mem_id), "the memory rides the first drive-by result");
        assert!(!first.contains(ELIDED), "first TOON surface is the full memory, not a stub");
        let second = text_of(&toon.call("symbol_lookup", lookup_args.clone()).unwrap());
        assert!(second.contains(ELIDED), "re-shown drive-by memory is stubbed with a fetch hint");
        assert!(
            second.contains(&mem_id) && second.contains("db open invariant"),
            "the stub keeps the id and title",
        );

        // An EXPLICIT memory tool is NEVER trimmed — even though this agent already saw the memory
        // via the drive-by surfaces above, `memory_for_symbol` returns it in full, not a stub.
        let explicit =
            text_of(&toon.call("memory_for_symbol", json!({"id": sym_id.clone()})).unwrap());
        assert!(explicit.contains(&mem_id), "the explicit memory tool returns the memory");
        assert!(!explicit.contains(ELIDED), "the explicit memory tool is not stubbed");

        // JSON mode is UNTRIMMED even on repeat: the drive-by memory keeps its full shape, no stub.
        for _ in 0..2 {
            let v = call_json(&json_svc, "symbol_lookup", lookup_args.clone());
            let m = &v["candidates"][0]["memories"][0];
            assert_eq!(m["memory_id"].as_str(), Some(mem_id.as_str()));
            assert!(m["elided"].is_null(), "JSON mode never stubs a drive-by memory");
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn get_info_advertises_tool_capability() {
        let (root, svc) = service_over_temp_repo();
        let info = svc.get_info();
        assert!(info.capabilities.tools.is_some(), "server must advertise tools");
        assert_eq!(info.server_info.name, "rag-rat");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn call_dispatches_every_read_tool_and_rejects_unknown() {
        let (root, svc) = service_over_temp_repo();
        // The chokepoint `call()` funnels every tool: success path (render to TOON text) across a
        // representative read-tool set, plus the error mapping for an unknown tool.
        let calls = [
            ("semantic_search", json!({ "query": "open_database" })),
            ("symbol_lookup", json!({ "symbol": "open_database" })),
            ("find_callers", json!({ "symbol": "open_database" })),
            ("trace_callees", json!({ "symbol": "open_database" })),
            ("impact_surface", json!({ "symbol": "open_database" })),
            ("docs_for_symbol", json!({ "symbol": "open_database" })),
            ("git_history_for_symbol", json!({ "symbol": "open_database" })),
            ("repo_brief", json!({})),
            ("repo_clusters", json!({})),
            ("important_symbols", json!({})),
            ("ffi_surface", json!({})),
            ("compare_graph_to_scip", json!({})),
            ("index_status", json!({})),
            ("llm_status", json!({})),
            ("papertrail_sync_status", json!({})),
            ("memory_validate", json!({})),
        ];
        for (name, args) in calls {
            let result = svc.call(name, args).unwrap_or_else(|e| panic!("{name} failed: {e:?}"));
            assert!(!result.content.is_empty(), "{name} returned no content");
        }
        assert!(svc.call("definitely_not_a_tool", json!({})).is_err(), "unknown tool must error");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_show_expands_a_memory_to_its_full_body_by_id() {
        // The expand path: given the `memory_id` from a compact summary (surface="summary"),
        // `memory_show` returns the COMPLETE original body — no shelling out to the CLI.
        let (root, config) = config_over_temp_repo();
        let db_path = &config.database;
        let created = crate::tools::call_tool(
            db_path,
            "memory_create",
            json!({
                "kind": "Invariant",
                "title": "t",
                "body": "FULL-BODY-MARKER expand text",
                "confidence": "high",
                "bind": { "path": "src/lib.rs" }
            }),
        )
        .unwrap();
        // memory_create returns RepoMemoryCreateResult { memory, duplicate } — the id is nested.
        let id = created["memory"]["memory_id"]
            .as_str()
            .expect("created memory carries a memory_id")
            .to_string();

        let shown =
            crate::tools::call_tool(db_path, "memory_show", json!({ "memory_id": id })).unwrap();
        assert_eq!(
            shown["body"].as_str(),
            Some("FULL-BODY-MARKER expand text"),
            "returns the full body"
        );
        assert_eq!(shown["title"].as_str(), Some("t"));

        // An unknown id errors, not a silent null.
        assert!(
            crate::tools::call_tool(db_path, "memory_show", json!({ "memory_id": "mem_nope" }))
                .is_err(),
            "unknown memory id must error",
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
