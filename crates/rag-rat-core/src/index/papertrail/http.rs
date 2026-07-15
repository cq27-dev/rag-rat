//! Provider HTTP plumbing shared across the native clients: RFC-8288 pagination decoding,
//! status-to-outcome mapping, and payload identity validation. Providers own their endpoints
//! and payload mapping; the mechanics of "talk JSON to a paginated forge API" live here once.

use super::PapertrailClientError;

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
