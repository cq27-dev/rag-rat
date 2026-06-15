use rag_rat_core::Config;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
#[cfg(not(unix))]
use rmcp::transport::stdio;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde_json::{Map, Value, json};

use crate::tools::{
    BlameChunkArgs, CompareGraphTextArgs, EmptyArgs, HealIndexArgs, ImpactArgs,
    ImportantSymbolsArgs, LimitArgs, MemoryCreateArgs, MemoryForCallPathArgs, MemoryForPathArgs,
    MemoryForSymbolArgs, MemoryIdArgs, MemoryRebindArgs, MemorySearchArgs, MemoryUpdateArgs,
    PapertrailChunkArgs, PapertrailCommitArgs, PathHistoryArgs, ReadChunkArgs, RepoBriefArgs,
    RepoClustersArgs, SearchArgs, SymbolArgs, SymbolGraphArgs,
};

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
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_router]
impl RagRatService {
    #[tool(
        name = "semantic_search",
        description = "Search indexed source and docs with hybrid BM25/vector/structural ranking; \
                       optionally explains score components."
    )]
    fn semantic_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("semantic_search", json!(args))
    }

    #[tool(
        name = "symbol_lookup",
        description = "Find exact or fuzzy Rust, TypeScript, Kotlin, C, or C++ symbols."
    )]
    fn symbol_lookup(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("symbol_lookup", json!(args))
    }

    #[tool(
        name = "find_callers",
        description = "Traverse tree-sitter-derived reverse graph edges for callers."
    )]
    fn find_callers(
        &self,
        Parameters(args): Parameters<SymbolGraphArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("find_callers", json!(args))
    }

    #[tool(
        name = "trace_callees",
        description = "Traverse tree-sitter-derived forward graph edges for callees."
    )]
    fn trace_callees(
        &self,
        Parameters(args): Parameters<SymbolGraphArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("trace_callees", json!(args))
    }

    #[tool(
        name = "compare_graph_to_text",
        description = "Compare graph caller edges for a symbol against regex text hits in indexed \
                       source."
    )]
    fn compare_graph_to_text(
        &self,
        Parameters(args): Parameters<CompareGraphTextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("compare_graph_to_text", json!(args))
    }

    #[tool(
        name = "compare_graph_to_scip",
        description = "Report edges where the tree-sitter graph and the SCIP compiler oracle \
                       disagree on resolution (contradictions). Requires `rag-rat oracle run`."
    )]
    fn compare_graph_to_scip(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("compare_graph_to_scip", json!({}))
    }

    #[tool(
        name = "impact_surface",
        description = "Graph-backed coding preflight with structural, textual fallback, and \
                       papertrail evidence."
    )]
    fn impact_surface(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("impact_surface", json!(args))
    }

    #[tool(
        name = "repo_brief",
        description = "Orientation-first repo brief with spine, churn, god-module, and \
                       refactor-candidate modes."
    )]
    fn repo_brief(
        &self,
        Parameters(args): Parameters<RepoBriefArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("repo_brief", json!(args))
    }

    #[tool(
        name = "repo_clusters",
        description = "Cheap file-level ownership clusters using path proximity, graph edges, and \
                       git co-touches."
    )]
    fn repo_clusters(
        &self,
        Parameters(args): Parameters<RepoClustersArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("repo_clusters", json!(args))
    }

    #[tool(
        name = "important_symbols",
        description = "Rank the most load-bearing symbols by weighted PageRank over the edge \
                       graph — the spine to not reinvent or break. Run before editing."
    )]
    fn important_symbols(
        &self,
        Parameters(args): Parameters<ImportantSymbolsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("important_symbols", json!(args))
    }

    #[tool(
        name = "ffi_surface",
        description = "Find UniFFI/export/generated-binding/call-site candidates."
    )]
    fn ffi_surface(
        &self,
        Parameters(args): Parameters<LimitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("ffi_surface", json!(args))
    }

    #[tool(name = "docs_for_symbol", description = "Find docs chunks related to a symbol.")]
    fn docs_for_symbol(
        &self,
        Parameters(args): Parameters<SymbolGraphArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("docs_for_symbol", json!(args))
    }

    #[tool(
        name = "read_chunk",
        description = "Read current text for one selected chunk ID with anchor validation."
    )]
    fn read_chunk(
        &self,
        Parameters(args): Parameters<ReadChunkArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("read_chunk", json!(args))
    }

    #[tool(
        name = "commit_search",
        description = "Search historical git commit subjects and bodies."
    )]
    fn commit_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("commit_search", json!(args))
    }

    #[tool(
        name = "git_history_for_path",
        description = "Return historical commits that touched one current path."
    )]
    fn git_history_for_path(
        &self,
        Parameters(args): Parameters<PathHistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("git_history_for_path", json!(args))
    }

    #[tool(
        name = "git_history_for_symbol",
        description = "Resolve a current symbol, then return historical commits touching its path."
    )]
    fn git_history_for_symbol(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("git_history_for_symbol", json!(args))
    }

    #[tool(
        name = "commits_touching_query",
        description = "Combine commit-message and current file-change evidence for a query."
    )]
    fn commits_touching_query(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("commits_touching_query", json!(args))
    }

    #[tool(
        name = "git_blame_chunk",
        description = "Compute lazy hash-bound git blame summary for one current chunk."
    )]
    fn git_blame_chunk(
        &self,
        Parameters(args): Parameters<BlameChunkArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("git_blame_chunk", json!(args))
    }

    #[tool(
        name = "papertrail_for_chunk",
        description = "Return current chunk context plus cached GitHub rationale."
    )]
    fn papertrail_for_chunk(
        &self,
        Parameters(args): Parameters<PapertrailChunkArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("papertrail_for_chunk", json!(args))
    }

    #[tool(
        name = "papertrail_for_symbol",
        description = "Return current symbol context plus cached GitHub rationale."
    )]
    fn papertrail_for_symbol(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("papertrail_for_symbol", json!(args))
    }

    #[tool(
        name = "papertrail_for_commit",
        description = "Return cached GitHub rationale related to a historical commit."
    )]
    fn papertrail_for_commit(
        &self,
        Parameters(args): Parameters<PapertrailCommitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("papertrail_for_commit", json!(args))
    }

    #[tool(name = "github_issue_search", description = "Search cached GitHub issue and PR text.")]
    fn github_issue_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("github_issue_search", json!(args))
    }

    #[tool(
        name = "github_refs_for_path",
        description = "List discovered GitHub references for one current path."
    )]
    fn github_refs_for_path(
        &self,
        Parameters(args): Parameters<PathHistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("github_refs_for_path", json!(args))
    }

    #[tool(name = "rationale_search", description = "Search cached GitHub rationale snippets.")]
    fn rationale_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("rationale_search", json!(args))
    }

    #[tool(
        name = "local_ai_status",
        description = "Report explicit local AI capability and artifact status."
    )]
    fn local_ai_status(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("local_ai_status", json!({}))
    }

    #[tool(
        name = "heal_index",
        description = "Repair stale already-indexed files and refresh SQLite FTS."
    )]
    fn heal_index(
        &self,
        Parameters(args): Parameters<HealIndexArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("heal_index", json!(args))
    }

    #[tool(
        name = "github_sync_status",
        description = "Report local GitHub papertrail cache status."
    )]
    fn github_sync_status(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("github_sync_status", json!({}))
    }

    #[tool(
        name = "index_status",
        description = "Report SQLite index freshness, git metadata, parser failures, and file \
                       counts."
    )]
    fn index_status(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("index_status", json!({}))
    }

    #[tool(
        name = "memory_create",
        description = "Create a source-anchored repo memory bound to a symbol, chunk, path, \
                       commit, or GitHub ref."
    )]
    fn memory_create(
        &self,
        Parameters(args): Parameters<MemoryCreateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("memory_create", json!(args))
    }

    #[tool(
        name = "memory_rebind",
        description = "Re-anchor an existing repo memory to a different symbol, chunk, path, or \
                       other source location after it moved or was renamed."
    )]
    fn memory_rebind(
        &self,
        Parameters(args): Parameters<MemoryRebindArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("memory_rebind", json!(args))
    }

    #[tool(
        name = "memory_update",
        description = "Update typed repo-memory text, status, confidence, kind, or tags."
    )]
    fn memory_update(
        &self,
        Parameters(args): Parameters<MemoryUpdateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("memory_update", json!(args))
    }

    #[tool(
        name = "memory_search",
        description = "Search active or stale repo memories with deterministic FTS recall."
    )]
    fn memory_search(
        &self,
        Parameters(args): Parameters<MemorySearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("memory_search", json!(args))
    }

    #[tool(
        name = "memory_for_symbol",
        description = "Return repo memories bound to a selected symbol or logical symbol."
    )]
    fn memory_for_symbol(
        &self,
        Parameters(args): Parameters<MemoryForSymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("memory_for_symbol", json!(args))
    }

    #[tool(name = "memory_for_path", description = "Return repo memories bound to one path.")]
    fn memory_for_path(
        &self,
        Parameters(args): Parameters<MemoryForPathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("memory_for_path", json!(args))
    }

    #[tool(
        name = "memory_for_call_path",
        description = "Return repo memories bound to one call-path edge sequence hash."
    )]
    fn memory_for_call_path(
        &self,
        Parameters(args): Parameters<MemoryForCallPathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("memory_for_call_path", json!(args))
    }

    #[tool(
        name = "memory_validate",
        description = "Validate repo-memory anchors and mark current, relocated, stale, or gone."
    )]
    fn memory_validate(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("memory_validate", json!({}))
    }

    #[tool(
        name = "memory_doctor",
        description = "List repo memories with stale/gone anchors plus suggested re-anchor \
                       targets — the actionable companion to memory_validate. Rebind them with \
                       memory_rebind."
    )]
    fn memory_doctor(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("memory_doctor", json!({}))
    }

    #[tool(
        name = "memory_mark_obsolete",
        description = "Mark a repo memory obsolete without deleting its audit trail."
    )]
    fn memory_mark_obsolete(
        &self,
        Parameters(args): Parameters<MemoryIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("memory_mark_obsolete", json!(args))
    }
}

