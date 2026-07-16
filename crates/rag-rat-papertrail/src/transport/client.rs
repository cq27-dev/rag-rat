//! The binding-scoped HTTP client: async `reqwest` over rustls, driven through the rate governor
//! on every request. Provider-neutral — URL building, pagination, and payload mapping stay in the
//! per-provider clients built on top; this layer owns admission, quota-header recording,
//! `429`/`Retry-After` backoff, and the sync pass's wall-clock cap.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use rag_rat_base::config::{PapertrailConfig, TrackerAuth};
use rag_rat_base::time::now_ms;
use reqwest::header::{self, HeaderMap};

use super::auth;
use super::governor::{
    Admission, GovernorConfig, GovernorKey, GovernorRegistry, PauseReason, QuotaSnapshot,
    RateGovernor, RetryHold, backoff_delay_ms,
};

/// How one transport call ends besides success.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    /// Rate-governed stop — NOT a failure. The caller keeps its pagination cursor and resumes
    /// at/after `resume_at_ms`; the next sync window continues mid-pagination.
    #[error("rate-governed pause ({reason}) until {resume_at_ms}")]
    Paused { resume_at_ms: i64, reason: PauseReason },
    /// The request URL escaped the binding's authority — refused before any bytes go out, so
    /// the binding's token can never travel to a foreign origin or over plaintext.
    #[error("url `{url}` is outside the binding `{host}`: {problem}")]
    UrlOutsideBinding { url: String, host: String, problem: &'static str },
    /// Connect/send/read failure from the HTTP stack.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

#[cfg(test)]
impl TransportError {
    pub(super) fn pause_details(&self) -> Option<(i64, PauseReason)> {
        match self {
            Self::Paused { resume_at_ms, reason } => Some((*resume_at_ms, *reason)),
            _ => None,
        }
    }
}

/// One completed HTTP exchange. Every non-rate-limited status comes back as `Ok` — provider
/// clients own status semantics (a 404 can be signal, not failure).
#[derive(Debug)]
pub(crate) struct TransportResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

/// Transport knobs. Everything has a conservative default; the config layer maps `[[tracker]]`
/// settings onto this.
#[derive(Debug, Clone)]
pub struct TransportOptions {
    pub governor: GovernorConfig,
    pub request_timeout_s: u64,
    /// Wall-clock budget for the whole sync pass, measured from construction: no request is sent
    /// past the deadline (healthy pagination included, not just backoff), in-flight timeouts are
    /// tightened to the remaining budget, and `429` backoff never sleeps past it — past it the
    /// call returns [`TransportError::Paused`] and the caller keeps its cursor.
    pub pass_budget_ms: i64,
    /// First backoff step; doubles per retry. `Retry-After` overrides when longer.
    pub backoff_base_ms: i64,
    /// `Authorization` scheme prefix. `Bearer` fits GitHub and GitLab; providers with another
    /// scheme override it.
    pub auth_scheme: &'static str,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            governor: GovernorConfig::default(),
            request_timeout_s: 30,
            pass_budget_ms: 300_000,
            backoff_base_ms: 1_000,
            auth_scheme: "Bearer",
        }
    }
}

impl From<&PapertrailConfig> for TransportOptions {
    fn from(config: &PapertrailConfig) -> Self {
        let mut options = Self::default();
        options.governor.reserve = config.rate_limit_reserve;
        options
    }
}

/// Construction inputs for one binding's transport.
pub(crate) struct TransportParams<'a> {
    /// Provider id (`github`, `gitlab`, …).
    pub provider: &'a str,
    /// Provider quota resource. GitHub Search and core REST are independent lanes.
    pub lane: &'a str,
    /// Authority the binding talks to — `api.github.com`, `gitlab.example.com:8443`, never a
    /// URL. Keys the governor AND pins every request: a URL outside it is refused.
    pub host: &'a str,
    #[cfg_attr(not(test), allow(dead_code))]
    pub auth: Option<&'a TrackerAuth>,
    /// Shared registry so sibling bindings on the same (provider, host, token) share one
    /// governor and jointly account their consumption.
    pub registry: &'a GovernorRegistry,
    pub options: TransportOptions,
}

