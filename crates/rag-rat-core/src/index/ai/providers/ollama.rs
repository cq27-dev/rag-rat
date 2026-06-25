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
use crate::embedding_models::OLLAMA_ALL_MINILM_MODEL_ID;

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
/// is the registry spec's dim (the parity contract); every response vector is checked against it on
/// each batch.
#[derive(Debug)]
pub struct OllamaEmbedder {
    agent: ureq::Agent,
    /// `<endpoint>/api/embed`, precomputed once at construction.
    embed_url: String,
    model: String,
    dim: usize,
    /// `Some("Bearer <token>")` when the server needs auth, read from `auth_env` at construction.
    auth_header: Option<String>,
}

impl OllamaEmbedder {
    /// Build the embedder from the `[local_ai.embedding.remote]` config and the registry spec's
    /// dim. `registry_dim` is the dim parity contract (the `ollama-all-minilm` row, 384) — it is
    /// the single source of truth for the expected vector length, never re-declared in config.
    ///
    /// Errors when the endpoint is absent (config validation already guarantees it in `Connect`
    /// mode, but the construction site refuses to build a half-formed embedder) or when `auth_env`
    /// names an env var that is missing or empty.
    pub fn from_remote_config(
        cfg: &RemoteEmbeddingConfig,
        registry_dim: usize,
    ) -> anyhow::Result<Self> {
        let endpoint =
            cfg.endpoint.as_deref().map(str::trim).filter(|e| !e.is_empty()).ok_or_else(|| {
                anyhow::anyhow!(
                    "remote embedding endpoint is required in connect mode but was not configured \
                     (`[local_ai.embedding.remote] endpoint`)"
                )
            })?;
        let embed_url = format!("{}/api/embed", endpoint.trim_end_matches('/'));

        let auth_header =
            resolve_auth_header(cfg.auth_env.as_deref(), |var| std::env::var(var).ok())?;

        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(cfg.request_timeout_s)))
            .user_agent(concat!("rag-rat/", env!("CARGO_PKG_VERSION"), " (ollama-embed)"));
        // ureq's default config inherits `HTTP_PROXY`/`HTTPS_PROXY` from the env. A local Ollama
        // (`http://127.0.0.1:11434`) routed through a corporate proxy 403s, so disable the proxy for
        // loopback endpoints only — a non-loopback (truly remote) endpoint may legitimately need
        // it.
        if endpoint_is_loopback(endpoint) {
            builder = builder.proxy(None);
        }
        let agent: ureq::Agent = builder.build().into();

        Ok(Self {
            agent,
            embed_url,
            model: cfg.model.trim().to_string(),
            dim: registry_dim,
            auth_header,
        })
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
        // The stable registry id, not the server-side Ollama model name (`self.model`): callers key
        // freshness/parity off the registry identity, which is what the rest of the pipeline knows.
        OLLAMA_ALL_MINILM_MODEL_ID
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let payload = EmbedRequest { model: &self.model, input: texts };
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
                     this model is configured for {} dims (model `{}`). The configured registry \
                     model and the Ollama server model must match.",
                    vector.len(),
                    i,
                    self.dim,
                    self.model
                );
            }
        }

        Ok(embeddings)
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
            mode: crate::config::RemoteMode::Connect,
            model: "all-minilm".to_string(),
            endpoint: Some(endpoint.to_string()),
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
        let embedder = OllamaEmbedder::from_remote_config(&config_for(&url, 5), DIM).unwrap();

        let got = embedder.embed_batch(&texts(3)).expect("happy batch");
        handle.join().unwrap();

        assert_eq!(got.len(), 3);
        assert_eq!(got[0][0], 1.0);
        assert_eq!(got[1][0], 2.0);
        assert_eq!(got[2][0], 3.0);
        assert!(got.iter().all(|v| v.len() == DIM));
    }

    #[test]
    fn embed_batch_errors_on_dim_mismatch_naming_both_dims() {
        let server_dim = 512;
        let want =
            vec![vec![1.0f32; server_dim], vec![2.0f32; server_dim], vec![3.0f32; server_dim]];
        let (url, handle) = spawn_stub("200 OK", embeddings_json(&want), None);
        let embedder = OllamaEmbedder::from_remote_config(&config_for(&url, 5), DIM).unwrap();

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
        let embedder = OllamaEmbedder::from_remote_config(&config_for(&url, 5), DIM).unwrap();

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
        let embedder = OllamaEmbedder::from_remote_config(&config_for(&url, 5), DIM).unwrap();

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
        let embedder = OllamaEmbedder::from_remote_config(&config_for(&url, 5), DIM).unwrap();

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
        let embedder = OllamaEmbedder::from_remote_config(&config_for(&url, 5), DIM).unwrap();

        let err = embedder.embed_batch(&texts(1)).expect_err("connection refused must error");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn embed_batch_errors_on_timeout_without_hanging() {
        // Stub sleeps 2s; the agent's global timeout is 1s, so the call must error, not hang.
        let want = vec![vec![1.0f32; DIM]];
        let (url, handle) =
            spawn_stub("200 OK", embeddings_json(&want), Some(Duration::from_secs(2)));
        let embedder = OllamaEmbedder::from_remote_config(&config_for(&url, 1), DIM).unwrap();

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
        let err =
            OllamaEmbedder::from_remote_config(&cfg, DIM).expect_err("missing endpoint errors");
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
    fn model_id_is_the_stable_registry_id() {
        let embedder =
            OllamaEmbedder::from_remote_config(&config_for("http://127.0.0.1:1", 5), DIM).unwrap();
        assert_eq!(embedder.model_id(), OLLAMA_ALL_MINILM_MODEL_ID);
        assert_eq!(embedder.dim(), DIM);
    }
}
