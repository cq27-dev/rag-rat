use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value};

use super::RagRatService;

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
        // `with_all_items` fills the SEP-2322 / SEP-2549 result fields with their protocol
        // defaults (`result_type: complete`, no TTL, no cache scope). The whole catalog ships in
        // one page, so there is no cursor; rmcp strips `result_type` again for peers that
        // negotiated a protocol version predating the field.
        Ok(ListToolsResult::with_all_items(tools))
    }
}