/// Injected clock: feeds every governor decision so tests exercise pause/resume deterministically
/// without real sleeps. Production uses [`now_ms`].
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub(crate) struct Transport {
    client: reqwest::Client,
    governor: Arc<RateGovernor>,
    retry_hold: Arc<RetryHold>,
    /// Prebuilt `Authorization` value; `None` for anonymous bindings.
    auth_header: Option<String>,
    /// The binding's pinned authority (lowercased host, optional port) — every request URL must
    /// stay inside it.
    bound_host: String,
    bound_port: Option<u16>,
    is_github: bool,
    pass_deadline_ms: i64,
    request_timeout_ms: i64,
    backoff_base_ms: i64,
    clock: Clock,
}

impl Transport {
    /// Build the transport for one binding: resolves the token (fails fast on a configured-but-
    /// missing one), joins the shared governor for (provider, host, token), and constructs the
    /// rustls HTTP client. The pass's wall-clock deadline starts here.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(params: TransportParams<'_>) -> anyhow::Result<Self> {
        Self::with_clock(params, Arc::new(now_ms))
    }

    /// Build a sibling quota lane from one already-resolved binding credential. Provider clients
    /// with several lanes must snapshot auth once so token commands cannot yield split identities.
    pub fn new_with_token(
        params: TransportParams<'_>,
        token: Option<&str>,
    ) -> anyhow::Result<Self> {
        Self::with_clock_and_token(params, Arc::new(now_ms), token.map(str::to_string))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_clock(params: TransportParams<'_>, clock: Clock) -> anyhow::Result<Self> {
        let token = auth::resolve_token(params.auth)?;
        Self::with_clock_and_token(params, clock, token)
    }

    fn with_clock_and_token(
        params: TransportParams<'_>,
        clock: Clock,
        token: Option<String>,
    ) -> anyhow::Result<Self> {
        let auth_header =
            token.as_deref().map(|token| format!("{} {token}", params.options.auth_scheme));
        let (bound_host, bound_port) = parse_bound_authority(params.host)?;
        let governor_authority = canonical_governor_authority(&bound_host, bound_port);
        let governor_key =
            GovernorKey::new(params.provider, &governor_authority, token.as_deref(), params.lane);
        let retry_hold = params.registry.retry_hold(&governor_key);
        let governor = params.registry.governor(governor_key, &params.options.governor);
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(params.options.request_timeout_s))
            // Redirects are provider semantics. Following them here would bypass the per-hop URL
            // guard and governor admission, potentially forwarding auth to another authority.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("rag-rat/", env!("CARGO_PKG_VERSION")));
        // Loopback bindings (tests, a forge on localhost) bypass the ambient HTTP proxy, same as
        // the embedding transport; remote hosts inherit it.
        if is_loopback_host(&bound_host) {
            builder = builder.no_proxy();
        }
        let pass_deadline_ms = clock() + params.options.pass_budget_ms;
        Ok(Self {
            client: builder.build()?,
            governor,
            retry_hold,
            auth_header,
            bound_host,
            bound_port,
            is_github: params.provider.eq_ignore_ascii_case("github"),
            pass_deadline_ms,
            request_timeout_ms: i64::try_from(params.options.request_timeout_s)
                .unwrap_or(i64::MAX)
                .saturating_mul(1000),
            backoff_base_ms: params.options.backoff_base_ms,
            clock,
        })
    }

    /// GET `url` with the binding's auth plus `extra_headers`, rate-governed: URL pinned to the
    /// binding's authority, pass deadline and governor admission checked before every send, quota
    /// headers recorded after, and rate-limited replies retried on an exponential backoff that a
    /// `Retry-After` can only lengthen — never sleeping past the pass's wall-clock deadline
    /// (past it: [`TransportError::Paused`], cursor intact).
    pub(crate) async fn get(
        &self,
        url: &str,
        extra_headers: &[(&str, &str)],
    ) -> Result<TransportResponse, TransportError> {
        self.validate_url(url)?;
        let mut attempt: u32 = 0;
        loop {
            let now = (self.clock)();
            if now > self.pass_deadline_ms {
                return Err(TransportError::Paused {
                    resume_at_ms: now,
                    reason: PauseReason::PassBudget,
                });
            }
            if let Some(resume_at_ms) = self.retry_hold.paused_until(now) {
                return Err(TransportError::Paused {
                    resume_at_ms,
                    reason: PauseReason::RetryAfter,
                });
            }
            if let Admission::PausedUntil { resume_at_ms, reason } = self.governor.admit(now) {
                return Err(TransportError::Paused { resume_at_ms, reason });
            }
            let mut request = self.client.get(url).timeout(self.request_timeout(now));
            if let Some(auth_header) = &self.auth_header {
                request = request.header(header::AUTHORIZATION, auth_header);
            }
            for (name, value) in extra_headers {
                request = request.header(*name, *value);
            }
            let response = request.send().await?;
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let now = (self.clock)();
            if let Some(snapshot) = QuotaSnapshot::from_headers(&headers, now) {
                self.governor.record_quota(snapshot);
            }
            // Drain the body on every path — a rate-limited reply carries one too, and leaving
            // it unread can abort the connection.
            let body = response.text().await?;
            let conditional_not_modified = self.is_github
                && self.auth_header.is_some()
                && status == 304
                && extra_headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(header::IF_NONE_MATCH.as_str()));
            if conditional_not_modified {
                self.governor.refund_admission();
            }
            let github_secondary = self.is_github && is_github_secondary_limit(status, &body);
            if !is_rate_limited(status, &headers, now) && !github_secondary {
                return Ok(TransportResponse { status, headers, body });
            }
            let retry_after_ms =
                retry_after_ms(&headers, now).or_else(|| github_secondary.then_some(60_000));
            let delay_ms = backoff_delay_ms(attempt, retry_after_ms, self.backoff_base_ms);
            attempt += 1;
            let resume_at_ms = now.saturating_add(delay_ms);
            // GitHub secondary/abuse limits cover REST as a whole, while primary quota windows
            // remain lane-specific. Other providers retain the conservative lane-local hold.
            let shared_github_hold = self.is_github
                && (github_secondary || status == 429 || headers.contains_key(header::RETRY_AFTER));
            if shared_github_hold {
                self.retry_hold.record(resume_at_ms);
            } else {
                self.governor.record_hold(resume_at_ms);
            }
            if resume_at_ms > self.pass_deadline_ms {
                return Err(TransportError::Paused {
                    resume_at_ms,
                    reason: PauseReason::RetryAfter,
                });
            }
            tokio::time::sleep(Duration::from_millis(delay_ms.max(0) as u64)).await;
        }
    }

    /// Pin every request to the binding's authority BEFORE anything is consumed or sent: the
    /// binding's token must never travel to a foreign origin (a hostile or malformed absolute
    /// pagination URL would otherwise exfiltrate it) and never over plaintext except to loopback
    /// (stubs, local forges). A bound port pins the port; without one, only the scheme-default
    /// port passes — except on loopback, where stubs bind ephemeral ports.
    fn validate_url(&self, url: &str) -> Result<(), TransportError> {
        let outside = |problem: &'static str| TransportError::UrlOutsideBinding {
            url: url.to_string(),
            host: self.bound_host.clone(),
            problem,
        };
        let parsed = reqwest::Url::parse(url).map_err(|_| outside("not a valid absolute URL"))?;
        // The url crate keeps IPv6 hosts bracketed; the bound side stores them bare.
        let host = parsed.host_str().unwrap_or("").trim_matches(['[', ']']).to_ascii_lowercase();
        if host.is_empty() || host != self.bound_host {
            return Err(outside("host differs from the binding's"));
        }
        let port_ok = match self.bound_port {
            Some(bound) => parsed.port_or_known_default() == Some(bound),
            // `Url::port()` is `None` for the scheme-default port, so this rejects exactly the
            // explicit non-default ports.
            None => is_loopback_host(&host) || parsed.port().is_none(),
        };
        if !port_ok {
            return Err(outside("port differs from the binding's"));
        }
        match parsed.scheme() {
            "https" => Ok(()),
            "http" if is_loopback_host(&host) => Ok(()),
            _ => Err(outside("requires https (http is loopback-only)")),
        }
    }

    /// Per-request timeout: the configured request timeout, tightened to the remaining pass
    /// budget (floored at one second) so an in-flight request cannot outlive the pass by more
    /// than a beat.
    fn request_timeout(&self, now_ms: i64) -> Duration {
        let remaining_ms = self.pass_deadline_ms.saturating_sub(now_ms).max(1_000);
        Duration::from_millis(self.request_timeout_ms.min(remaining_ms).max(1) as u64)
    }
}

