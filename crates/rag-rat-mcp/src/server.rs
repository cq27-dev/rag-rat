use rag_rat_core::Config;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
#[cfg(not(unix))]
use rmcp::transport::stdio;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Map, Value};

#[derive(Clone)]
pub struct RagRatService {
    config: Config,
    /// Output format for tool results. Defaults to TOON (denser for the LLM that reads them); set
    /// to JSON when the server was launched as `rag-rat mcp --json` — MCP has no per-call flag, so
    /// the choice is made once at launch (the CLI `--json` flag flows in via `run_stdio`).
    output_format: rag_rat_core::OutputFormat,
    /// In-flight tool-call counter, observed by the hot-upgrade teardown so it drains at a request
    /// boundary before `exec`. Present only on Unix, where hot-upgrade is supported.
    #[cfg(unix)]
    inflight: std::sync::Arc<crate::upgrade::Inflight>,
}

impl RagRatService {
    pub fn new(config: Config, output_format: rag_rat_core::OutputFormat) -> Self {
        Self {
            config,
            output_format,
            #[cfg(unix)]
            inflight: crate::upgrade::Inflight::new(),
        }
    }

    /// Shared in-flight counter, so the hot-upgrade signal task can wait for tool calls to drain.
    #[cfg(unix)]
    pub fn inflight(&self) -> std::sync::Arc<crate::upgrade::Inflight> {
        std::sync::Arc::clone(&self.inflight)
    }

    fn call(&self, name: &str, value: Value) -> Result<CallToolResult, ErrorData> {
        // All ~34 tools funnel through here; the guard makes every tool call observable to the
        // hot-upgrade drain via one chokepoint instead of 34 handlers.
        #[cfg(unix)]
        let _inflight = self.inflight.guard();
        let value = crate::tools::call_tool_for_config(&self.config, name, value)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        // MCP tool results are text content read by an LLM, so render TOON by default — it is
        // materially denser than JSON on the uniform-row payloads that dominate these tools, and
        // ties JSON on nested ones. `render` falls back to compact JSON on a TOON encode error, so
        // a tool result is never lost. JSON is reachable by launching `rag-rat mcp --json`
        // (the format is chosen once at launch; MCP has no per-call flag).
        let text = rag_rat_core::render(&value, self.output_format);
        let mut content = vec![Content::text(text)];
        if let Some(nudge) = self.stale_memory_nudge() {
            content.push(Content::text(nudge));
        }
        Ok(CallToolResult::success(content))
    }

    /// Surface drifted repo-memory anchors to the AGENT as a second tool-result content block.
    /// Claude Code's agent is pull-based — MCP server notifications (`notifications/message`) reach
    /// the UI/logs but NOT the model's context (anthropics/claude-code#3174) — so a tool result is
    /// the one MCP-native channel that puts an actionable signal in front of the model. The nudge
    /// self-limits: once the agent runs `memory_rebind`, the count drops to 0 and it stops showing.
    /// Best-effort + lock-free (a bare read-only count), so it never slows or fails a tool call.
    ///
    /// Suppressed in `--json` mode: that mode exists for clients that parse the tool text AS JSON
    /// (or concatenate all text blocks), and a prose block would break them. The nudge is
    /// agent-directed prose, meaningful only in the default TOON (LLM-facing) mode.
    fn stale_memory_nudge(&self) -> Option<String> {
        if self.output_format == rag_rat_core::OutputFormat::Json {
            return None;
        }
        let n = rag_rat_core::memory_attention_count(&self.config.database);
        (n > 0).then(|| {
            let noun = if n == 1 { "memory" } else { "memories" };
            format!(
                "rag-rat: {n} active repo {noun} have stale/gone anchors. Call `memory_doctor` to \
                 list them with suggested re-anchor targets, then `memory_rebind` to fix — so \
                 source-anchored memory stays trustworthy for the next agent."
            )
        })
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
        self.call(&request.name, args)
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
    let _hook_listener = AbortOnDrop(crate::claude_hook::spawn_listener(config.clone()));

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

    use rag_rat_core::language::Language;
    use rag_rat_core::{Config, IndexDatabase, OutputFormat, ResolvedTarget, TargetKind};
    use serde_json::json;

    use super::*;

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
        };
        IndexDatabase::rebuild(&config).unwrap();
        (root, config)
    }

    fn service_over_temp_repo() -> (PathBuf, RagRatService) {
        let (root, config) = config_over_temp_repo();
        (root, RagRatService::new(config, OutputFormat::Toon))
    }

    /// The staleness nudge (#160) rides a TOON tool result as a second content block, but is
    /// SUPPRESSED in `--json` mode so JSON-parsing clients aren't broken (Codex #160 review).
    #[test]
    fn stale_memory_nudge_rides_toon_but_not_json() {
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
        // A path binding is created `current`; validation flips the absent path to `gone`.
        toon.call("memory_validate", json!({})).unwrap();

        // TOON (default, LLM-facing): nudge present as a second block.
        let toon_result = toon.call("index_status", json!({})).unwrap();
        assert_eq!(toon_result.content.len(), 2, "TOON result carries the nudge block");

        // JSON mode: suppressed (single parseable block).
        let json_svc = RagRatService::new(config, OutputFormat::Json);
        let json_result = json_svc.call("index_status", json!({})).unwrap();
        assert_eq!(json_result.content.len(), 1, "JSON result omits the prose nudge");

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
            ("github_sync_status", json!({})),
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
