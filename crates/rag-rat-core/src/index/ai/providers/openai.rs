//! The OpenAI-compatible HTTP embedding backend: the ONLY code that connects to a remote embedding
//! server. All HTTP to `/v1/embeddings` lives in [`OpenAiEmbedder::embed_batch`]; everything else
//! (install-time reachability probes, the dispatch in `providers/mod.rs`) reaches the server by
//! constructing and calling one of these. ONE client for every OpenAI-speaking backend — ollama
//! (its `/v1/embeddings` compatibility route), michaelfeil/infinity, and vLLM — so there is one
//! place to audit/secure/retry. The `[remote] backend` selector routes provisioning + the
//! freshness/tune markers, NOT the wire call (identical across backends).
//!
//! Native, blocking HTTP via `ureq` (v3, rustls) — already a non-optional workspace dep (the
//! crates.io version check uses it), so this backend ships unconditionally: no cargo feature, no
//! missing-feature message. The cloneable `ureq::Agent` is held bare (no `Mutex`) and cloned into
//! bounded blocking worker threads for remote request fan-out.

use std::net::ToSocketAddrs;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::Embedder;
use crate::config::RemoteEmbeddingConfig;

/// Request body for the OpenAI-compatible `POST /v1/embeddings`. `input` is an array — the batch
/// endpoint embeds every text in one request. `encoding_format: "float"` is the OpenAI default,
/// sent explicitly so a server that might otherwise emit base64 stays on plain `f32` arrays. NOTE:
/// the OpenAI schema has no `options`/`num_ctx` — context length is a model-load setting on the
/// server (a Modelfile for ollama), not a per-request field (see `RemoteEmbeddingConfig::num_ctx`).
#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
    encoding_format: &'static str,
}

/// One item of the `/v1/embeddings` response `data` array. `index` is the position of this
/// embedding's input WITHIN the request (0-based); a server may return items out of order, so we
/// reorder by it and require the indices to cover exactly `0..len` (see `embed_one_request_with`).
#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
    index: usize,
}

/// Success response body from `POST /v1/embeddings`: `{ "data": [ { embedding, index }, ... ] }`.
#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

/// Error body most OpenAI-compatible servers return on 4xx/5xx (`{ "error": { "message" } }`),
/// parsed on non-2xx for a clearer message than the raw excerpt.
#[derive(Deserialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
}

/// A native HTTP embedder that offloads embedding work to an OpenAI-compatible `/v1/embeddings`
/// server (ollama/infinity/vLLM). The dim is the SELECTED model's registry dim (the parity
/// contract); every response vector is checked against it on each batch.
#[derive(Debug)]
pub struct OpenAiEmbedder {
    agent: ureq::Agent,
    /// `<endpoint>/v1/embeddings`, precomputed once at construction.
    embed_url: String,
    /// The SELECTED model's persisted registry id (e.g. `fastembed-all-minilm-l6-v2`).
    /// `model_id()` returns THIS — chunk_embeddings key by the selected model regardless of
    /// runtime, so the same rows are reused whether the model is embedded locally or via
    /// Ollama (#317 rework). NOT the server-side Ollama model name (that is `server_model`).
    selected_model_id: String,
    /// The Ollama API model name sent in the `/api/embed` request body (`[remote] model`, e.g.
    /// `all-minilm`) — the server's own identifier, NOT the registry id.
    server_model: String,
    dim: usize,
    /// Max texts per `/api/embed` request (`[llm.embedding.remote] batch_size`).
    /// `embed_batch` splits its input into sub-batches of at most this, so a request never
    /// exceeds the configured cap regardless of the reconcile/runtime batch size. Clamped to
    /// `>= 1` at construction so the `chunks()` split can't panic on a misconfigured `0`.
    batch_size: usize,
    /// Max concurrent `/api/embed` requests in one `embed_batch` call.
    concurrency: usize,
    /// Max total input characters per `/v1/embeddings` request.
    max_batch_chars: usize,
    /// `Some("Bearer <token>")` when the server needs auth, read from `auth_env` at construction.
    auth_header: Option<String>,
}

/// Construction params for [`OpenAiEmbedder::from_provisioned`] — groups the handshake outputs
/// (`endpoint`, `auth_token`) with the model identity + transport knobs so the constructor takes
/// one struct instead of positional args.
pub struct ProvisionedEmbedderParams<'a> {
    /// The serving endpoint from the cookbook handshake (`https://...`).
    pub endpoint: &'a str,
    /// The backend's embeddings route appended to `endpoint` (`RemoteBackend::embed_path()` —
    /// `/v1/embeddings` for ollama/vLLM, `/embeddings` for infinity).
    pub embed_path: &'a str,
    /// A DIRECT bearer token from the handshake (NOT an env-var name), or `None` for an open box.
    pub auth_token: Option<&'a str>,
    /// The Ollama API model name sent in the request body (`[remote] model`).
    pub server_model: &'a str,
    /// The SELECTED model's registry id (what `model_id()` returns — chunks key by the model).
    pub selected_model_id: &'a str,
    /// The dim parity contract (the selected model's `spec.dim`).
    pub dim: usize,
    /// Per-request HTTP timeout, seconds.
    pub request_timeout_s: u64,
    /// Max texts per `/api/embed` request.
    pub batch_size: u32,
    /// Max concurrent `/api/embed` requests.
    pub concurrency: u32,
    /// Max total input characters per `/v1/embeddings` request.
    pub max_batch_chars: usize,
}