/// Canonical quota-pool authority. Case was normalized by `parse_bound_authority`; an explicit
/// HTTPS default port names the same origin as no port, while non-default ports remain distinct.
fn canonical_governor_authority(host: &str, port: Option<u16>) -> String {
    match port {
        None | Some(443) => host.to_string(),
        Some(port) if host.contains(':') => format!("[{host}]:{port}"),
        Some(port) => format!("{host}:{port}"),
    }
}

/// Split a configured authority into (lowercased host, optional port). Accepts `host`,
/// `host:port`, `[v6]`, `[v6]:port`, and a bare IPv6 literal (multiple colons, no port).
/// Malformed input is an ERROR, never normalized — silently dropping a bad port would quietly
/// re-point the binding (and its token) at the host's scheme-default port.
fn parse_bound_authority(authority: &str) -> anyhow::Result<(String, Option<u16>)> {
    let authority = authority.trim();
    let parse_port = |port: &str| {
        port.parse::<u16>()
            .map_err(|_| anyhow::anyhow!("invalid port `{port}` in tracker host `{authority}`"))
    };
    if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            anyhow::bail!("unclosed `[` in tracker host `{authority}`");
        };
        anyhow::ensure!(!host.is_empty(), "empty host in tracker host `{authority}`");
        let port = match tail.strip_prefix(':') {
            Some(port) => Some(parse_port(port)?),
            None if tail.is_empty() => None,
            None => anyhow::bail!("unexpected `{tail}` after `]` in tracker host `{authority}`"),
        };
        return Ok((host.to_ascii_lowercase(), port));
    }
    match authority.split_once(':') {
        Some((host, port)) if !port.contains(':') => {
            anyhow::ensure!(!host.is_empty(), "empty host in tracker host `{authority}`");
            Ok((host.to_ascii_lowercase(), Some(parse_port(port)?)))
        },
        _ => {
            anyhow::ensure!(!authority.is_empty(), "tracker host is empty");
            // A colon here means a bare IPv6 literal — anything else with colons is malformed.
            anyhow::ensure!(
                !authority.contains(':') || authority.parse::<std::net::Ipv6Addr>().is_ok(),
                "malformed tracker host `{authority}`"
            );
            Ok((authority.to_ascii_lowercase(), None))
        },
    }
}

