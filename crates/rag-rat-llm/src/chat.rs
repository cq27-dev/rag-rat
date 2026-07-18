//! The single-turn chat client for the rag-rat workspace: a one-turn completion over an
//! OpenAI-compatible chat endpoint (a local Ollama by default), shared by the dream verdict/compact
//! passes (#122) and the distill LLM pass (#704). [`ChatModel`] is object-safe so callers take a
//! `&dyn ChatModel` and a test can swap in a mock, never a socket. The client is deliberately
//! logic-free: prompt rendering, parsing, and any citation/output guard live in the caller (dream's
//! `verdict`/`compact`, distill's ladder) — this module only speaks HTTP.
//!
//! Guided decoding: [`ChatModel::complete_guided`] can request structured JSON output via the
//! OpenAI `response_format: {type: json_schema}` selector. We always talk to the server's
//! OpenAI-compatible `/v1/chat/completions` route (both vLLM and Ollama expose one), so the same
//! selector constrains output on every backend; a server that cannot honor it degrades to an
//! unguided completion (the caller's output ladder retries unguided) rather than failing.

use std::time::Duration;

use rag_rat_base::config::RemoteDreamConfig;
use serde::{Deserialize, Serialize};

use crate::openai::{endpoint_is_loopback, resolve_auth_header};

/// A single-turn chat model: given a fully-rendered prompt, return the model's raw completion text.
/// Object-safe so a pass takes a `&dyn ChatModel` and a test can swap in a mock.
pub trait ChatModel {
    /// One completion for `prompt` (temperature 0, no streaming), optionally requesting
    /// backend-native structured output via `guided`. Returns the raw text; parsing + any output
    /// guard live in the caller, never here. This is the single required method so an implementor
    /// (real client or mock) covers both the guided and unguided paths in one place.
    fn complete_guided(
        &self,
        prompt: &str,
        guided: Option<GuidedJson<'_>>,
    ) -> anyhow::Result<String>;

    /// One UNGUIDED completion — the common case. Defaults to `complete_guided(prompt, None)` so
    /// existing callers (the dream text-marker passes) stay unchanged.
    fn complete(&self, prompt: &str) -> anyhow::Result<String> {
        self.complete_guided(prompt, None)
    }

    /// The model identifier stamped into a produced row (e.g. `qwen3:4b-instruct`), so a record
    /// records which model produced it.
    fn model_id(&self) -> &str;
}

/// A guided-decoding request: a named JSON schema the server must constrain output to. Borrows the
/// schema `Value` so the caller keeps ownership (the distill pass emits it once from its record
/// type). The `name` labels the schema in the `response_format` selector.
#[derive(Debug, Clone, Copy)]
pub struct GuidedJson<'a> {
    pub name: &'a str,
    pub schema: &'a serde_json::Value,
}

/// Chat-completion request body for `POST /v1/chat/completions`. `temperature: 0` + `stream: false`
/// are the contract — deterministic, single-shot. `response_format` requests structured output; it
/// is omitted for an unguided call (`skip_serializing_if`). We always talk to the server's
/// OpenAI-compatible `/v1/chat/completions` route (both vLLM and Ollama expose one), so the OpenAI
/// `response_format` is the guided selector for every backend — Ollama's native `format` parameter
/// belongs to its `/api/chat` route, which we never call, so sending it here would be silently
/// ignored and leave output unconstrained.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage<'a>],
    temperature: f32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat<'a>>,
}

/// The OpenAI structured-output selector (vLLM + Ollama's OpenAI-compatible route):
/// `{"type":"json_schema","json_schema":{name,schema}}`.
#[derive(Serialize)]
struct ResponseFormat<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    json_schema: JsonSchemaSpec<'a>,
}

#[derive(Serialize)]
struct JsonSchemaSpec<'a> {
    name: &'a str,
    schema: &'a serde_json::Value,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Success response body: `{ "choices": [ { "message": { "content": "..." } } ] }`.
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: String,
}

/// Error body most OpenAI-compatible servers return on non-2xx (`{ "error": { "message" } }`).
#[derive(Deserialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
}