struct BuildParams<'a> {
    endpoint: &'a str,
    embed_path: &'a str,
    auth_header: Option<String>,
    selected_model_id: &'a str,
    server_model: &'a str,
    dim: usize,
    request_timeout_s: u64,
    batch_size: u32,
    concurrency: u32,
    max_batch_chars: usize,
}

impl OpenAiEmbedder {
    /// Build the embedder for the SELECTED model served over Ollama. `selected_model_id` + `dim`
    /// come from the model the user picked (`model = "sentence-transformers/all-MiniLM-L6-v2"`,
    /// 384): `model_id()` returns that id so chunk_embeddings key by the model regardless of
    /// runtime, and `dim` is the parity contract the embedder checks every response vector
    /// against — the single source of truth for the expected vector length, never re-declared
    /// in config. The `[remote]` block supplies only the transport: endpoint, auth, timeout,
    /// batch, and the SERVER-side model name (`cfg.model`).
    ///
    /// Errors when the endpoint is absent (the use layer requires it in connect mode, but the
    /// construction site refuses to build a half-formed embedder) or when `auth_env` names an env
    /// var that is missing or empty.
    pub fn from_remote_config(
        cfg: &RemoteEmbeddingConfig,
        selected_model_id: &str,
        dim: usize,
    ) -> anyhow::Result<Self> {
        let endpoint =
            cfg.endpoint.as_deref().map(str::trim).filter(|e| !e.is_empty()).ok_or_else(|| {
                anyhow::anyhow!(
                    "remote embedding endpoint is required in connect mode but was not configured \
                     (`[llm.embedding.remote] endpoint`)"
                )
            })?;
        // CONNECT auth comes from the env var NAMED by `auth_env` (the token never enters config).
        let auth_header =
            resolve_auth_header(cfg.auth_env.as_deref(), |var| std::env::var(var).ok())?;
        Ok(Self::build(BuildParams {
            endpoint,
            embed_path: cfg.backend.embed_path(),
            auth_header,
            selected_model_id,
            server_model: cfg.model.trim(),
            dim,
            request_timeout_s: cfg.request_timeout_s,
            batch_size: cfg.batch_size,
            concurrency: cfg.concurrency,
            max_batch_chars: cfg.max_batch_chars,
        }))
    }