/// Loopback spellings a binding/test can use; these allow plain http and arbitrary ports.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Whether a response is a rate-limit signal. `429` always; GitHub reports its primary limit as
/// `403` + `x-ratelimit-remaining: 0` and its secondary limits as `403` + `Retry-After`, so
/// those count too — still pure header semantics, no provider branching.
fn is_rate_limited(status: u16, headers: &HeaderMap, now_ms: i64) -> bool {
    match status {
        429 => true,
        403 =>
            headers.contains_key(header::RETRY_AFTER)
                || QuotaSnapshot::from_headers(headers, now_ms)
                    .is_some_and(|quota| quota.remaining == 0),
        _ => false,
    }
}

fn is_github_secondary_limit(status: u16, body: &str) -> bool {
    if status != 403 && status != 429 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("secondary rate limit") || body.contains("abuse detection")
}

/// `Retry-After` in milliseconds: delta-seconds or the HTTP-date form required by RFC 9110.
fn retry_after_ms(headers: &HeaderMap, now_ms: i64) -> Option<i64> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Some(seconds) = value.parse::<i64>().ok().filter(|seconds| *seconds >= 0) {
        return Some(seconds.saturating_mul(1000));
    }
    let target_ms = i64::try_from(
        httpdate::parse_http_date(value).ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis(),
    )
    .ok()?;
    Some(target_ms.saturating_sub(now_ms).max(0))
}