/// The HTTP chat model: a blocking `ureq` client against an OpenAI-compatible
/// `/v1/chat/completions` route (ollama/vLLM/any compatible server). Mirrors the embedder client's
/// transport posture ([`crate::openai`]) — one place to audit, loopback proxy bypass,
/// `http_status_as_error(false)` so the server's JSON error body survives.
#[derive(Debug)]
pub struct HttpChatModel {
    agent: ureq::Agent,
    /// `<endpoint>/v1/chat/completions`, precomputed once at construction.
    chat_url: String,
    /// The server-side model name sent in the request body.
    model: String,
    /// The full `Authorization` header value (`"Bearer <token>"`) sent when the server needs auth
    /// — from the config's `auth_env` variable (connect) or the ephemeral box's tunnel token
    /// (`from_provisioned`). `None` for a local server that needs no auth.
    auth_header: Option<String>,
}

impl HttpChatModel {
    /// Build the chat client from a CONNECT-mode remote config. The endpoint is trimmed and the
    /// chat route is appended; loopback endpoints bypass the ambient HTTP proxy (a local Ollama
    /// routed through a corporate proxy 403s), matching the embedder client. `endpoint` is
    /// optional on the config, so an absent value falls back to the local-Ollama default. A
    /// configured `auth_env` names the environment variable holding the bearer token; a
    /// NAMED-but-unset/empty var is a CONFIG ERROR (the same `resolve_auth_header` the embedder
    /// uses), so this is fallible — an authenticated endpoint must fail fast at startup, not
    /// send unauthenticated requests that 401 later.
    pub fn from_config(cfg: &RemoteDreamConfig) -> anyhow::Result<Self> {
        let endpoint = cfg
            .endpoint
            .as_deref()
            .unwrap_or("http://localhost:11434")
            .trim()
            .trim_end_matches('/');
        let auth_header =
            resolve_auth_header(cfg.auth_env.as_deref(), |var| std::env::var(var).ok())?;
        Ok(Self::build(endpoint, cfg.model.trim(), auth_header, cfg.request_timeout_s))
    }

    /// Build the chat client against an EPHEMERAL box just provisioned by the cookbook: the tunnel
    /// `endpoint` and `auth_token` come from the `ready` handshake (NOT config), while the model
    /// and per-request timeout come from the remote config. The box's tunnel is a public HTTPS
    /// URL (not loopback), so the proxy bypass does not apply. The `auth_token` here is a
    /// DIRECT credential (not an env-var name), so it is wrapped into the header as-is. Pair
    /// with [`provision_chat_model`], which keeps the `ProvisionedBox` alive for the model's
    /// lifetime.
    pub fn from_provisioned(params: ProvisionedChatParams<'_>) -> Self {
        let auth_header = params
            .auth_token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| format!("Bearer {t}"));
        Self::build(
            params.endpoint.trim().trim_end_matches('/'),
            params.model.trim(),
            auth_header,
            params.request_timeout_s,
        )
    }

    /// Shared constructor for both modes: precompute the chat URL, build the `ureq` agent (loopback
    /// proxy bypass), and store the resolved `Authorization` header.
    fn build(
        endpoint: &str,
        model: &str,
        auth_header: Option<String>,
        request_timeout_s: u64,
    ) -> Self {
        let chat_url = format!("{endpoint}/v1/chat/completions");
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(request_timeout_s)))
            .http_status_as_error(false)
            .user_agent(concat!("rag-rat/", env!("CARGO_PKG_VERSION"), " (chat)"));
        if endpoint_is_loopback(endpoint) {
            builder = builder.proxy(None);
        }
        let agent: ureq::Agent = builder.build().into();
        Self { agent, chat_url, model: model.to_string(), auth_header }
    }

    /// The `response_format` selector for a guided request (`None` for an unguided call). We always
    /// hit the OpenAI-compatible route, so the same `json_schema` selector constrains output on
    /// every chat backend; a server that cannot honor it 400s or ignores it, and the caller's
    /// output ladder recovers with an unguided retry.
    fn guided_response_format<'a>(guided: Option<GuidedJson<'a>>) -> Option<ResponseFormat<'a>> {
        guided.map(|g| ResponseFormat {
            kind: "json_schema",
            json_schema: JsonSchemaSpec { name: g.name, schema: g.schema },
        })
    }
}

