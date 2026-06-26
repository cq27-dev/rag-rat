//! The Ollama HTTP embedding backend: the ONLY code that connects to an Ollama server. All HTTP to
//! `/api/embed` lives in [`OllamaEmbedder::embed_batch`]; everything else (install-time
//! reachability probes, the dispatch in `providers/mod.rs`) reaches Ollama by constructing and
//! calling one of these. One connection implementation, one place to audit/secure/retry.
//!
//! Native, blocking HTTP via `ureq` (v3, rustls) — already a non-optional workspace dep (the
//! crates.io version check uses it), so this backend ships unconditionally: no cargo feature, no
//! missing-feature message. The reconcile loop is single-threaded, so the cloneable `ureq::Agent`
//! is held bare (no `Mutex`).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::Embedder;
use crate::config::RemoteEmbeddingConfig;

/// Request body for Ollama's `/api/embed`. `input` is an array — the batch endpoint embeds every
/// text in one request.
#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

/// Response body from Ollama's `/api/embed`: one vector per input, in order.
#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

/// A native HTTP embedder that offloads embedding work to an Ollama server's `/api/embed`. The dim
/// is the SELECTED model's registry dim (the parity contract); every response vector is checked
/// against it on each batch.
#[derive(Debug)]
pub struct OllamaEmbedder {
    agent: ureq::Agent,
    /// `<endpoint>/api/embed`, precomputed once at construction.
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
    /// Max texts per `/api/embed` request (`[local_ai.embedding.remote] batch_size`).
    /// `embed_batch` splits its input into sub-batches of at most this, so a request never
    /// exceeds the configured cap regardless of the reconcile/runtime batch size. Clamped to
    /// `>= 1` at construction so the `chunks()` split can't panic on a misconfigured `0`.
    batch_size: usize,
    /// `Some("Bearer <token>")` when the server needs auth, read from `auth_env` at construction.
    auth_header: Option<String>,
}

impl OllamaEmbedder {
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
                     (`[local_ai.embedding.remote] endpoint`)"
                )
            })?;
        // CONNECT auth comes from the env var NAMED by `auth_env` (the token never enters config).
        let auth_header =
            resolve_auth_header(cfg.auth_env.as_deref(), |var| std::env::var(var).ok())?;
        Ok(Self::build(
            endpoint,
            auth_header,
            selected_model_id,
            cfg.model.trim(),
            dim,
            cfg.request_timeout_s,
            cfg.batch_size,
        ))
    }

    /// Build the embedder against a freshly PROVISIONED ephemeral box (#318). The `endpoint` +
    /// `auth_token` come from the cookbook handshake — `auth_token` is a DIRECT bearer token (the
    /// box's per-run credential), NOT an env-var name (contrast [`Self::from_remote_config`], which
    /// resolves `auth_env`). The model identity (`selected_model_id` + `dim`) + transport knobs
    /// (server `model`, timeout, batch) come from the config the same way.
    #[allow(clippy::too_many_arguments)]
    pub fn from_provisioned(
        endpoint: &str,
        auth_token: Option<&str>,
        server_model: &str,
        selected_model_id: &str,
        dim: usize,
        request_timeout_s: u64,
        batch_size: u32,
    ) -> Self {
        let auth_header = auth_token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|token| format!("Bearer {token}"));
        Self::build(
            endpoint.trim(),
            auth_header,
            selected_model_id,
            server_model.trim(),
            dim,
            request_timeout_s,
            batch_size,
        )
    }

    /// Shared assembler: build the `ureq::Agent` (with the loopback proxy bypass) + the struct.
    /// `endpoint` is already trimmed/validated by the caller.
    fn build(
        endpoint: &str,
        auth_header: Option<String>,
        selected_model_id: &str,
        server_model: &str,
        dim: usize,
        request_timeout_s: u64,
        batch_size: u32,
    ) -> Self {
        let embed_url = format!("{}/api/embed", endpoint.trim_end_matches('/'));
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(request_timeout_s)))
            .user_agent(concat!("rag-rat/", env!("CARGO_PKG_VERSION"), " (ollama-embed)"));
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
            auth_header,
        }
    }

    /// Send ONE `/api/embed` request for `texts` (already sized to `<= self.batch_size` by the
    /// caller) and return the parsed vectors, enforcing the count + per-vector dim contracts.
    /// Factored out of `embed_batch` so the sub-batch loop reuses the exact request/validation
    /// logic.
    fn embed_one_request(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let payload = EmbedRequest { model: &self.server_model, input: texts };
        // The `json` ureq feature is not enabled (workspace ureq is rustls-only), so serialize the
        // body ourselves and send it with an explicit content-type.
        let body = serde_json::to_vec(&payload)
            .map_err(|e| anyhow::anyhow!("failed to serialize ollama embed request: {e}"))?;

        let mut request = self.agent.post(&self.embed_url).content_type("application/json");
        if let Some(header) = &self.auth_header {
            request = request.header("Authorization", header);
        }

        // `http_status_as_error` defaults to true in ureq 3, so a non-2xx status, a connection
        // refusal, or a timeout all surface as `Err` here — all retryable by the reconcile loop.
        let raw = request
            .send(body)
            .map_err(|e| {
                anyhow::anyhow!("ollama embed request to `{}` failed: {e}", self.embed_url)
            })?
            .body_mut()
            .read_to_string()
            .map_err(|e| anyhow::anyhow!("reading ollama embed response failed: {e}"))?;

        let parsed: EmbedResponse = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("malformed ollama embed response: {e}"))?;
        let embeddings = parsed.embeddings;

        // Count contract: one vector per input, retryable on violation (a transient server fault
        // can drop rows).
        if embeddings.len() != texts.len() {
            anyhow::bail!(
                "ollama embed count mismatch: requested {} texts but server returned {} vectors",
                texts.len(),
                embeddings.len()
            );
        }

        // Dim contract (LOUD): the int8 encoding, per-family centroids, and the linked-entries rail
        // all assume a fixed dim. A server returning a different-width vector means the configured
        // model and the server model disagree — naming both dims makes the misconfiguration
        // obvious. EVERY vector is checked, not just the first: a late wrong-width vector that
        // slipped through would make `store_embedding` bail and ABORT the whole reconcile instead
        // of failing just that chunk, so we name the offending index and reject here.
        for (i, vector) in embeddings.iter().enumerate() {
            if vector.len() != self.dim {
                anyhow::bail!(
                    "ollama embed dim mismatch: server returned a {}-dim vector at index {} but \
                     this model is configured for {} dims (server model `{}`). The selected \
                     registry model and the Ollama server model must match.",
                    vector.len(),
                    i,
                    self.dim,
                    self.server_model
                );
            }
        }

        Ok(embeddings)
    }
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