#[cfg(test)]
mod tests {
    use super::super::governor;
    use super::super::stub::{StubResponse, spawn_script_stub};
    use super::*;

    /// Drive a transport future on a current-thread runtime — the same flavor the papertrail
    /// `block_on` bridge uses in production.
    fn drive<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    fn transport(url: &str, auth: Option<&TrackerAuth>, options: TransportOptions) -> Transport {
        let registry = GovernorRegistry::default();
        transport_in_lane(url, auth, options, &registry, "core")
    }

    fn transport_in_lane(
        url: &str,
        auth: Option<&TrackerAuth>,
        options: TransportOptions,
        registry: &GovernorRegistry,
        lane: &str,
    ) -> Transport {
        let host = url.trim_start_matches("http://").split(':').next().unwrap().to_string();
        Transport::new(TransportParams {
            provider: "github",
            lane,
            host: &host,
            auth,
            registry,
            options,
        })
        .expect("transport")
    }

    fn fast_options() -> TransportOptions {
        TransportOptions { backoff_base_ms: 10, ..TransportOptions::default() }
    }

    #[test]
    fn papertrail_config_sets_the_governor_reserve() {
        let config = PapertrailConfig { rate_limit_reserve: 0.2, ..PapertrailConfig::default() };
        let options = TransportOptions::from(&config);
        assert_eq!(options.governor.reserve, 0.2);
        assert_eq!(
            options.governor.fallback_budget.max_requests,
            TransportOptions::default().governor.fallback_budget.max_requests
        );
    }