impl ChatModel for HttpChatModel {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn complete_guided(
        &self,
        prompt: &str,
        guided: Option<GuidedJson<'_>>,
    ) -> anyhow::Result<String> {
        let messages = [ChatMessage { role: "user", content: prompt }];
        let payload = ChatRequest {
            model: &self.model,
            messages: &messages,
            temperature: 0.0,
            stream: false,
            response_format: Self::guided_response_format(guided),
        };
        // The `json` ureq feature is not enabled (workspace ureq is rustls-only), so serialize the
        // body ourselves and send it with an explicit content-type — same as the embedder client.
        let body = serde_json::to_vec(&payload)
            .map_err(|e| anyhow::anyhow!("failed to serialize chat request: {e}"))?;

        let mut request = self.agent.post(&self.chat_url).content_type("application/json");
        if let Some(header) = &self.auth_header {
            request = request.header("authorization", header.as_str());
        }
        let mut response = request
            .send(body)
            .map_err(|e| anyhow::anyhow!("chat request to `{}` failed: {e}", self.chat_url))?;
        let status = response.status();
        let raw = response
            .body_mut()
            .read_to_string()
            .map_err(|e| anyhow::anyhow!("reading chat response failed: {e}"))?;
        if !status.is_success() {
            // OpenAI-compatible servers usually return `{"error":{"message":...}}`; surface that
            // clean message when present, else a bounded raw excerpt.
            let detail = serde_json::from_str::<ErrorResponse>(&raw)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| response_excerpt(&raw));
            anyhow::bail!(
                "chat request to `{}` failed: http status {}: {}",
                self.chat_url,
                status.as_u16(),
                detail
            );
        }
        let parsed: ChatResponse = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("malformed chat completion response: {e}"))?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow::anyhow!("chat completion response carried no choices"))
    }
}

/// Constructor params for [`HttpChatModel::from_provisioned`] — grouped so the ephemeral-box call
/// site reads by field rather than a positional argument train.
#[derive(Debug, Clone, Copy)]
pub struct ProvisionedChatParams<'a> {
    pub endpoint: &'a str,
    pub auth_token: Option<&'a str>,
    pub model: &'a str,
    pub request_timeout_s: u64,
}

/// Provision an ephemeral cookbook box serving a CHAT model and build a client against it, reusing
/// the embedding path's provisioning driver + leak-safety wholesale (only the `capability` +
/// backend differ). The returned [`crate::ProvisionedBox`] MUST be kept alive for as long as the
/// model is used — its `Drop` tears the box down. Ephemeral-only: the caller checks
/// `remote.is_ephemeral()` (and the zero-work guard) BEFORE calling, so we never cold-start a paid
/// box for nothing.
pub fn provision_chat_model(
    remote: &RemoteDreamConfig,
) -> anyhow::Result<(HttpChatModel, crate::ProvisionedBox)> {
    use crate::CookbookProvisioner;

    let cookbook = remote.cookbook.as_deref().ok_or_else(|| {
        anyhow::anyhow!("provision_chat_model called on a non-ephemeral remote chat config")
    })?;
    let provisioned = CookbookProvisioner::provision(cookbook, &chat_cookbook_input(remote))?;
    let model = HttpChatModel::from_provisioned(ProvisionedChatParams {
        endpoint: &provisioned.endpoint,
        auth_token: provisioned.auth_token.as_deref(),
        model: remote.model.trim(),
        request_timeout_s: remote.request_timeout_s,
    });
    Ok((model, provisioned))
}

/// The remote-config → chat [`crate::CookbookInput`] mapping — the pure, unit-testable half of
/// [`provision_chat_model`], mirroring the embedding `cookbook_input_for`. `capability` is pinned
/// `"chat"`; the provisioning budget sits just under the Rust hard ceiling (backend-aware — vLLM's
/// large image needs longer) so the recipe's own budget expires first (clean provider teardown)
/// before the Rust SIGKILL backstop fires. Chat serving ignores `num_ctx` (an ollama-embedding
/// knob) and needs only ONE server slot — the callers invoke the model sequentially.
fn chat_cookbook_input(remote: &RemoteDreamConfig) -> crate::CookbookInput {
    crate::CookbookInput {
        model: remote.model.trim().to_string(),
        backend: remote.backend.as_db_str(),
        capability: "chat",
        request_timeout_s: remote.request_timeout_s,
        provision_timeout_s: remote
            .resolved_provision_timeout()
            .as_secs()
            .saturating_sub(crate::cookbook::PROVISION_TEARDOWN_MARGIN_SECS),
        gpu: remote.gpu.clone(),
        num_ctx: None,
        server_concurrency: 1,
    }
}