#[tool_handler]
impl ServerHandler for RagRatService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rag-rat", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Read-only-source repo intelligence. Index and auto-heal writes are confined to \
                 the configured SQLite database.",
            )
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
        let peer_info = running.peer().peer_info().cloned().unwrap_or_default();
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
    use crate::tools::EmptyArgs;

    static N: AtomicU64 = AtomicU64::new(0);

    fn service_over_temp_repo() -> (PathBuf, RagRatService) {
        let root = std::env::temp_dir().join(format!(
            "rag-rat-server-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn open_database() {}\n").unwrap();
        let config = Config {
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
            local_ai: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
        };
        IndexDatabase::rebuild(&config).unwrap();
        (root, RagRatService::new(config, OutputFormat::Toon))
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
            ("local_ai_status", json!({})),
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
    fn tool_wrappers_forward_to_call() {
        // The #[tool] methods are thin forwarders to `call()`; exercise a representative set across
        // the distinct arg shapes so the forwarding path is covered in-process (the stdio test runs
        // the server out-of-process, where coverage isn't collected).
        let (root, svc) = service_over_temp_repo();
        // Each method takes a distinct Parameters<T>, so the args are built inline per call
        // (a shared closure would monomorphize to a single T).
        let sym = json!({ "symbol": "open_database" });
        svc.semantic_search(Parameters(
            serde_json::from_value(json!({ "query": "open_database" })).unwrap(),
        ))
        .unwrap();
        svc.symbol_lookup(Parameters(serde_json::from_value(sym.clone()).unwrap())).unwrap();
        svc.find_callers(Parameters(serde_json::from_value(sym.clone()).unwrap())).unwrap();
        svc.impact_surface(Parameters(serde_json::from_value(sym.clone()).unwrap())).unwrap();
        svc.repo_brief(Parameters(serde_json::from_value(json!({})).unwrap())).unwrap();
        svc.important_symbols(Parameters(serde_json::from_value(json!({})).unwrap())).unwrap();
        svc.index_status(Parameters(EmptyArgs {})).unwrap();
        svc.local_ai_status(Parameters(EmptyArgs {})).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