    /// Build the embedder against a freshly PROVISIONED ephemeral box (#318) from
    /// [`ProvisionedEmbedderParams`]. The `endpoint` + `auth_token` come from the cookbook
    /// handshake — `auth_token` is a DIRECT bearer token (the box's per-run credential), NOT an
    /// env-var name (contrast [`Self::from_remote_config`], which resolves `auth_env`). The
    /// model identity (`selected_model_id` + `dim`) + transport knobs (server `model`, timeout,
    /// batch) come from the config the same way.
    pub fn from_provisioned(params: ProvisionedEmbedderParams<'_>) -> Self {
        let auth_header = params
            .auth_token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|token| format!("Bearer {token}"));
        Self::build(BuildParams {
            endpoint: params.endpoint.trim(),
            embed_path: params.embed_path,
            auth_header,
            selected_model_id: params.selected_model_id,
            server_model: params.server_model.trim(),
            dim: params.dim,
            request_timeout_s: params.request_timeout_s,
            batch_size: params.batch_size,
            concurrency: params.concurrency,
            max_batch_chars: params.max_batch_chars,
        })
    }

    /// Shared assembler: build the `ureq::Agent` (with the loopback proxy bypass) + the struct.
    /// `endpoint` is already trimmed/validated by the caller.
    fn build(params: BuildParams<'_>) -> Self {
        let BuildParams {
            endpoint,
            embed_path,
            auth_header,
            selected_model_id,
            server_model,
            dim,
            request_timeout_s,
            batch_size,
            concurrency,
            max_batch_chars,
        } = params;
        let embed_url = format!("{}{}", endpoint.trim_end_matches('/'), embed_path);
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(request_timeout_s)))
            .http_status_as_error(false)
            .user_agent(concat!("rag-rat/", env!("CARGO_PKG_VERSION"), " (openai-embed)"));
        // ureq's default config inherits `HTTP_PROXY`/`HTTPS_PROXY` from the env. A local Ollama
        // (`http://127.0.0.1:11434`) routed through a corporate proxy 403s, so disable the proxy for
        // loopback endpoints only — a non-loopback (truly remote) endpoint may legitimately need
        // it.
        if endpoint_is_loopback(endpoint) {
            builder = builder.proxy(None);
        }
        let agent: ureq::Agent = builder.build().into();
        Self {
            agent,
            embed_url,
            selected_model_id: selected_model_id.to_string(),
            server_model: server_model.to_string(),
            dim,
            // Clamp to >= 1: a configured 0 would panic `slice::chunks`; treat it as "one per
            // request" rather than failing construction.
            batch_size: (batch_size as usize).max(1),
            concurrency: RemoteEmbeddingConfig::bounded_concurrency_value(concurrency) as usize,
            max_batch_chars: max_batch_chars.max(1),
            auth_header,
        }
    }

    fn sub_batch_ranges(&self, texts: &[String]) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        let mut start = 0usize;
        let mut chars = 0usize;
        for (idx, text) in texts.iter().enumerate() {
            let text_chars = text.chars().count();
            let count_full = idx.saturating_sub(start) >= self.batch_size;
            let chars_full = idx > start && chars.saturating_add(text_chars) > self.max_batch_chars;
            if count_full || chars_full {
                ranges.push((start, idx));
                start = idx;
                chars = 0;
            }
            chars = chars.saturating_add(text_chars);
        }
        if start < texts.len() {
            ranges.push((start, texts.len()));
        }
        ranges
    }

    fn embed_one_request_with(
        agent: ureq::Agent,
        embed_url: &str,
        server_model: &str,
        dim: usize,
        auth_header: Option<&str>,
        texts: &[String],
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let payload = EmbedRequest { model: server_model, input: texts, encoding_format: "float" };
        // The `json` ureq feature is not enabled (workspace ureq is rustls-only), so serialize the
        // body ourselves and send it with an explicit content-type.
        let body = serde_json::to_vec(&payload)
            .map_err(|e| anyhow::anyhow!("failed to serialize embed request: {e}"))?;

        let mut request = agent.post(embed_url).content_type("application/json");
        if let Some(header) = auth_header {
            request = request.header("Authorization", header);
        }

        // `http_status_as_error(false)` lets us read the server's JSON error body on 4xx/5xx
        // instead of losing the actionable reason behind ureq's bare `http status: N`
        // error.
        let mut response = request
            .send(body)
            .map_err(|e| anyhow::anyhow!("embed request to `{embed_url}` failed: {e}"))?;
        let status = response.status();
        let raw = response
            .body_mut()
            .read_to_string()
            .map_err(|e| anyhow::anyhow!("reading embed response failed: {e}"))?;
        if !status.is_success() {
            // OpenAI-compatible servers usually return `{"error":{"message":...}}`; surface that
            // clean message when present, else a bounded raw excerpt.
            let detail = serde_json::from_str::<ErrorResponse>(&raw)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| response_excerpt(&raw));
            anyhow::bail!(
                "embed request to `{}` failed: http status {}: {}",
                embed_url,
                status.as_u16(),
                detail
            );
        }

        let parsed: EmbedResponse = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("malformed embed response: {e}"))?;

        // Count contract: one embedding per input, retryable on violation (a transient server fault
        // can drop rows).
        let n = texts.len();
        if parsed.data.len() != n {
            anyhow::bail!(
                "embed count mismatch: requested {n} texts but server returned {} embeddings",
                parsed.data.len()
            );
        }

        // The OpenAI `data[]` carries a per-request `index`; a server MAY return items out of
        // order. Place each embedding at its `index` and require the indices to cover
        // EXACTLY `0..n` with no duplicate or out-of-range — sorting alone would silently
        // accept a dup/gap and misalign vectors with their chunks.
        let mut ordered: Vec<Option<Vec<f32>>> = std::iter::repeat_with(|| None).take(n).collect();
        for item in parsed.data {
            let idx = item.index;
            if idx >= n {
                anyhow::bail!("embed response index {idx} out of range for {n} inputs");
            }
            if ordered[idx].is_some() {
                anyhow::bail!("embed response has a duplicate index {idx}");
            }
            ordered[idx] = Some(item.embedding);
        }
        let embeddings = ordered
            .into_iter()
            .enumerate()
            .map(|(i, v)| v.ok_or_else(|| anyhow::anyhow!("embed response is missing index {i}")))
            .collect::<anyhow::Result<Vec<Vec<f32>>>>()?;

        // Dim contract (LOUD): the int8 encoding, per-family centroids, and the linked-entries rail
        // all assume a fixed dim. A server returning a different-width vector means the configured
        // model and the server model disagree — naming both dims makes the misconfiguration
        // obvious. EVERY vector is checked, not just the first: a late wrong-width vector that
        // slipped through would make `store_embedding` bail and ABORT the whole reconcile instead
        // of failing just that chunk, so we name the offending index and reject here.
        for (i, vector) in embeddings.iter().enumerate() {
            if vector.len() != dim {
                anyhow::bail!(
                    "embed dim mismatch: server returned a {}-dim vector at index {} but this \
                     model is configured for {} dims (server model `{}`). The selected registry \
                     model and the server model must match.",
                    vector.len(),
                    i,
                    dim,
                    server_model
                );
            }
        }

        Ok(embeddings)
    }

    /// Send ONE `/v1/embeddings` request for `texts` (already sized to `<= self.batch_size` by the
    /// caller) and return the parsed vectors, enforcing the count + per-vector dim contracts.
    /// Factored out of `embed_batch` so the sub-batch loop reuses the exact request/validation
    /// logic.
    fn embed_one_request(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Self::embed_one_request_with(
            self.agent.clone(),
            &self.embed_url,
            &self.server_model,
            self.dim,
            self.auth_header.as_deref(),
            texts,
        )
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

/// Resolve the `Authorization` header from the configured `auth_env` name, looking the value up
/// through `lookup` (the env in production; a fake closure in tests). `None`/empty `auth_env` → no
/// auth (`Ok(None)`); a named-but-missing/empty value → `Err` (the operator asked for auth but the
/// token isn't there). Closure-injected so the env-mutation footgun (unsafe + flaky under nextest's
/// parallel runner in Rust 2024) never enters the test path.
fn resolve_auth_header(
    auth_env: Option<&str>,
    lookup: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Option<String>> {
    let Some(var) = auth_env.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let token =
        lookup(var).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).ok_or_else(|| {
            anyhow::anyhow!(
                "remote embedding auth env var `{var}` is set in config but missing or empty in \
                 the environment"
            )
        })?;
    Ok(Some(format!("Bearer {token}")))
}

/// Whether the endpoint's host is loopback (`127.0.0.1`, `localhost`, `::1`). Loopback endpoints
/// bypass the ambient HTTP proxy; everything else inherits it. Parses the host out of the URL by
/// stripping the scheme then the path/port, tolerating a bracketed IPv6 literal.
fn endpoint_is_loopback(endpoint: &str) -> bool {
    let after_scheme = endpoint.split_once("://").map_or(endpoint, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or(after_scheme);
    // Strip userinfo (`user:pass@host`) if present.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = match host_port.strip_prefix('[') {
        // Bracketed IPv6 literal: `[::1]:11434` → `::1`.
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        // Bare host or IPv4: take everything before the first `:` (the port).
        None => host_port.split(':').next().unwrap_or(host_port),
    };
    matches!(host.trim().to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1")
        || host.starts_with("127.")
}

/// Best-effort reachability probe for a query/connect endpoint: resolve the URL's `host:port` and
/// attempt a short TCP connect. GATES the ephemeral light-embed path — an unreachable local query
/// server must DEFER (never embed-and-fail into `Failed` chunk_embeddings), and the fast refusal
/// keeps a watcher/maintenance pass cheap when no local server is running (it returns before any
/// O(repo) scan). A refused connection returns at once; only an unroutable host waits out the ~1s
/// timeout. An endpoint we can't parse a `host:port` from is treated as unreachable.
pub(crate) fn endpoint_reachable(endpoint: &str) -> bool {
    let Some(host_port) = endpoint_host_port(endpoint) else {
        return false;
    };
    host_port
        .to_socket_addrs()
        .map(|addrs| {
            addrs.into_iter().any(|addr| {
                std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
            })
        })
        .unwrap_or(false)
}

/// Extract `host:port` from an endpoint URL for a socket connect, defaulting the port by scheme
/// (`https` → 443, else 80) when the authority omits it. Mirrors the host parse in
/// `endpoint_is_loopback` but keeps the port.
fn endpoint_host_port(endpoint: &str) -> Option<String> {
    let (scheme, after_scheme) =
        endpoint.split_once("://").map_or(("http", endpoint), |(s, rest)| (s, rest));
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or(after_scheme);
    let host_port = authority.rsplit('@').next().unwrap_or(authority).trim();
    if host_port.is_empty() {
        return None;
    }
    let has_port = match host_port.strip_prefix('[') {
        // Bracketed IPv6 (`[::1]:8000`) has a port iff `]` is followed by `:`.
        Some(rest) => rest.split_once(']').is_some_and(|(_, tail)| tail.starts_with(':')),
        // Bare host / IPv4: exactly one `:` separates host and port.
        None => host_port.matches(':').count() == 1,
    };
    if has_port {
        Some(host_port.to_string())
    } else {
        let port = if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 };
        Some(format!("{host_port}:{port}"))
    }
}

impl Embedder for OpenAiEmbedder {
    fn model_id(&self) -> &str {
        // The SELECTED model's registry id (e.g. `fastembed-all-minilm-l6-v2`), NOT the server-side
        // Ollama model name (`self.server_model`): chunk_embeddings key by the selected model, so
        // the same rows are reused whether the model runs locally or via Ollama. The
        // freshness version (not the model_id) is what distinguishes local vs remote.
        &self.selected_model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Split into sub-batches by both text count and total input chars, then send up to the
        // configured concurrency at once. Results are joined in request/input order, so HTTP
        // completion order cannot reorder vectors.
        let ranges = self.sub_batch_ranges(texts);
        if ranges.len() == 1 {
            return self.embed_one_request(texts);
        }
        let mut out = Vec::with_capacity(texts.len());
        for window in ranges.chunks(self.concurrency) {
            let results = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(window.len());
                for &(start, end) in window {
                    let agent = self.agent.clone();
                    let embed_url = self.embed_url.clone();
                    let server_model = self.server_model.clone();
                    let auth_header = self.auth_header.clone();
                    let dim = self.dim;
                    let sub_batch = &texts[start..end];
                    handles.push(scope.spawn(move || {
                        Self::embed_one_request_with(
                            agent,
                            &embed_url,
                            &server_model,
                            dim,
                            auth_header.as_deref(),
                            sub_batch,
                        )
                    }));
                }
                handles.into_iter().map(|handle| handle.join()).collect::<Result<Vec<_>, _>>()
            })
            .map_err(|_| anyhow::anyhow!("embed worker thread panicked"))?;
            for result in results {
                out.extend(result?);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    use super::*;

    const DIM: usize = 384;
    // The SELECTED model's registry id — what `model_id()` must return regardless of the runtime.
    const SELECTED_ID: &str = crate::embedding_models::FASTEMBED_MODEL_ID;

    #[test]
    fn endpoint_host_port_keeps_explicit_ports_and_defaults_by_scheme() {
        // Explicit port preserved.
        assert_eq!(endpoint_host_port("http://localhost:7997").as_deref(), Some("localhost:7997"));
        assert_eq!(
            endpoint_host_port("http://127.0.0.1:11434").as_deref(),
            Some("127.0.0.1:11434")
        );
        // Bracketed IPv6 with a port.
        assert_eq!(endpoint_host_port("http://[::1]:8000").as_deref(), Some("[::1]:8000"));
        // No port → default by scheme.
        assert_eq!(endpoint_host_port("http://example.com").as_deref(), Some("example.com:80"));
        assert_eq!(endpoint_host_port("https://example.com").as_deref(), Some("example.com:443"));
        // Userinfo stripped; trailing path ignored.
        assert_eq!(
            endpoint_host_port("http://user:pass@host:9000/v1").as_deref(),
            Some("host:9000")
        );
    }

    #[test]
    fn endpoint_reachable_is_false_for_a_closed_port() {
        // Bind then drop → the port refuses connections; the probe must return false FAST (a
        // refusal returns immediately, it does not wait out the timeout).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!endpoint_reachable(&format!("http://127.0.0.1:{port}")));
        assert!(!endpoint_reachable("not a url"));
    }

    #[test]
    fn endpoint_reachable_is_true_for_a_live_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            // Accept one connection (the probe) so the listener stays bound for the check.
            let _ = listener.accept();
        });
        assert!(endpoint_reachable(&format!("http://127.0.0.1:{port}")));
        let _ = handle.join();
    }

    /// Construct the embedder for the selected model over the given remote config + dim. Thin
    /// wrapper so the many tests don't repeat the `selected_model_id` arg.
    fn build(cfg: &RemoteEmbeddingConfig, dim: usize) -> anyhow::Result<OpenAiEmbedder> {
        OpenAiEmbedder::from_remote_config(cfg, SELECTED_ID, dim)
    }

    /// Spawn a one-shot HTTP/1.1 server on `127.0.0.1:0` that accepts a single connection, drains
    /// the request, optionally sleeps, then writes `status` + `body`. Returns the bound base URL
    /// (`http://127.0.0.1:<port>`) and the server thread's join handle.
    fn spawn_stub(
        status_line: &'static str,
        body: String,
        delay: Option<Duration>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                drain_request(&mut stream);
                if let Some(d) = delay {
                    thread::sleep(d);
                }
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: \
                     {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    /// Spawn a one-shot stub that captures the request JSON body before replying.
    fn spawn_body_capture_stub(response_body: String) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return String::new();
            };
            let request_body = read_request_body(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            request_body
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    /// Read headers, parse Content-Length, then read exactly the request body bytes.
    fn read_request_body(stream: &mut TcpStream) -> String {
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n");
            if let Some(end) = header_end {
                let headers = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                let content_len = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let body_start = end + 4;
                while raw.len() < body_start + content_len {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                return String::from_utf8_lossy(&raw[body_start..body_start + content_len])
                    .to_string();
            }
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return String::new(),
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// Read the request headers (up to the blank line) so the client's write completes before we
    /// reply — a one-shot read is enough for these small test bodies.
    fn drain_request(stream: &mut TcpStream) {
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
    }

    /// OpenAI `/v1/embeddings` success body: `{"data":[{"embedding":[..],"index":i}, ...]}` where
    /// `index` is the position within THIS request (0-based).
    fn embeddings_json(vectors: &[Vec<f32>]) -> String {
        let rows = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let nums = v.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(",");
                format!("{{\"embedding\":[{nums}],\"index\":{i}}}")
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"data\":[{rows}]}}")
    }

    fn config_for(endpoint: &str, timeout_s: u64) -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: crate::config::RemoteBackend::Ollama,
            endpoint: Some(endpoint.to_string()),
            cookbook: None,
            query_endpoint: None,
            auth_env: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            request_timeout_s: timeout_s,
        }
    }

    fn texts(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("text {i}")).collect()
    }

    #[test]
    fn embed_batch_returns_vectors_in_order() {
        let want = vec![vec![1.0f32; DIM], vec![2.0f32; DIM], vec![3.0f32; DIM]];
        let (url, handle) = spawn_stub("200 OK", embeddings_json(&want), None);
        let embedder = build(&config_for(&url, 5), DIM).unwrap();

        let got = embedder.embed_batch(&texts(3)).expect("happy batch");
        handle.join().unwrap();

        assert_eq!(got.len(), 3);
        assert_eq!(got[0][0], 1.0);
        assert_eq!(got[1][0], 2.0);
        assert_eq!(got[2][0], 3.0);
        assert!(got.iter().all(|v| v.len() == DIM));
    }

    #[test]
    fn embed_request_uses_the_openai_shape_and_omits_num_ctx() {
        let want = vec![vec![1.0f32; DIM]];
        let (url, handle) = spawn_body_capture_stub(embeddings_json(&want));
        let mut cfg = config_for(&url, 5);
        // `num_ctx` stays a config field (freshness + model-load) but has NO per-request wire form
        // on `/v1/embeddings` — it must NOT appear in the request body.
        cfg.num_ctx = Some(4096);
        let embedder = build(&cfg, DIM).unwrap();

        embedder.embed_batch(&texts(1)).expect("embed succeeds");
        let body = handle.join().unwrap();

        let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(payload["model"], "all-minilm");
        assert!(payload["input"].is_array());
        assert_eq!(payload["encoding_format"], "float");
        assert!(
            payload.get("options").is_none(),
            "num_ctx/options must not be sent to /v1/embeddings"
        );
    }

    /// A multi-request HTTP stub: accepts `max_conns` connections, and for each request replies
    /// with one DIM-wide vector PER input text in that request's body (so the per-sub-batch
    /// count check passes). Each returned vector's first component is the `N` from `text N`, so
    /// the concatenated `embed_batch` output reads `[0.0, 1.0, 2.0, ...]` iff sub-batches are
    /// stitched back in input order. Returns the URL, the join handle, and the shared request
    /// COUNT so the test can assert the configured cap produced the expected number of requests.
    fn spawn_counting_stub(
        max_conns: usize,
    ) -> (String, thread::JoinHandle<()>, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let (url, handle, requests, _) = spawn_parallel_counting_stub(max_conns, Vec::new());
        (url, handle, requests)
    }

    fn spawn_parallel_counting_stub(
        max_conns: usize,
        delays: Vec<(usize, Duration)>,
    ) -> (
        String,
        thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let delays = Arc::new(delays);
        let max_seen = Arc::clone(&max_in_flight);
        let handle = thread::spawn(move || {
            let mut workers = Vec::new();
            for _ in 0..max_conns {
                let Ok((mut stream, _)) = listener.accept() else { break };
                let counter = Arc::clone(&counter);
                let in_flight = Arc::clone(&in_flight);
                let max_seen = Arc::clone(&max_seen);
                let delays = Arc::clone(&delays);
                workers.push(thread::spawn(move || {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    raise_max(&max_seen, now);
                    let body = read_request_body(&mut stream);
                    let indices = request_text_indices(&body);
                    if let Some(first) = indices.first().copied()
                        && let Some((_, delay)) = delays.iter().find(|(idx, _)| *idx == first)
                    {
                        thread::sleep(*delay);
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    let vector_ids = if indices.is_empty() { vec![0] } else { indices };
                    let vectors: Vec<Vec<f32>> = vector_ids
                        .into_iter()
                        .map(|idx| {
                            let mut v = vec![idx as f32; DIM];
                            v[0] = idx as f32;
                            v
                        })
                        .collect();
                    let json = embeddings_json(&vectors);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                         {}\r\nConnection: close\r\n\r\n{json}",
                        json.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                }));
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        (format!("http://127.0.0.1:{port}"), handle, requests, max_in_flight)
    }

    fn request_text_indices(body: &str) -> Vec<usize> {
        body.match_indices("text ")
            .filter_map(|(start, _)| {
                body[start + 5..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<usize>()
                    .ok()
            })
            .collect()
    }

    fn raise_max(max_seen: &std::sync::atomic::AtomicUsize, value: usize) {
        use std::sync::atomic::Ordering;

        let mut current = max_seen.load(Ordering::SeqCst);
        while value > current {
            match max_seen.compare_exchange(current, value, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }

    fn indexed_texts(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("text {i}")).collect()
    }

    fn first_components(vectors: &[Vec<f32>]) -> Vec<f32> {
        vectors.iter().map(|vector| vector.first().copied().unwrap_or_default()).collect()
    }

    #[test]
    fn embed_batch_splits_by_char_budget_as_well_as_count_budget() {
        use std::sync::atomic::Ordering;

        let (url, handle, requests) = spawn_counting_stub(2);
        let mut cfg = config_for(&url, 5);
        cfg.batch_size = 10;
        cfg.max_batch_chars = 13;
        let embedder = build(&cfg, DIM).unwrap();

        let got = embedder.embed_batch(&indexed_texts(3)).expect("char-budgeted embed");
        handle.join().unwrap();

        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "three 6-char texts at max_batch_chars 13 → 2 requests"
        );
        assert_eq!(first_components(&got), vec![0.0, 1.0, 2.0]);
    }

    #[test]
    fn embed_batch_dispatches_sub_batches_concurrently() {
        use std::sync::atomic::Ordering;

        let delay = Duration::from_millis(120);
        let all_delayed = (0..8).map(|i| (i, delay)).collect::<Vec<_>>();

        let (seq_url, seq_handle, _, seq_max) =
            spawn_parallel_counting_stub(8, all_delayed.clone());
        let mut seq_cfg = config_for(&seq_url, 5);
        seq_cfg.batch_size = 1;
        seq_cfg.concurrency = 1;
        let seq_embedder = build(&seq_cfg, DIM).unwrap();
        let started = std::time::Instant::now();
        seq_embedder.embed_batch(&indexed_texts(8)).expect("sequential delayed embed");
        let sequential = started.elapsed();
        seq_handle.join().unwrap();

        let (conc_url, conc_handle, _, conc_max) = spawn_parallel_counting_stub(8, all_delayed);
        let mut conc_cfg = config_for(&conc_url, 5);
        conc_cfg.batch_size = 1;
        conc_cfg.concurrency = 4;
        let conc_embedder = build(&conc_cfg, DIM).unwrap();
        let started = std::time::Instant::now();
        conc_embedder.embed_batch(&indexed_texts(8)).expect("concurrent delayed embed");
        let concurrent = started.elapsed();
        conc_handle.join().unwrap();

        assert_eq!(seq_max.load(Ordering::SeqCst), 1, "sequential config has one in flight");
        assert!(conc_max.load(Ordering::SeqCst) > 1, "concurrent config must overlap requests");
        assert!(
            concurrent < sequential,
            "concurrent dispatch should be faster than sequential: {concurrent:?} vs \
             {sequential:?}"
        );
    }

    #[test]
    fn embed_batch_keeps_output_order_when_later_requests_finish_first() {
        let (url, handle, _, max_in_flight) =
            spawn_parallel_counting_stub(2, vec![(0, Duration::from_millis(200))]);
        let mut cfg = config_for(&url, 5);
        cfg.batch_size = 1;
        cfg.concurrency = 2;
        let embedder = build(&cfg, DIM).unwrap();

        let got = embedder.embed_batch(&indexed_texts(2)).expect("out-of-order responses succeed");
        handle.join().unwrap();

        assert!(
            max_in_flight.load(std::sync::atomic::Ordering::SeqCst) > 1,
            "test must overlap the delayed and fast requests"
        );
        assert_eq!(first_components(&got), vec![0.0, 1.0]);
    }

    #[test]
    fn embed_batch_single_over_char_budget_text_is_sent_alone() {
        use std::sync::atomic::Ordering;

        let (url, handle, requests) = spawn_counting_stub(2);
        let mut cfg = config_for(&url, 5);
        cfg.batch_size = 10;
        cfg.max_batch_chars = 1;
        let embedder = build(&cfg, DIM).unwrap();

        let got = embedder.embed_batch(&indexed_texts(2)).expect("over-budget singletons embed");
        handle.join().unwrap();

        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(first_components(&got), vec![0.0, 1.0]);
    }

    #[test]
    fn sub_batch_ranges_respects_exact_char_budget_boundary() {
        let embedder = build(&config_for("http://127.0.0.1:1", 5), DIM).unwrap();
        let mut sized = embedder;
        sized.batch_size = 10;
        sized.max_batch_chars = 12;
        assert_eq!(sized.sub_batch_ranges(&indexed_texts(3)), vec![(0, 2), (2, 3)]);
    }

    #[test]
    fn request_text_indices_finds_json_string_markers_in_order() {
        assert_eq!(request_text_indices(r#"{"input":["text 7","text 2"]}"#), vec![7, 2]);
    }

    #[test]
    fn embed_batch_splits_into_sub_batches_of_at_most_the_configured_batch_size() {
        // batch_size = 2, 5 texts → 3 requests (2 + 2 + 1), all 5 vectors returned IN ORDER. This
        // is the P2 fix: `[remote] batch_size` caps the per-request size regardless of the
        // larger batch the reconcile loop hands the embedder.
        use std::sync::atomic::Ordering;

        let (url, handle, requests) = spawn_counting_stub(3);
        let mut cfg = config_for(&url, 5);
        cfg.batch_size = 2;
        let embedder = build(&cfg, DIM).unwrap();

        let got = embedder.embed_batch(&texts(5)).expect("sub-batched embed");
        handle.join().unwrap();

        assert_eq!(requests.load(Ordering::SeqCst), 3, "5 texts at batch_size 2 → 3 requests");
        assert_eq!(got.len(), 5, "all 5 vectors returned");
        // The global counter encodes order: sub-batch boundaries must not reorder the output.
        for (i, v) in got.iter().enumerate() {
            assert_eq!(v[0], i as f32, "vector {i} out of order: {}", v[0]);
            assert_eq!(v.len(), DIM);
        }
    }

    #[test]
    fn embed_batch_with_a_large_batch_size_sends_a_single_request() {
        // batch_size default (256) >> 3 texts → exactly one request (no needless fan-out).
        use std::sync::atomic::Ordering;

        let (url, handle, requests) = spawn_counting_stub(1);
        let embedder = build(&config_for(&url, 5), DIM).unwrap();

        let got = embedder.embed_batch(&texts(3)).expect("single-request embed");
        handle.join().unwrap();

        assert_eq!(requests.load(Ordering::SeqCst), 1, "3 texts under the cap → one request");
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn batch_size_zero_is_clamped_to_one_per_request() {
        // A misconfigured `batch_size = 0` must NOT panic `chunks(0)`; it degrades to one text per
        // request.
        use std::sync::atomic::Ordering;

        let (url, handle, requests) = spawn_counting_stub(3);
        let mut cfg = config_for(&url, 5);
        cfg.batch_size = 0;
        let embedder = build(&cfg, DIM).unwrap();

        let got = embedder.embed_batch(&texts(3)).expect("clamped embed");
        handle.join().unwrap();

        assert_eq!(requests.load(Ordering::SeqCst), 3, "batch_size 0 → one text per request");
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn embed_batch_errors_on_dim_mismatch_naming_both_dims() {
        let server_dim = 512;
        let want =
            vec![vec![1.0f32; server_dim], vec![2.0f32; server_dim], vec![3.0f32; server_dim]];
        let (url, handle) = spawn_stub("200 OK", embeddings_json(&want), None);
        let embedder = build(&config_for(&url, 5), DIM).unwrap();

        let err = embedder.embed_batch(&texts(3)).expect_err("dim mismatch must error");
        handle.join().unwrap();

        let msg = err.to_string();
        assert!(msg.contains("512"), "names the server dim: {msg}");
        assert!(msg.contains("384"), "names the configured dim: {msg}");
    }

    #[test]
    fn embed_batch_errors_when_a_later_vector_has_the_wrong_dim() {
        // First vector is correct width; the SECOND is wrong. A first-only check would return Ok
        // and let store_embedding abort the whole reconcile — every vector must be
        // validated here.
        let want = vec![vec![1.0f32; DIM], vec![2.0f32; DIM + 1], vec![3.0f32; DIM]];
        let (url, handle) = spawn_stub("200 OK", embeddings_json(&want), None);
        let embedder = build(&config_for(&url, 5), DIM).unwrap();

        let err = embedder.embed_batch(&texts(3)).expect_err("late wrong-width vector must error");
        handle.join().unwrap();

        let msg = err.to_string();
        assert!(msg.contains(&(DIM + 1).to_string()), "names the offending vector's dim: {msg}");
        assert!(msg.contains("384"), "names the configured dim: {msg}");
        assert!(msg.contains("index 1"), "names the offending vector's index: {msg}");
    }

    #[test]
    fn embed_batch_errors_on_count_mismatch() {
        // 2 vectors returned for 3 inputs.
        let want = vec![vec![1.0f32; DIM], vec![2.0f32; DIM]];
        let (url, handle) = spawn_stub("200 OK", embeddings_json(&want), None);
        let embedder = build(&config_for(&url, 5), DIM).unwrap();

        let err = embedder.embed_batch(&texts(3)).expect_err("count mismatch must error");
        handle.join().unwrap();

        let msg = err.to_string();
        assert!(
            msg.contains('3') && msg.contains('2'),
            "names requested vs returned counts: {msg}"
        );
    }

    #[test]
    fn embed_batch_errors_on_http_500() {
        let (url, handle) = spawn_stub("500 Internal Server Error", "boom".to_string(), None);
        let embedder = build(&config_for(&url, 5), DIM).unwrap();

        let err = embedder.embed_batch(&texts(1)).expect_err("non-2xx must error");
        handle.join().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("http status 500"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    #[test]
    fn embed_batch_errors_on_connection_refused() {
        // Bind then immediately drop the listener so the port is closed: connect is refused.
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{port}");
        let embedder = build(&config_for(&url, 5), DIM).unwrap();

        let err = embedder.embed_batch(&texts(1)).expect_err("connection refused must error");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn embed_batch_errors_on_timeout_without_hanging() {
        // Stub sleeps 2s; the agent's global timeout is 1s, so the call must error, not hang.
        let want = vec![vec![1.0f32; DIM]];
        let (url, handle) =
            spawn_stub("200 OK", embeddings_json(&want), Some(Duration::from_secs(2)));
        let embedder = build(&config_for(&url, 1), DIM).unwrap();

        let started = std::time::Instant::now();
        let err = embedder.embed_batch(&texts(1)).expect_err("timeout must error");
        let elapsed = started.elapsed();

        // The stub is one-shot and may be left mid-sleep; detach rather than block the test on it.
        drop(handle);
        assert!(
            elapsed < Duration::from_millis(1800),
            "must time out near the 1s bound, not after the 2s server delay (elapsed: {elapsed:?})"
        );
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn from_remote_config_errors_when_endpoint_missing() {
        let mut cfg = config_for("unused", 5);
        cfg.endpoint = None;
        let err = build(&cfg, DIM).expect_err("missing endpoint errors");
        assert!(err.to_string().contains("endpoint"), "{err}");
    }

    #[test]
    fn resolve_auth_header_none_when_auth_env_absent_or_empty() {
        // Lookup must never run when there's no var name to resolve.
        let lookup = |_: &str| -> Option<String> { panic!("lookup should not be called") };
        assert_eq!(resolve_auth_header(None, lookup).unwrap(), None);
        assert_eq!(resolve_auth_header(Some("  "), lookup).unwrap(), None);
    }

    #[test]
    fn resolve_auth_header_errors_when_named_var_unset() {
        // Closure-injected lookup — no process-env mutation, safe under nextest's parallel runner.
        let err = resolve_auth_header(Some("OLLAMA_TOKEN"), |_| None)
            .expect_err("named-but-unset var errors");
        assert!(err.to_string().contains("auth env"), "{err}");
    }

    #[test]
    fn resolve_auth_header_errors_when_named_var_empty() {
        let err = resolve_auth_header(Some("OLLAMA_TOKEN"), |_| Some("   ".to_string()))
            .expect_err("named-but-empty var errors");
        assert!(err.to_string().contains("auth env"), "{err}");
    }

    #[test]
    fn resolve_auth_header_builds_bearer_from_looked_up_token() {
        let header =
            resolve_auth_header(Some("OLLAMA_TOKEN"), |_| Some("sekret".to_string())).unwrap();
        assert_eq!(header.as_deref(), Some("Bearer sekret"));
    }

    #[test]
    fn endpoint_is_loopback_classifies_hosts() {
        for ep in [
            "http://127.0.0.1:11434",
            "http://localhost:11434",
            "http://LOCALHOST",
            "http://127.0.0.5:11434",
            "http://[::1]:11434",
            "http://127.0.0.1",
        ] {
            assert!(endpoint_is_loopback(ep), "should be loopback: {ep}");
        }
        for ep in [
            "https://ollama.example.com:11434",
            "http://10.0.0.5:11434",
            "https://user:pass@remote.host/path",
            "http://192.168.1.10",
        ] {
            assert!(!endpoint_is_loopback(ep), "should NOT be loopback: {ep}");
        }
    }

    #[test]
    fn model_id_is_the_selected_model_not_the_server_model() {
        // `model_id()` returns the SELECTED model's registry id (so chunks key by the model, not
        // the runtime), NOT the server-side `model` from the remote config (`all-minilm`).
        let embedder = build(&config_for("http://127.0.0.1:1", 5), DIM).unwrap();
        assert_eq!(embedder.model_id(), SELECTED_ID);
        assert_ne!(embedder.model_id(), "all-minilm", "must not echo the server-side model name");
        assert_eq!(embedder.dim(), DIM);
    }
}