    #[test]
    fn authenticated_github_conditional_304_refunds_the_local_budget() {
        let (url, handle) = spawn_script_stub(vec![
            StubResponse::status("304 Not Modified", ""),
            StubResponse::ok(r#"{"ok":true}"#),
        ]);
        let mut options = fast_options();
        options.governor.fallback_budget =
            governor::BudgetPolicy { max_requests: 1, window_ms: 60_000 };
        let auth = TrackerAuth::TokenCommand("echo stub-token".to_string());
        let transport = transport(&url, Some(&auth), options);
        drive(async {
            let first = transport
                .get(&format!("{url}/probe"), &[(header::IF_NONE_MATCH.as_str(), "\"v1\"")])
                .await
                .unwrap();
            assert_eq!(first.status, 304);
            let second = transport.get(&format!("{url}/items"), &[]).await.unwrap();
            assert_eq!(second.status, 200);
        });
        assert_eq!(handle.join().unwrap().len(), 2);
    }

    #[test]
    fn get_sends_auth_and_extra_headers() {
        let (url, handle) = spawn_script_stub(vec![StubResponse::ok(r#"{"ok":true}"#)]);
        let auth = TrackerAuth::TokenCommand("echo stub-token".to_string());
        let transport = transport(&url, Some(&auth), fast_options());
        let response = drive(async {
            transport.get(&format!("{url}/items"), &[("accept", "application/json")]).await
        })
        .expect("success");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, r#"{"ok":true}"#);
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 1);
        let head = requests[0].to_ascii_lowercase();
        assert!(head.starts_with("get /items http/1.1"), "{head}");
        assert!(head.contains("authorization: bearer stub-token"), "{head}");
        assert!(head.contains("accept: application/json"), "{head}");
        assert!(head.contains("user-agent: rag-rat/"), "{head}");
    }

    #[test]
    fn a_429_is_retried_with_backoff_and_succeeds() {
        let (url, handle) = spawn_script_stub(vec![
            StubResponse::status("429 Too Many Requests", "slow down"),
            StubResponse::ok("recovered"),
        ]);
        let transport = transport(&url, None, fast_options());
        let response =
            drive(async { transport.get(&format!("{url}/items"), &[]).await }).expect("retried");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "recovered");
        assert_eq!(handle.join().unwrap().len(), 2, "one retry after the 429");
    }

    #[test]
    fn a_retry_after_beyond_the_pass_budget_pauses_instead_of_sleeping() {
        let (url, handle) = spawn_script_stub(vec![StubResponse {
            status: "429 Too Many Requests",
            headers: vec![("retry-after".into(), "3600".into())],
            body: "later".to_string(),
        }]);
        let options = TransportOptions { pass_budget_ms: 1_000, ..fast_options() };
        let transport = transport(&url, None, options);
        let before = now_ms();
        let err = drive(async { transport.get(&format!("{url}/items"), &[]).await })
            .expect_err("must pause");
        let (resume_at_ms, reason) = err.pause_details().expect("expected Paused");
        assert_eq!(reason, PauseReason::RetryAfter);
        assert!(
            resume_at_ms >= before + 3_600_000,
            "resume honors Retry-After: {resume_at_ms} vs {before}"
        );
        assert_eq!(handle.join().unwrap().len(), 1, "no retry once the budget is overrun");
        // The hold covers the whole key: the next call stands down WITHOUT touching the network
        // (the stub has no scripted responses left — a request would fail, not pause).
        let second = drive(async { transport.get(&format!("{url}/items"), &[]).await })
            .expect_err("still held");
        assert!(matches!(second, TransportError::Paused { .. }), "{second:?}");
    }

    #[test]
    fn a_secondary_403_with_retry_after_is_rate_limited() {
        let (url, handle) = spawn_script_stub(vec![
            StubResponse {
                status: "403 Forbidden",
                headers: vec![("retry-after".into(), "0".into())],
                body: "secondary limit".to_string(),
            },
            StubResponse::ok("recovered"),
        ]);
        let transport = transport(&url, None, fast_options());
        let response =
            drive(async { transport.get(&format!("{url}/items"), &[]).await }).expect("retried");
        assert_eq!(response.body, "recovered");
        assert_eq!(handle.join().unwrap().len(), 2);
    }

    #[test]
    fn a_body_only_github_secondary_limit_pauses_for_at_least_a_minute() {
        let (url, handle) = spawn_script_stub(vec![StubResponse::status(
            "403 Forbidden",
            "You have exceeded a secondary rate limit.",
        )]);
        let options = TransportOptions { pass_budget_ms: 1_000, ..fast_options() };
        let transport = transport(&url, None, options);
        let before = now_ms();
        let err = drive(async { transport.get(&format!("{url}/items"), &[]).await })
            .expect_err("body-only secondary limit must pause");
        let (resume_at_ms, reason) = err.pause_details().expect("expected Paused");
        assert_eq!(reason, PauseReason::RetryAfter);
        assert!(resume_at_ms >= before + 60_000);
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn a_github_secondary_hold_stops_sibling_quota_lanes_before_they_send() {
        let (url, handle) = spawn_script_stub(vec![StubResponse::status(
            "403 Forbidden",
            "You have exceeded a secondary rate limit.",
        )]);
        let options = TransportOptions { pass_budget_ms: 1_000, ..fast_options() };
        let registry = GovernorRegistry::default();
        let core = transport_in_lane(&url, None, options.clone(), &registry, "core");
        let search = transport_in_lane(&url, None, options, &registry, "search");

        let core_error = drive(async { core.get(&format!("{url}/issues"), &[]).await })
            .expect_err("secondary limit pauses core");
        assert!(matches!(core_error, TransportError::Paused { .. }));
        let search_error = drive(async { search.get(&format!("{url}/search"), &[]).await })
            .expect_err("shared secondary hold pauses search");
        assert!(matches!(search_error, TransportError::Paused { .. }));
        assert_eq!(handle.join().unwrap().len(), 1, "search never touched the network");
    }

    #[test]
    fn a_plain_403_is_returned_to_the_caller_not_retried() {
        let (url, handle) =
            spawn_script_stub(vec![StubResponse::status("403 Forbidden", "no access")]);
        let transport = transport(&url, None, fast_options());
        let response = drive(async { transport.get(&format!("{url}/items"), &[]).await })
            .expect("statuses are the provider's business");
        assert_eq!(response.status, 403);
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn requests_outside_the_bound_authority_are_refused_before_any_send() {
        let registry = GovernorRegistry::default();
        let transport = Transport::new(TransportParams {
            provider: "github",
            lane: "core",
            host: "api.github.com",
            auth: None,
            registry: &registry,
            options: TransportOptions::default(),
        })
        .expect("transport");
        // No stub, no network: every one of these must be refused by the URL guard alone.
        for (url, why) in [
            ("https://evil.example.com/items", "foreign host"),
            ("https://api.github.com.evil.example.com/items", "suffix-spoofed host"),
            ("http://api.github.com/items", "plaintext to a non-loopback host"),
            ("https://api.github.com:8443/items", "non-default port without a bound one"),
            ("not a url", "unparseable"),
        ] {
            let err = drive(async { transport.get(url, &[]).await }).expect_err(why);
            assert!(matches!(err, TransportError::UrlOutsideBinding { .. }), "{why}: {err}");
            assert_eq!(err.pause_details(), None);
        }
    }

    #[test]
    fn redirects_are_returned_without_following_an_unguarded_hop() {
        let (url, origin) = spawn_script_stub(vec![StubResponse {
            status: "302 Found",
            // Port 9 has no test server: following this hop would fail the request.
            headers: vec![("location".into(), "http://127.0.0.1:9/secret".into())],
            body: String::new(),
        }]);
        let transport = transport(&url, None, fast_options());
        let response = drive(async { transport.get(&format!("{url}/items"), &[]).await })
            .expect("redirect is provider semantics");
        assert_eq!(response.status, 302);
        assert_eq!(origin.join().unwrap().len(), 1);
    }

    #[test]
    fn equivalent_authority_spellings_share_one_governor() {
        let registry = GovernorRegistry::default();
        let build = |host: &str| {
            Transport::new(TransportParams {
                provider: "github",
                lane: "core",
                host,
                auth: None,
                registry: &registry,
                options: TransportOptions::default(),
            })
            .expect("transport")
        };
        let lowercase = build("api.github.com");
        let uppercase_default_port = build("API.GitHub.com:443");
        assert!(Arc::ptr_eq(&lowercase.governor, &uppercase_default_port.governor));
    }

    #[test]
    fn a_bound_port_pins_requests_to_that_port() {
        let registry = GovernorRegistry::default();
        let transport = Transport::new(TransportParams {
            provider: "gitlab",
            lane: "core",
            host: "gitlab.example.com:8443",
            auth: None,
            registry: &registry,
            options: TransportOptions::default(),
        })
        .expect("transport");
        let err = drive(async { transport.get("https://gitlab.example.com/x", &[]).await })
            .expect_err("default port differs from the bound 8443");
        assert!(matches!(err, TransportError::UrlOutsideBinding { .. }), "{err}");
    }

    #[test]
    fn an_expired_pass_budget_pauses_before_any_send() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let (url, handle) = spawn_script_stub(vec![]);
        let now = Arc::new(AtomicI64::new(1_700_000_000_000));
        let clock = {
            let now = Arc::clone(&now);
            Arc::new(move || now.load(Ordering::SeqCst))
        };
        let registry = GovernorRegistry::default();
        let transport = Transport::with_clock(
            TransportParams {
                provider: "github",
                lane: "core",
                host: "127.0.0.1",
                auth: None,
                registry: &registry,
                options: TransportOptions { pass_budget_ms: 1_000, ..fast_options() },
            },
            clock,
        )
        .expect("transport");
        now.fetch_add(1_001, Ordering::SeqCst);
        let err = drive(async { transport.get(&format!("{url}/items"), &[]).await })
            .expect_err("the pass budget is spent");
        assert!(
            matches!(err, TransportError::Paused { reason: PauseReason::PassBudget, .. }),
            "{err:?}"
        );
        assert!(handle.join().unwrap().is_empty(), "nothing was sent past the deadline");
    }

    #[test]
    fn bound_authority_parsing_accepts_valid_forms_and_rejects_malformed_ones() {
        assert_eq!(
            parse_bound_authority("API.GitHub.com").unwrap(),
            ("api.github.com".to_string(), None)
        );
        assert_eq!(
            parse_bound_authority("gitlab.example.com:8443").unwrap(),
            ("gitlab.example.com".to_string(), Some(8443))
        );
        assert_eq!(parse_bound_authority("[::1]:9000").unwrap(), ("::1".to_string(), Some(9000)));
        assert_eq!(parse_bound_authority("[::1]").unwrap(), ("::1".to_string(), None));
        assert_eq!(parse_bound_authority("::1").unwrap(), ("::1".to_string(), None));
        // Malformed authorities ERROR instead of silently re-pointing the binding at the
        // scheme-default port.
        for bad in [
            "gitlab.example.com:not-a-port",
            "gitlab.example.com:",
            "gitlab.example.com:70000",
            "[::1",
            "[::1]x",
            "[::1]:bad",
            "[]",
            ":8443",
            "a:1:2",
            "",
        ] {
            assert!(parse_bound_authority(bad).is_err(), "`{bad}` must be rejected");
        }
    }

    #[test]
    fn a_malformed_bound_authority_fails_construction() {
        let registry = GovernorRegistry::default();
        let err = Transport::new(TransportParams {
            provider: "gitlab",
            lane: "core",
            host: "gitlab.example.com:not-a-port",
            auth: None,
            registry: &registry,
            options: TransportOptions::default(),
        })
        .err()
        .expect("a malformed authority must fail fast");
        assert!(err.to_string().contains("not-a-port"), "{err}");
    }

    #[test]
    fn a_configured_but_missing_token_fails_construction() {
        let registry = GovernorRegistry::default();
        let err = Transport::new(TransportParams {
            provider: "github",
            lane: "core",
            host: "api.github.com",
            auth: Some(&TrackerAuth::Env("RAG_RAT_TEST_UNSET_TOKEN_VAR".to_string())),
            registry: &registry,
            options: TransportOptions::default(),
        })
        .err()
        .expect("fail fast, never degrade to anonymous");
        assert!(err.to_string().contains("RAG_RAT_TEST_UNSET_TOKEN_VAR"), "{err}");
    }

    #[test]
    fn retry_after_accepts_delta_seconds_and_http_dates() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, "60".parse().unwrap());
        assert_eq!(retry_after_ms(&headers, 1_700_000_000_000), Some(60_000));

        headers.insert(header::RETRY_AFTER, "Tue, 14 Nov 2023 22:14:20 GMT".parse().unwrap());
        assert_eq!(retry_after_ms(&headers, 1_700_000_000_000), Some(60_000));
    }
}
