//! Provider HTTP plumbing shared across the native clients: RFC-8288 pagination decoding,
//! status-to-outcome mapping, and payload identity validation. Providers own their endpoints
//! and payload mapping; the mechanics of "talk JSON to a paginated forge API" live here once.

use reqwest::Url;

use super::PapertrailClientError;
use super::transport::is_loopback_host;

/// Parse and validate a provider API origin: the provider's cloud default, or a self-hosted
/// `base_url` that must be a bare http(s) origin — credentials-free, path/query/fragment-free,
/// and plaintext ONLY to loopback. The transport enforces the loopback rule per request;
/// accepting a non-loopback `http` origin here would mint a "native" binding whose every sync
/// fails at the first request. Returns the trimmed API origin and the governor authority key.
pub fn resolve_api_origin(
    provider: &str,
    base_url: Option<&str>,
    default_origin: &str,
    api_path: &str,
) -> anyhow::Result<(String, String)> {
    let parsed = match base_url {
        None => Url::parse(default_origin)?,
        Some(base) => {
            let mut base = Url::parse(base)?;
            anyhow::ensure!(
                matches!(base.scheme(), "http" | "https"),
                "{provider} base URL must use http(s)"
            );
            anyhow::ensure!(
                base.username().is_empty() && base.password().is_none(),
                "{provider} base URL must not contain credentials"
            );
            anyhow::ensure!(
                matches!(base.path(), "" | "/")
                    && base.query().is_none()
                    && base.fragment().is_none(),
                "{provider} base URL must be an origin without a path, query, or fragment"
            );
            // `Url::host_str` keeps IPv6 brackets (`[::1]`); the transport's predicate
            // matches the bare host form.
            anyhow::ensure!(
                base.scheme() == "https"
                    || base
                        .host_str()
                        .is_some_and(|host| is_loopback_host(host.trim_matches(['[', ']']))),
                "{provider} base URL must use https (http is loopback-only)"
            );
            base.set_path(api_path);
            base
        },
    };
    let host =
        parsed.host_str().ok_or_else(|| anyhow::anyhow!("{provider} API origin has no host"))?;
    let authority_host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let authority = parsed
        .port()
        .map_or_else(|| authority_host.clone(), |port| format!("{authority_host}:{port}"));
    let api_origin = parsed.as_str().trim_end_matches('/').to_string();
    Ok((api_origin, authority))
}

/// Reject a non-2xx provider response, mapping 404 to the typed not-found outcome the mirror
/// understands. `provider` labels the error ("GitHub", "GitLab").
pub(crate) fn ensure_provider_success(
    provider: &str,
    status: u16,
    body: &str,
) -> anyhow::Result<()> {
    if status == 404 {
        return Err(PapertrailClientError::ItemNotFound.into());
    }
    anyhow::ensure!((200..300).contains(&status), "{provider} HTTP {status}: {body}");
    Ok(())
}

/// The `rel="next"` target from an RFC-8288 `Link` header — the pagination continuation both
/// GitHub and GitLab emit.
pub(crate) fn next_link(headers: &reqwest::header::HeaderMap) -> anyhow::Result<Option<String>> {
    let Some(link) = headers.get(reqwest::header::LINK) else { return Ok(None) };
    let link = link.to_str()?;
    Ok(link.split(',').find_map(|part| {
        let (url, rel) = part.trim().split_once(';')?;
        rel.trim().eq(r#"rel="next""#).then(|| url.trim().trim_matches(['<', '>']).to_string())
    }))
}

/// Provider payload rows must carry a positive numeric identity before they are trusted.
pub(crate) fn validate_positive_id(
    value: &serde_json::Value,
    field: &str,
    resource: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        value[field].as_u64().is_some_and(|id| id > 0),
        "{resource} has no valid {field}"
    );
    Ok(())
}
