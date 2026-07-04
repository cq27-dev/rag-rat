//! The verdict-model abstraction for the dream v2 pass-1 (model verdict) pass — rag-rat's first
//! generative-model dependency (#122). Out-of-process only: [`VerdictModel`] is a one-turn
//! completion over an OpenAI-compatible chat endpoint (a local Ollama by default), and the
//! deterministic layer (`verify` / `verdict`) is what gates it. The trait keeps the pass testable
//! without a network — every test drives a mock, never a socket.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::RemoteDreamConfig;

/// A single-turn verdict model: given a fully-rendered prompt, return the model's raw completion
/// text. Object-safe so the verdict pass takes a `&dyn VerdictModel` and a test can swap in a mock.
pub trait VerdictModel {
    /// One completion for `prompt` (temperature 0, no streaming). Returns the raw text; parsing +
    /// the citation guard live in `verdict`, never here.
    fn complete(&self, prompt: &str) -> anyhow::Result<String>;

    /// The model identifier stamped into `memory_reality.model_id` (e.g. `qwen3:4b-instruct`),
    /// so a verdict row records which model produced it.
    fn model_id(&self) -> &str;
}

/// Chat-completion request body for `POST /v1/chat/completions`. `temperature: 0` +
/// `stream: false` are the verdict-pass contract — deterministic, single-shot.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage<'a>],
    temperature: f32,
    stream: bool,
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

/// The HTTP verdict model: a blocking `ureq` client against an OpenAI-compatible
/// `/v1/chat/completions` route (ollama/vLLM/any compatible server). Mirrors the embedder client's
/// transport posture ([`crate::index::ai::providers::openai`]) — one place to audit, loopback
/// proxy bypass, `http_status_as_error(false)` so the server's JSON error body survives.
#[derive(Debug)]
pub struct HttpVerdictModel {
    agent: ureq::Agent,
    /// `<endpoint>/v1/chat/completions`, precomputed once at construction.
    chat_url: String,
    /// The server-side model name sent in the request body (`[llm.dream.remote] model`).
    model: String,
    /// Bearer token sent as `Authorization: Bearer <token>` when present — the value of the
    /// `[llm.dream.remote] auth_env` variable (connect mode) or the ephemeral box's tunnel token
    /// (`from_provisioned`). `None` for a local server that needs no auth.
    auth_token: Option<String>,
}

impl HttpVerdictModel {
    /// Build the verdict client from a CONNECT-mode `[llm.dream.remote]`. The endpoint is trimmed
    /// and the chat route is appended; loopback endpoints bypass the ambient HTTP proxy (a local
    /// Ollama routed through a corporate proxy 403s), matching the embedder client. `endpoint` is
    /// optional on the config, so an absent value falls back to the local-Ollama default (the same
    /// default `RemoteDreamConfig::default` carries). A configured `auth_env` names the environment
    /// variable holding the bearer token, read once here.
    pub fn from_config(cfg: &RemoteDreamConfig) -> Self {
        let endpoint = cfg
            .endpoint
            .as_deref()
            .unwrap_or("http://localhost:11434")
            .trim()
            .trim_end_matches('/');
        let auth_token = cfg
            .auth_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        Self::build(endpoint, cfg.model.trim(), auth_token, cfg.request_timeout_s)
    }

    /// Build the verdict client against an EPHEMERAL box just provisioned by the cookbook: the
    /// tunnel `endpoint` + `auth_token` come from the `ready` handshake (NOT config), the model +
    /// per-request timeout from `[llm.dream.remote]`. The box's tunnel is a public HTTPS URL (not
    /// loopback), so the proxy bypass does not apply. Pair with [`provision_verdict_model`], which
    /// keeps the `ProvisionedBox` alive for the model's lifetime.
    pub fn from_provisioned(
        endpoint: &str,
        auth_token: Option<&str>,
        model: &str,
        request_timeout_s: u64,
    ) -> Self {
        let auth_token = auth_token.map(str::to_string).filter(|t| !t.is_empty());
        Self::build(
            endpoint.trim().trim_end_matches('/'),
            model.trim(),
            auth_token,
            request_timeout_s,
        )
    }

    /// Shared constructor for both modes: precompute the chat URL, build the `ureq` agent (loopback
    /// proxy bypass), and store the bearer token.
    fn build(
        endpoint: &str,
        model: &str,
        auth_token: Option<String>,
        request_timeout_s: u64,
    ) -> Self {
        let chat_url = format!("{endpoint}/v1/chat/completions");
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(request_timeout_s)))
            .http_status_as_error(false)
            .user_agent(concat!("rag-rat/", env!("CARGO_PKG_VERSION"), " (dream-verdict)"));
        if endpoint_is_loopback(endpoint) {
            builder = builder.proxy(None);
        }
        let agent: ureq::Agent = builder.build().into();
        Self { agent, chat_url, model: model.to_string(), auth_token }
    }
}

impl VerdictModel for HttpVerdictModel {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn complete(&self, prompt: &str) -> anyhow::Result<String> {
        let messages = [ChatMessage { role: "user", content: prompt }];
        let payload = ChatRequest {
            model: &self.model,
            messages: &messages,
            temperature: 0.0,
            stream: false,
        };
        // The `json` ureq feature is not enabled (workspace ureq is rustls-only), so serialize the
        // body ourselves and send it with an explicit content-type — same as the embedder client.
        let body = serde_json::to_vec(&payload)
            .map_err(|e| anyhow::anyhow!("failed to serialize verdict request: {e}"))?;