fn response_excerpt(body: &str) -> String {
    let trimmed = body.trim();
    let mut excerpt = trimmed.chars().take(500).collect::<String>();
    if trimmed.chars().count() > 500 {
        excerpt.push_str("...");
    }
    excerpt
}

#[cfg(test)]
mod http_tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use rag_rat_base::config::{RemoteBackend, RemoteDreamConfig};

    use super::{
        ChatModel, GuidedJson, HttpChatModel, ProvisionedChatParams, chat_cookbook_input,
        provision_chat_model,
    };

    /// A one-shot HTTP/1.1 server on `127.0.0.1:0`: accepts one connection, reads the full request
    /// (headers + Content-Length body), replies `status`/`response_body`, and returns the RAW
    /// captured request so a test can assert the sent body + headers. Base URL + join handle.
    fn one_shot(
        status: &'static str,
        response_body: String,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return String::new();
            };
            let mut data = Vec::new();
            let mut buf = [0u8; 2048];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&data);
                if let Some(hdr_end) = text.find("\r\n\r\n") {
                    let content_len = text[..hdr_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if data.len() >= hdr_end + 4 + content_len {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            String::from_utf8_lossy(&data).into_owned()
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn provisioned(url: &str, model: &str) -> HttpChatModel {
        HttpChatModel::from_provisioned(ProvisionedChatParams {
            endpoint: url,
            auth_token: Some("TOK123"),
            model,
            request_timeout_s: 10,
        })
    }

    #[test]
    fn complete_sends_the_chat_body_and_bearer_and_parses_the_content() {
        let (url, handle) = one_shot(
            "200 OK",
            r#"{"choices":[{"message":{"content":"VERDICT: current\nREASON: ok"}}]}"#.to_string(),
        );
        let model = provisioned(&url, "Qwen/Qwen3-8B");
        let out = model.complete("audit this note").unwrap();
        assert_eq!(out, "VERDICT: current\nREASON: ok");

        let req = handle.join().unwrap();
        assert!(
            req.to_ascii_lowercase().contains("authorization: bearer tok123"),
            "the bearer token is sent: {req}"
        );
        assert!(req.contains("/v1/chat/completions"), "chat route: {req}");
        assert!(req.contains("\"model\":\"Qwen/Qwen3-8B\""), "model in body: {req}");
        assert!(req.contains("audit this note"), "prompt in body: {req}");
        assert!(req.contains("\"temperature\":0"), "temperature 0: {req}");
        // An unguided call carries no structured-output selector.
        assert!(!req.contains("response_format"), "unguided: no response_format: {req}");
    }

    #[test]
    fn guided_call_sends_response_format_json_schema() {
        let (url, handle) =
            one_shot("200 OK", r#"{"choices":[{"message":{"content":"{}"}}]}"#.to_string());
        let model = provisioned(&url, "m");
        let schema = serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}});
        let out = model
            .complete_guided("go", Some(GuidedJson { name: "record", schema: &schema }))
            .unwrap();
        assert_eq!(out, "{}");
        let req = handle.join().unwrap();
        assert!(req.contains("\"response_format\""), "guided uses response_format: {req}");
        assert!(req.contains("\"json_schema\""), "carries the json_schema wrapper: {req}");
        assert!(req.contains("\"name\":\"record\""), "schema name: {req}");
    }

    #[test]
    fn an_ollama_backed_client_also_uses_response_format_not_the_native_format_field() {
        // We always POST to the OpenAI-compatible `/v1/chat/completions`, so even an Ollama-backed
        // client must send structured output via `response_format` — Ollama's native `format`
        // parameter belongs to `/api/chat`, which we never call, and would be silently ignored.
        let (url, handle) =
            one_shot("200 OK", r#"{"choices":[{"message":{"content":"{}"}}]}"#.to_string());
        let cfg = RemoteDreamConfig {
            backend: RemoteBackend::Ollama,
            endpoint: Some(url),
            model: "qwen3:4b-instruct".to_string(),
            ..RemoteDreamConfig::default()
        };
        let model = HttpChatModel::from_config(&cfg).unwrap();
        let schema = serde_json::json!({"type": "object"});
        model.complete_guided("go", Some(GuidedJson { name: "record", schema: &schema })).unwrap();
        let req = handle.join().unwrap();
        assert!(req.contains("\"response_format\""), "ollama uses response_format too: {req}");
        assert!(!req.contains("\"format\":"), "no native ollama `format` field is sent: {req}");
    }

    #[test]
    fn complete_surfaces_the_server_error_message_on_non_2xx() {
        let (url, handle) = one_shot(
            "500 Internal Server Error",
            r#"{"error":{"message":"maximum context length exceeded"}}"#.to_string(),
        );
        let model = provisioned(&url, "m");
        let err = model.complete("x").unwrap_err().to_string();
        assert!(err.contains("maximum context length exceeded"), "surfaces server error: {err}");
        let _ = handle.join();
    }

    #[test]
    fn from_config_errors_when_auth_env_var_is_unset() {
        // Connect mode naming an env var that does not resolve is a CONFIG error — fail fast rather
        // than send unauthenticated requests. Uses a var name that is not set in the environment.
        let cfg = RemoteDreamConfig {
            auth_env: Some("RAG_RAT_DEFINITELY_UNSET_CHAT_AUTH_VAR".to_string()),
            ..RemoteDreamConfig::default()
        };
        let err = HttpChatModel::from_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("auth env"), "surfaces the unresolved auth var: {err}");
    }

    #[test]
    fn constructors_set_the_model_id() {
        let cfg = RemoteDreamConfig {
            model: "qwen3:4b-instruct".to_string(),
            ..RemoteDreamConfig::default()
        };
        assert_eq!(HttpChatModel::from_config(&cfg).unwrap().model_id(), "qwen3:4b-instruct");
        assert_eq!(
            provisioned("https://box.modal.run", "Qwen/Qwen3-8B").model_id(),
            "Qwen/Qwen3-8B"
        );
    }

    #[test]
    fn provision_chat_model_rejects_a_non_ephemeral_config() {
        // The default is a local-Ollama CONNECT (no cookbook) — provisioning must refuse it rather
        // than attempt to spawn a recipe.
        let err = provision_chat_model(&RemoteDreamConfig::default()).unwrap_err().to_string();
        assert!(err.contains("non-ephemeral"), "{err}");
    }

    #[test]
    fn chat_cookbook_input_pins_chat_and_maps_backend_gpu_and_budget() {
        let remote = RemoteDreamConfig {
            backend: RemoteBackend::Vllm,
            endpoint: None,
            cookbook: Some("@rag-rat/cookbook modal".to_string()),
            model: "Qwen/Qwen3-8B".to_string(),
            gpu: Some("A10G".to_string()),
            request_timeout_s: 900,
            ..RemoteDreamConfig::default()
        };
        let input = chat_cookbook_input(&remote);
        assert_eq!(input.capability, "chat", "provisions a CHAT box");
        assert_eq!(input.backend, "vllm");
        assert_eq!(input.model, "Qwen/Qwen3-8B");
        assert_eq!(input.gpu.as_deref(), Some("A10G"));
        assert_eq!(input.num_ctx, None);
        assert_eq!(input.server_concurrency, 1, "one turn at a time → one server slot");
        assert_eq!(input.request_timeout_s, 900);
        // The provisioning budget sits just under the backend's hard ceiling (clean provider
        // teardown before the Rust SIGKILL backstop).
        assert_eq!(
            input.provision_timeout_s,
            RemoteBackend::Vllm.provision_timeout().as_secs() - 20
        );
    }

    #[test]
    fn chat_cookbook_input_honors_the_provision_timeout_override() {
        // A `provision_timeout_s` override (the distill 30B-box knob) flows into the cookbook boot
        // budget instead of the backend default, minus the same safety margin.
        let remote = RemoteDreamConfig {
            backend: RemoteBackend::Vllm,
            endpoint: None,
            cookbook: Some("@rag-rat/cookbook modal".to_string()),
            model: "Qwen/Qwen3-30B-A3B-Instruct-2507-FP8".to_string(),
            provision_timeout_s: Some(1500),
            ..RemoteDreamConfig::default()
        };
        let input = chat_cookbook_input(&remote);
        assert_eq!(input.provision_timeout_s, 1500 - 20, "override wins over the backend default");
    }
}
