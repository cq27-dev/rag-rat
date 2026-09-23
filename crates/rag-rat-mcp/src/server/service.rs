use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value};

use super::RagRatService;

impl ServerHandler for RagRatService {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
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
    ) -> Result<CallToolResponse, ErrorData> {
        // The catalog (TOOL_NAMES / schema / description) is the single source of truth advertised
        // by `list_tools`, and `call_tool_for_config` (via `call`) is the by-name dispatcher.
        // Forward the raw request arguments straight to the `call()` chokepoint: no per-tool
        // `#[tool]` forwarder and no `Parameters<T>` serialize->deserialize round-trip — the client
        // JSON reaches exactly one deserialize, in `call_tool_with_db`.
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);
        // Every rag-rat tool answers in one round trip: the arguments fully determine the result,
        // so there is nothing to elicit mid-call. Always the `Complete` arm — a multi-round-trip
        // `InputRequired` response would stall an agent waiting on a read-only lookup.
        self.call_async(request.name.to_string(), args).await.map(CallToolResponse::from)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let enabled = crate::tools::enabled_toolsets(self.config.as_ref());
        let tools = crate::tools::TOOL_NAMES
            .iter()
            .filter(|name| crate::tools::is_listed(name, &enabled))
            .map(|name| {
                let input_schema = match crate::tools::schema(name) {
                    Value::Object(map) => map,
                    _ => Map::new(),
                };
                Tool::new((*name).to_string(), crate::tools::description(name), input_schema)
            })
            .collect();
        // `with_all_items` fills the SEP-2322 result field with its protocol default
        // (`result_type: complete`). The whole catalog ships in one page, so there is no cursor;
        // rmcp strips `result_type` again for peers that negotiated a protocol version predating
        // the field.
        let mut result = ListToolsResult::with_all_items(tools);
        // SEP-2549 makes `ttlMs` / `cacheScope` REQUIRED on list results as of protocol
        // 2026-07-28 — strict clients reject a `tools/list` response without them, which bricks
        // the whole server (no tools ever load). Immediately-stale public hints preserve the
        // pre-SEP-2549 behavior (no client caching). Older peers may reject unknown fields, so
        // the hints are gated on the negotiated version, mirroring rmcp's `#[tool_handler]`.
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        if supports_cache_hints {
            result = result.with_ttl_ms(0).with_cache_scope(CacheScope::Public);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::ClientConfig;
    use rmcp::service::serve_directly;
    use rmcp::{ClientHandler, RoleClient};

    use super::*;

    #[derive(Debug, Clone)]
    struct VersionedClient {
        protocol_version: ProtocolVersion,
    }

    impl ClientHandler for VersionedClient {
        fn get_info(&self) -> ClientConfig {
            let mut info = ClientConfig::default();
            info.protocol_version = self.protocol_version.clone();
            info
        }
    }

    /// Drive `tools/list` through a real in-process client/server pair with the protocol version
    /// already agreed, the way the discover lifecycle leaves a peer: 2026-07-28 removed the
    /// `initialize` handshake, so the handler sees the version exactly as it does over stdio.
    async fn list_tools_at(protocol_version: ProtocolVersion) -> ListToolsResult {
        let (server_transport, client_transport) = tokio::io::duplex(1 << 20);
        let service = RagRatService::new_dormant(rag_rat_core::OutputFormat::Json);
        let client_handler = VersionedClient { protocol_version: protocol_version.clone() };
        let mut server_peer_info = ServerConfig::default();
        server_peer_info.protocol_version = protocol_version;
        let server = serve_directly::<RoleServer, _, _, _, _>(
            service,
            server_transport,
            Some(client_handler.get_info()),
        );
        let server = tokio::spawn(async move {
            server.waiting().await?;
            anyhow::Ok(())
        });
        let client = serve_directly::<RoleClient, _, _, _, _>(
            client_handler,
            client_transport,
            Some(server_peer_info.into()),
        );
        let tools = client.list_tools(None).await.expect("tools/list should succeed");
        client.cancel().await.expect("client should cancel");
        server.await.expect("server task should join").expect("server should exit cleanly");
        tools
    }

    /// SEP-2549: strict 2026-07-28 clients reject a `tools/list` response without `ttlMs` /
    /// `cacheScope`, which bricks the whole server — no tools ever load.
    #[tokio::test]
    async fn list_tools_emits_required_cache_hints_for_2026_07_28() {
        let tools = list_tools_at(ProtocolVersion::V_2026_07_28).await;
        assert_eq!(
            (tools.ttl_ms, tools.cache_scope),
            (Some(0), Some(CacheScope::Public)),
            "2026-07-28 peers must receive the required cache hints"
        );
    }

    /// `tools/list` advertises the default surface, not the maintenance toolsets.
    #[tokio::test]
    async fn tools_list_advertises_only_the_default_surface() {
        let names = list_tools_at(ProtocolVersion::V_2025_06_18)
            .await
            .tools
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == "impact_surface"));
        assert!(!names.iter().any(|name| name == "heal_index"), "admin tools are not listed");
        assert!(!names.iter().any(|name| name == "memory_edges"), "graph tools are not listed");
    }

    /// Peers on older protocol versions keep the pre-SEP-2549 wire shape: strict legacy clients
    /// may reject unknown fields.
    #[tokio::test]
    async fn list_tools_omits_cache_hints_for_legacy_peers() {
        let tools = list_tools_at(ProtocolVersion::V_2025_06_18).await;
        assert_eq!(
            (tools.ttl_ms, tools.cache_scope),
            (None, None),
            "legacy peers must keep the pre-SEP-2549 wire shape"
        );
    }
}