        let mut request = self.agent.post(&self.chat_url).content_type("application/json");
        if let Some(token) = &self.auth_token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let mut response = request
            .send(body)
            .map_err(|e| anyhow::anyhow!("verdict request to `{}` failed: {e}", self.chat_url))?;
        let status = response.status();
        let raw = response
            .body_mut()
            .read_to_string()
            .map_err(|e| anyhow::anyhow!("reading verdict response failed: {e}"))?;
        if !status.is_success() {
            // OpenAI-compatible servers usually return `{"error":{"message":...}}`; surface that
            // clean message when present, else a bounded raw excerpt.
            let detail = serde_json::from_str::<ErrorResponse>(&raw)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| response_excerpt(&raw));
            anyhow::bail!(
                "verdict request to `{}` failed: http status {}: {}",
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

/// Provision an ephemeral cookbook box serving the dream CHAT model and build a verdict client
/// against it, reusing the embedding path's provisioning driver + leak-safety wholesale (only the
/// `capability` + backend differ). The returned [`ProvisionedBox`] MUST be kept alive for as long
/// as the model is used — its `Drop` tears the box down. Ephemeral-only: the caller checks
/// `remote.is_ephemeral()` (and the zero-work guard) BEFORE calling, so we never cold-start a paid
/// box for nothing.
pub fn provision_verdict_model(
    remote: &RemoteDreamConfig,
) -> anyhow::Result<(HttpVerdictModel, crate::index::ai::ProvisionedBox)> {
    use crate::index::ai::{CookbookInput, CookbookProvisioner};

    let cookbook = remote.cookbook.as_deref().ok_or_else(|| {
        anyhow::anyhow!("provision_verdict_model called on a non-ephemeral `[llm.dream.remote]`")
    })?;
    let input = CookbookInput {
        model: remote.model.trim().to_string(),
        backend: remote.backend.as_db_str(),
        capability: "chat",
        request_timeout_s: remote.request_timeout_s,
        // Give the recipe a provisioning budget just under the Rust hard ceiling (backend-aware —
        // vLLM's large image needs longer), so ITS budget expires first (clean provider teardown)
        // before the Rust SIGKILL backstop fires. Mirrors the embedding `cookbook_input_for`.
        provision_timeout_s: remote.backend.provision_timeout().as_secs().saturating_sub(20),
        gpu: remote.gpu.clone(),
        // Chat serving ignores these — num_ctx is an ollama-embedding knob, and the
        // verdict/compaction passes call the model sequentially (one turn per memory), so a
        // single server slot suffices.
        num_ctx: None,
        server_concurrency: 1,
    };
    let provisioned = CookbookProvisioner::provision(cookbook, &input)?;
    let model = HttpVerdictModel::from_provisioned(
        &provisioned.endpoint,
        provisioned.auth_token.as_deref(),
        remote.model.trim(),
        remote.request_timeout_s,
    );
    Ok((model, provisioned))
}

fn response_excerpt(body: &str) -> String {
    let trimmed = body.trim();
    let mut excerpt = trimmed.chars().take(500).collect::<String>();
    if trimmed.chars().count() > 500 {
        excerpt.push_str("...");
    }
    excerpt
}

/// Whether the endpoint's host is loopback (`127.0.0.1`, `localhost`, `::1`) — those bypass the
/// ambient HTTP proxy. Same parse as the embedder client's helper.
fn endpoint_is_loopback(endpoint: &str) -> bool {
    let after_scheme = endpoint.split_once("://").map_or(endpoint, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or(after_scheme);
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = match host_port.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => host_port.split(':').next().unwrap_or(host_port),
    };
    matches!(host.trim().to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1")
        || host.starts_with("127.")
}

#[cfg(test)]
pub(super) mod mock {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::VerdictModel;

    /// A network-free [`VerdictModel`] for the dream tests: it hands back canned completions in
    /// order and counts how many times it was called (so a churn-skip test can assert the model was
    /// NOT re-invoked). When the queue drains it repeats the last response, or errors if it was
    /// never given one.
    pub(in crate::dream) struct MockVerdictModel {
        responses: Mutex<VecDeque<String>>,
        last: Mutex<Option<String>>,
        calls: AtomicUsize,
    }

    impl MockVerdictModel {
        /// A mock that returns `responses` in order (then repeats the last one).
        pub(in crate::dream) fn new<I, S>(responses: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: Into<String>,
        {
            Self {
                responses: Mutex::new(responses.into_iter().map(Into::into).collect()),
                last: Mutex::new(None),
                calls: AtomicUsize::new(0),
            }
        }

        /// How many times `complete` has been called — the churn-skip assertion hook.
        pub(in crate::dream) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl VerdictModel for MockVerdictModel {
        fn model_id(&self) -> &str {
            "mock-verdict-model"
        }

        fn complete(&self, _prompt: &str) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(next) = self.responses.lock().unwrap().pop_front() {
                *self.last.lock().unwrap() = Some(next.clone());
                return Ok(next);
            }
            self.last
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow::anyhow!("mock verdict model was given no responses"))
        }
    }

    #[test]
    fn mock_returns_queued_responses_then_repeats_last_and_counts_calls() {
        let m = MockVerdictModel::new(["first", "second"]);
        assert_eq!(m.complete("p").unwrap(), "first");
        assert_eq!(m.complete("p").unwrap(), "second");
        assert_eq!(m.complete("p").unwrap(), "second", "drained queue repeats the last response");
        assert_eq!(m.calls(), 3);
    }

    #[test]
    fn mock_without_responses_errors() {
        let m = MockVerdictModel::new(Vec::<String>::new());
        assert!(m.complete("p").is_err(), "a mock given no responses errors rather than panicking");
    }
}
