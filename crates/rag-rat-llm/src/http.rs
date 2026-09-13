//! The shared OpenAI-compatible HTTP transport: the one place both remote clients — the embedder
//! ([`crate::openai`]) and the chat model ([`crate::chat`]) — build their `ureq` agent and send a
//! JSON POST. Agent posture, auth, the server error-body parse and the response excerpt live here
//! so there is one place to audit/secure the wire path and the two clients cannot drift.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Error body most OpenAI-compatible servers return on non-2xx (`{ "error": { "message" } }`),
/// parsed for a clearer message than the raw excerpt.
#[derive(Deserialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
}

/// Build a blocking `ureq` agent: a global per-request timeout, `http_status_as_error(false)` so a
/// non-2xx body stays readable, and no proxy for loopback endpoints. ureq's default config
/// inherits `HTTP_PROXY`/`HTTPS_PROXY` from the env; a local Ollama (`http://127.0.0.1:11434`)
/// routed through a corporate proxy 403s, so the proxy is disabled for loopback endpoints only — a
/// non-loopback (truly remote) endpoint may legitimately need it.
pub(crate) fn build_agent(
    endpoint: &str,
    request_timeout_s: u64,
    user_agent: &'static str,
) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(request_timeout_s)))
        .http_status_as_error(false)
        .user_agent(user_agent);
    if endpoint_is_loopback(endpoint) {
        builder = builder.proxy(None);
    }
    builder.build().into()
}

/// POST `payload` as JSON to `url` (with the `Authorization` header when `auth_header` is set) and
/// return the raw 2xx body; the caller parses it into its own response type. `op` names the call
/// in every error (`"<op> request to `<url>` failed: …"`). A non-2xx surfaces the server's
/// `{"error":{"message"}}` when present, else a bounded raw excerpt.
pub(crate) fn post_json(
    agent: &ureq::Agent,
    url: &str,
    auth_header: Option<&str>,
    payload: &impl Serialize,
    op: &str,
) -> anyhow::Result<String> {
    // The `json` ureq feature is not enabled (workspace ureq is rustls-only), so serialize the
    // body ourselves and send it with an explicit content-type.
    let body = serde_json::to_vec(payload)
        .map_err(|e| anyhow::anyhow!("failed to serialize {op} request: {e}"))?;
    let mut request = agent.post(url).content_type("application/json");
    if let Some(header) = auth_header {
        request = request.header("Authorization", header);
    }
    // `http_status_as_error(false)` lets us read the server's JSON error body on 4xx/5xx instead of
    // losing the actionable reason behind ureq's bare `http status: N` error.
    let mut response =
        request.send(body).map_err(|e| anyhow::anyhow!("{op} request to `{url}` failed: {e}"))?;
    let status = response.status();
    let raw = response
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow::anyhow!("reading {op} response failed: {e}"))?;
    if !status.is_success() {
        let detail = serde_json::from_str::<ErrorResponse>(&raw)
            .map(|e| e.error.message)
            .unwrap_or_else(|_| response_excerpt(&raw));
        anyhow::bail!("{op} request to `{url}` failed: http status {}: {detail}", status.as_u16());
    }
    Ok(raw)
}

fn response_excerpt(body: &str) -> String {
    let trimmed = body.trim();
    let mut excerpt = trimmed.chars().take(500).collect::<String>();
    if trimmed.chars().count() > 500 {
        excerpt.push_str("...");
    }
    excerpt
}

/// Whether the endpoint's host is loopback (`127.0.0.1`, `localhost`, `::1`). Loopback endpoints
/// bypass the ambient HTTP proxy; everything else inherits it. Tolerates a bracketed IPv6 literal.
pub(crate) fn endpoint_is_loopback(endpoint: &str) -> bool {
    let host_port = url_authority(endpoint);
    let host = match host_port.strip_prefix('[') {
        // Bracketed IPv6 literal: `[::1]:11434` → `::1`.
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        // Bare host or IPv4: take everything before the first `:` (the port).
        None => host_port.split(':').next().unwrap_or(host_port),
    };
    matches!(host.trim().to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1")
        || host.starts_with("127.")
}

/// The `host[:port]` of an endpoint URL: the scheme, path/query/fragment, and any `user:pass@`
/// userinfo stripped. The one authority parse behind both loopback classification and
/// credential-free endpoint logging (`sanitize_endpoint`), so the two can't disagree on the host.
pub(crate) fn url_authority(url: &str) -> &str {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or(after_scheme);
    authority.rsplit_once('@').map_or(authority, |(_, host_port)| host_port)
}

#[cfg(test)]
mod tests {
    use super::endpoint_is_loopback;

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
}