impl Embedder for OllamaEmbedder {
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

        // Split into sub-batches of at most the configured `batch_size` and send one `/api/embed`
        // per sub-batch, concatenating the results IN ORDER. This honors `[remote] batch_size`
        // regardless of the (larger) reconcile/runtime batch the loop hands us — a user capping it
        // for a server/proxy request-size limit is respected. The count + per-vector dim checks run
        // per sub-batch (in `embed_one_request`).
        let mut out = Vec::with_capacity(texts.len());
        for sub_batch in texts.chunks(self.batch_size) {
            out.extend(self.embed_one_request(sub_batch)?);
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

    /// Construct the embedder for the selected model over the given remote config + dim. Thin
    /// wrapper so the many tests don't repeat the `selected_model_id` arg.
    fn build(cfg: &RemoteEmbeddingConfig, dim: usize) -> anyhow::Result<OllamaEmbedder> {
        OllamaEmbedder::from_remote_config(cfg, SELECTED_ID, dim)
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

    /// Read the request headers (up to the blank line) so the client's write completes before we
    /// reply — a one-shot read is enough for these small test bodies.
    fn drain_request(stream: &mut TcpStream) {
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
    }

    fn embeddings_json(vectors: &[Vec<f32>]) -> String {
        let rows = vectors
            .iter()
            .map(|v| {
                let nums = v.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(",");
                format!("[{nums}]")
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"embeddings\":[{rows}]}}")
    }

    fn config_for(endpoint: &str, timeout_s: u64) -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            endpoint: Some(endpoint.to_string()),
            cookbook: None,
            query_endpoint: None,
            auth_env: None,
            batch_size: 256,
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

    /// A multi-request HTTP stub: accepts `max_conns` connections, and for each request replies
    /// with one DIM-wide vector PER input text in that request's body (so the per-sub-batch
    /// count check passes). Each returned vector's first component is a monotonically
    /// increasing global counter, so the concatenated `embed_batch` output reads `[0.0, 1.0,
    /// 2.0, ...]` IFF the sub-batches are stitched back in order. Returns the URL, the join
    /// handle, and the shared request COUNT so the test can assert the configured cap produced
    /// the expected number of requests.
    fn spawn_counting_stub(
        max_conns: usize,
    ) -> (String, thread::JoinHandle<()>, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            let mut next_value = 0u32;
            for _ in 0..max_conns {
                let Ok((mut stream, _)) = listener.accept() else { break };
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                // Read headers, parse Content-Length, then read EXACTLY that many body bytes. A
                // single `read()` can return just the headers before the body arrives, so reading a
                // fixed-size buffer once undercounts the inputs; this reads the whole request.
                let mut raw = Vec::new();
                let mut buf = [0u8; 8192];
                let body = loop {
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
                        break String::from_utf8_lossy(&raw[body_start..]).to_string();
                    }
                    match stream.read(&mut buf) {
                        Ok(0) => break String::new(),
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                        Err(_) => break String::new(),
                    }
                };
                // Count inputs in THIS request by the `"text N"` markers the test sends.
                let inputs = body.matches("text ").count().max(1);
                counter.fetch_add(1, Ordering::SeqCst);
                let vectors: Vec<Vec<f32>> = (0..inputs)
                    .map(|_| {
                        let v = vec![next_value as f32; DIM];
                        // Encode the global order in the FIRST component only.
                        let mut v = v;
                        v[0] = next_value as f32;
                        next_value += 1;
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
            }
        });
        (format!("http://127.0.0.1:{port}"), handle, requests)
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
        assert!(!err.to_string().is_empty());
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
