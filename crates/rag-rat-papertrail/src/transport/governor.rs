//! Rate governance for the papertrail transport, one governor per
//! (provider, host, token, provider quota lane).
//!
//! Header-driven where the provider reports quota (GitHub `x-ratelimit-*`, GitLab `RateLimit-*`):
//! after each response the governor updates its view and stops consuming once
//! `remaining <= reserve × limit` — the reserved fraction (default 35%) stays untouched for the
//! user's own CLI tools on the same token. Budget-driven fallback where headers are absent
//! (Bitbucket-style per-hour endpoint-group quotas): a conservative fixed request budget per
//! window. A `429`/`Retry-After` hold always wins over both.
//!
//! The governor is a pure state machine over injected wall-clock instants (`now_ms` parameters),
//! so every pause/resume path is deterministic under test; the transport supplies real time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use reqwest::header::HeaderMap;

/// Fraction of a header-reported quota kept for the user's own tools on the same token: the
/// governor stops consuming once `remaining <= reserve * limit`.
pub(crate) const DEFAULT_RATE_LIMIT_RESERVE: f64 = 0.35;

/// Resume horizon when a reserve pause has no `*-reset` header to anchor to: check back after a
/// minute — the next response's headers re-establish the real window.
const RESET_FALLBACK_MS: i64 = 60_000;

/// Delta reset headers are anchored to each response's local receive time, so responses from the
/// same provider window can differ slightly through latency/rounding. Treat nearby horizons as
/// one window and ratchet quota down; real adjacent windows are minutes or hours apart.
const RESET_DRIFT_TOLERANCE_MS: u64 = 10_000;

/// `*-reset` values at or above this are Unix-epoch seconds (GitHub, GitLab); below it they are
/// delta seconds (the IETF RateLimit draft form). The floor is ~2001-09-09 — no live reset
/// timestamp is older, and no sane delta is longer.
const EPOCH_SECONDS_FLOOR: i64 = 1_000_000_000;

/// Identity of one governed quota pool. Two bindings that talk to the same host with the same
/// token share a governor, so their consumption is jointly accounted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GovernorKey {
    /// Provider id (`github`, `gitlab`, …) — distinguishes co-hosted APIs behind one host.
    pub provider: String,
    /// Host the binding talks to (`api.github.com`, a self-hosted GitLab).
    pub host: String,
    /// Fingerprint of the resolved token — never the token itself; `anonymous` without one.
    pub token_fingerprint: String,
    /// Provider quota resource (`core`, `search`, ...). GitHub reports these as independent
    /// windows; mixing their response headers would let one lane corrupt the other's budget.
    pub lane: String,
}

impl GovernorKey {
    pub fn new(provider: &str, host: &str, token: Option<&str>, lane: &str) -> Self {
        Self {
            provider: provider.to_string(),
            host: host.to_string(),
            token_fingerprint: token_fingerprint(token),
            lane: lane.to_string(),
        }
    }

    fn retry_hold_key(&self) -> RetryHoldKey {
        RetryHoldKey {
            provider: self.provider.clone(),
            host: self.host.clone(),
            token_fingerprint: self.token_fingerprint.clone(),
        }
    }
}

/// Secondary/abuse limits are scoped more broadly than provider primary-quota lanes. This key
/// deliberately omits `lane`, so core and search transports for one host/token stand down
/// together without mixing their independent quota headers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RetryHoldKey {
    provider: String,
    host: String,
    token_fingerprint: String,
}

#[derive(Debug, Default)]
pub(crate) struct RetryHold {
    until_ms: Mutex<Option<i64>>,
}

impl RetryHold {
    pub(crate) fn paused_until(&self, now_ms: i64) -> Option<i64> {
        let mut hold = self.until_ms.lock().expect("retry-hold mutex");
        match *hold {
            Some(until) if now_ms < until => Some(until),
            Some(_) => {
                *hold = None;
                None
            },
            None => None,
        }
    }

    pub(crate) fn record(&self, until_ms: i64) {
        let mut hold = self.until_ms.lock().expect("retry-hold mutex");
        *hold = Some(hold.map_or(until_ms, |current| current.max(until_ms)));
    }
}

/// Short sha256 fingerprint so the key never holds a copy of the secret. Eight bytes of digest
/// are plenty to tell two tokens apart in an in-process map.
fn token_fingerprint(token: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    match token.map(str::trim).filter(|t| !t.is_empty()) {
        None => "anonymous".to_string(),
        Some(token) =>
            Sha256::digest(token.as_bytes())[..8].iter().map(|b| format!("{b:02x}")).collect(),
    }
}

/// Fixed request budget for providers that report no quota headers: at most `max_requests` per
/// `window_ms`, then pause until the window rolls.
#[derive(Debug, Clone, Copy)]
pub struct BudgetPolicy {
    pub max_requests: u32,
    pub window_ms: i64,
}

/// Conservative default: 300 requests/hour — well under Bitbucket's 1000/hour endpoint-group
/// quotas even with several bindings sharing a workspace token.
pub(crate) const DEFAULT_FALLBACK_BUDGET: BudgetPolicy =
    BudgetPolicy { max_requests: 300, window_ms: 3_600_000 };

#[derive(Debug, Clone)]
pub struct GovernorConfig {
    /// Stop-consuming threshold: paused while `remaining <= reserve * limit`.
    pub reserve: f64,
    /// Applied only while no header-reported quota view exists.
    pub fallback_budget: BudgetPolicy,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self { reserve: DEFAULT_RATE_LIMIT_RESERVE, fallback_budget: DEFAULT_FALLBACK_BUDGET }
    }
}

/// One header-reported quota view. GitHub (`x-ratelimit-*`) and GitLab (`RateLimit-*`) report the
/// same triple; the un-prefixed spelling also covers IETF-draft providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuotaSnapshot {
    pub limit: i64,
    pub remaining: i64,
    /// Wall-clock instant the provider window resets; `None` when the provider omits it.
    pub reset_at_ms: Option<i64>,
}

impl QuotaSnapshot {
    /// Parse a quota view out of response headers; `None` when the provider reports none (the
    /// budget fallback governs then). `limit` and `remaining` are required and must be sane;
    /// `reset` is optional and accepted as epoch seconds or delta seconds (see
    /// [`EPOCH_SECONDS_FLOOR`]).
    pub(crate) fn from_headers(headers: &HeaderMap, now_ms: i64) -> Option<Self> {
        let read = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<i64>().ok();
        let quota_header = |suffix: &str| {
            read(&format!("x-ratelimit-{suffix}")).or_else(|| read(&format!("ratelimit-{suffix}")))
        };
        let limit = quota_header("limit").filter(|limit| *limit > 0)?;
        let remaining = quota_header("remaining").filter(|remaining| *remaining >= 0)?;
        let reset_at_ms = quota_header("reset").filter(|reset| *reset >= 0).map(|reset| {
            if reset >= EPOCH_SECONDS_FLOOR {
                reset.saturating_mul(1000)
            } else {
                now_ms.saturating_add(reset.saturating_mul(1000))
            }
        });
        Some(Self { limit, remaining, reset_at_ms })
    }
}

/// Why an [`Admission`] came back paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PauseReason {
    /// A `429`/`Retry-After` hold (or a backoff that would overrun the sync pass's budget).
    RetryAfter,
    /// Header-reported quota is at/below the user reserve; resume when the window resets.
    QuotaReserve,
    /// The headerless fallback budget for this window is spent.
    RequestBudget,
    /// The sync pass's wall-clock budget is spent; the next pass resumes from the kept cursor.
    PassBudget,
}

impl PauseReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RetryAfter => "retry_after",
            Self::QuotaReserve => "quota_reserve",
            Self::RequestBudget => "request_budget",
            Self::PassBudget => "pass_budget",
        }
    }
}

impl std::fmt::Display for PauseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Admission decision for one outbound request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    Proceed,
    /// Stand down until `resume_at_ms`. The caller keeps its pagination cursor — the next sync
    /// window resumes mid-pagination, nothing is dropped or refetched.
    PausedUntil {
        resume_at_ms: i64,
        reason: PauseReason,
    },
}

#[derive(Debug, Default)]
struct GovernorState {
    /// Latest header-reported quota; `None` until a response carries quota headers (and again
    /// after a view goes stale past its reset).
    quota: Option<QuotaSnapshot>,
    /// Fallback-budget window accounting; consulted only while `quota` is `None`.
    window_started_at_ms: Option<i64>,
    window_requests: u32,
    /// Hard hold from a `429`/`Retry-After`; wins over both quota and budget until it expires.
    hold_until_ms: Option<i64>,
}

/// Shared, thread-safe rate governor for one [`GovernorKey`]. All decisions run against caller-
/// supplied `now_ms` instants — the governor itself never reads the clock.
#[derive(Debug)]
pub(crate) struct RateGovernor {
    config: GovernorConfig,
    state: Mutex<GovernorState>,
}

impl RateGovernor {
    pub fn new(config: GovernorConfig) -> Self {
        Self { config, state: Mutex::new(GovernorState::default()) }
    }

    /// Decide whether one request may go out now. Every `Proceed` permanently debits the active
    /// local quota view before returning: a timed-out send may still have consumed server-side
    /// quota, and concurrent siblings must not all spend the same reported `remaining` slot.
    pub(crate) fn admit(&self, now_ms: i64) -> Admission {
        let mut state = self.state.lock().expect("governor mutex");
        if let Some(hold) = state.hold_until_ms {
            if now_ms < hold {
                return Admission::PausedUntil {
                    resume_at_ms: hold,
                    reason: PauseReason::RetryAfter,
                };
            }
            state.hold_until_ms = None;
        }
        if let Some(quota) = state.quota {
            match quota.reset_at_ms {
                // A view whose window already reset is stale: drop it and fall through to the
                // budget lane; the next response re-establishes the real remaining.
                Some(reset) if now_ms >= reset => state.quota = None,
                _ => {
                    let reserved_requests =
                        (self.config.reserve * quota.limit as f64).ceil() as i64;
                    // Judge the state AFTER this admission: fractional reserves round up, so a
                    // limit of 99 at 35% keeps 35 whole requests, never 34.
                    if quota.remaining == 0 || quota.remaining.saturating_sub(1) < reserved_requests
                    {
                        // No reset header to anchor the pause: materialize a short horizon into
                        // the stored view so the stale-view drop above ends the pause instead of
                        // it renewing forever.
                        let resume_at_ms = quota.reset_at_ms.unwrap_or_else(|| {
                            let resume = now_ms + RESET_FALLBACK_MS;
                            if let Some(view) = state.quota.as_mut() {
                                view.reset_at_ms = Some(resume);
                            }
                            resume
                        });
                        return Admission::PausedUntil {
                            resume_at_ms,
                            reason: PauseReason::QuotaReserve,
                        };
                    }
                    // Debit at admission, not completion. Response snapshots merge monotonically
                    // below, so failures without headers cannot refund a possibly consumed call.
                    if let Some(view) = state.quota.as_mut() {
                        view.remaining = view.remaining.saturating_sub(1);
                    }
                    return Admission::Proceed;
                },
            }
        }
        // Budget fallback: no live header view — count requests against the fixed window.
        let window_ms = self.config.fallback_budget.window_ms.max(1);
        let window_start = match state.window_started_at_ms {
            Some(start) if now_ms.saturating_sub(start) < window_ms => start,
            _ => {
                state.window_started_at_ms = Some(now_ms);
                state.window_requests = 0;
                now_ms
            },
        };
        if state.window_requests >= self.config.fallback_budget.max_requests {
            return Admission::PausedUntil {
                resume_at_ms: window_start + window_ms,
                reason: PauseReason::RequestBudget,
            };
        }
        state.window_requests += 1;
        Admission::Proceed
    }

    /// Undo one completed admission that the provider explicitly documents as quota-free (GitHub
    /// authenticated conditional `304`). This is never used for failed/timed-out requests.
    pub(crate) fn refund_admission(&self) {
        let mut state = self.state.lock().expect("governor mutex");
        if let Some(quota) = state.quota.as_mut() {
            quota.remaining = quota.remaining.saturating_add(1).min(quota.limit);
        } else {
            state.window_requests = state.window_requests.saturating_sub(1);
        }
    }

    /// Record the quota view a response reported. Merged monotonically: within the same provider
    /// window `remaining` only ratchets DOWN — an out-of-order older response cannot raise it
    /// back. Resetless views and nearby reset horizons are the same live window; an older window
    /// is dropped and a genuinely newer window replaces the view. Never clears a `429` hold.
    pub(crate) fn record_quota(&self, snapshot: QuotaSnapshot) {
        let mut state = self.state.lock().expect("governor mutex");
        state.quota = Some(match state.quota {
            Some(current) => match (current.reset_at_ms, snapshot.reset_at_ms) {
                (Some(current_reset), Some(new_reset))
                    if new_reset.abs_diff(current_reset) <= RESET_DRIFT_TOLERANCE_MS =>
                    merge_quota(current, snapshot, Some(current_reset.max(new_reset))),
                (Some(current_reset), Some(new_reset)) if new_reset < current_reset => current,
                (Some(_), Some(_)) => snapshot,
                (None, None) => merge_quota(current, snapshot, None),
                (Some(reset), None) => merge_quota(current, snapshot, Some(reset)),
                (None, Some(reset)) => merge_quota(current, snapshot, Some(reset)),
            },
            None => snapshot,
        });
    }

    /// Record a `429`/`Retry-After` hold: nothing on this key goes out before `until_ms`. Holds
    /// only extend — a shorter later hold never shortens an earlier one.
    pub(crate) fn record_hold(&self, until_ms: i64) {
        let mut state = self.state.lock().expect("governor mutex");
        state.hold_until_ms = Some(state.hold_until_ms.map_or(until_ms, |h| h.max(until_ms)));
    }
}

fn merge_quota(
    current: QuotaSnapshot,
    snapshot: QuotaSnapshot,
    reset_at_ms: Option<i64>,
) -> QuotaSnapshot {
    QuotaSnapshot {
        limit: current.limit.max(snapshot.limit),
        remaining: current.remaining.min(snapshot.remaining),
        reset_at_ms,
    }
}

/// Delay before retry `attempt` (0-based) of a rate-limited request: exponential from `base_ms`,
/// but a server-provided `Retry-After` always wins when longer. The shift is clamped so a long
/// retry storm cannot overflow — the per-pass wall-clock cap ends it far earlier anyway.
pub(crate) fn backoff_delay_ms(attempt: u32, retry_after_ms: Option<i64>, base_ms: i64) -> i64 {
    let exponential = base_ms.max(1).saturating_mul(1_i64 << attempt.min(20));
    exponential.max(retry_after_ms.unwrap_or(0))
}

/// Process-wide registry: hands out the shared governor for a key, creating it on first sight
/// with the caller's config (later callers on the same key inherit the existing governor).
#[derive(Debug, Default)]
pub(crate) struct GovernorRegistry {
    inner: Mutex<HashMap<GovernorKey, Arc<RateGovernor>>>,
    retry_holds: Mutex<HashMap<RetryHoldKey, Arc<RetryHold>>>,
}

impl GovernorRegistry {
    pub(crate) fn governor(&self, key: GovernorKey, config: &GovernorConfig) -> Arc<RateGovernor> {
        Arc::clone(
            self.inner
                .lock()
                .expect("governor registry mutex")
                .entry(key)
                .or_insert_with(|| Arc::new(RateGovernor::new(config.clone()))),
        )
    }

    pub(crate) fn retry_hold(&self, key: &GovernorKey) -> Arc<RetryHold> {
        Arc::clone(
            self.retry_holds
                .lock()
                .expect("retry-hold registry mutex")
                .entry(key.retry_hold_key())
                .or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    use super::*;

    const T0: i64 = 1_700_000_000_000;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                name.parse::<HeaderName>().expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        map
    }

    fn governor(reserve: f64, budget: BudgetPolicy) -> RateGovernor {
        RateGovernor::new(GovernorConfig { reserve, fallback_budget: budget })
    }

    #[test]
    fn pause_reasons_have_stable_machine_strings_and_display() {
        for (reason, expected) in [
            (PauseReason::RetryAfter, "retry_after"),
            (PauseReason::QuotaReserve, "quota_reserve"),
            (PauseReason::RequestBudget, "request_budget"),
            (PauseReason::PassBudget, "pass_budget"),
        ] {
            assert_eq!(reason.as_str(), expected);
            assert_eq!(reason.to_string(), expected);
        }
    }

    #[test]
    fn quota_snapshot_parses_github_style_headers() {
        let map = headers(&[
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-remaining", "4321"),
            ("x-ratelimit-reset", "1700000123"),
        ]);
        assert_eq!(
            QuotaSnapshot::from_headers(&map, T0),
            Some(QuotaSnapshot {
                limit: 5000,
                remaining: 4321,
                reset_at_ms: Some(1_700_000_123_000),
            })
        );
    }

    #[test]
    fn quota_snapshot_parses_gitlab_style_headers() {
        // GitLab spells them without the `x-` prefix (`RateLimit-*`); HeaderMap lookups are
        // case-insensitive, so the canonical lowercase names cover the wire form.
        let map = headers(&[
            ("ratelimit-limit", "600"),
            ("ratelimit-remaining", "599"),
            ("ratelimit-reset", "1700000060"),
        ]);
        assert_eq!(
            QuotaSnapshot::from_headers(&map, T0),
            Some(QuotaSnapshot {
                limit: 600,
                remaining: 599,
                reset_at_ms: Some(1_700_000_060_000)
            })
        );
    }

    #[test]
    fn quota_snapshot_treats_small_reset_as_delta_seconds() {
        // The IETF RateLimit draft form: `RateLimit-Reset: 30` means "in 30 seconds".
        let map = headers(&[
            ("ratelimit-limit", "100"),
            ("ratelimit-remaining", "50"),
            ("ratelimit-reset", "30"),
        ]);
        assert_eq!(QuotaSnapshot::from_headers(&map, T0).unwrap().reset_at_ms, Some(T0 + 30_000));
    }

    #[test]
    fn quota_snapshot_requires_sane_limit_and_remaining() {
        // Missing remaining.
        assert_eq!(
            QuotaSnapshot::from_headers(&headers(&[("x-ratelimit-limit", "100")]), T0),
            None
        );
        // Missing limit.
        assert_eq!(
            QuotaSnapshot::from_headers(&headers(&[("x-ratelimit-remaining", "10")]), T0),
            None
        );
        // Garbage values.
        assert_eq!(
            QuotaSnapshot::from_headers(
                &headers(&[("x-ratelimit-limit", "many"), ("x-ratelimit-remaining", "10")]),
                T0
            ),
            None
        );
        // Non-positive limit / negative remaining are not a usable view.
        assert_eq!(
            QuotaSnapshot::from_headers(
                &headers(&[("x-ratelimit-limit", "0"), ("x-ratelimit-remaining", "0")]),
                T0
            ),
            None
        );
        assert_eq!(
            QuotaSnapshot::from_headers(
                &headers(&[("x-ratelimit-limit", "100"), ("x-ratelimit-remaining", "-1")]),
                T0
            ),
            None
        );
        // No quota headers at all.
        assert_eq!(QuotaSnapshot::from_headers(&HeaderMap::new(), T0), None);
    }

    #[test]
    fn admit_pauses_at_the_reserve_threshold_and_not_above_it() {
        let governor = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        let reset = T0 + 60_000;
        // remaining 36 > 35 = 0.35 × 100 → proceed.
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 36,
            reset_at_ms: Some(reset),
        });
        assert_eq!(governor.admit(T0), Admission::Proceed);
        // remaining 35 <= 35 → paused until the provider window resets (threshold is <=).
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 35,
            reset_at_ms: Some(reset),
        });
        assert_eq!(governor.admit(T0), Admission::PausedUntil {
            resume_at_ms: reset,
            reason: PauseReason::QuotaReserve
        });
    }

    #[test]
    fn fractional_reserve_rounds_up_before_admitting() {
        let fractional = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        let reset = T0 + 60_000;
        fractional.record_quota(QuotaSnapshot {
            limit: 99,
            remaining: 35,
            reset_at_ms: Some(reset),
        });
        assert_eq!(fractional.admit(T0), Admission::PausedUntil {
            resume_at_ms: reset,
            reason: PauseReason::QuotaReserve,
        });
        fractional.record_quota(QuotaSnapshot {
            limit: 99,
            remaining: 36,
            reset_at_ms: Some(reset + 60_000),
        });
        assert_eq!(fractional.admit(T0), Admission::Proceed);

        let no_reserve = governor(0.0, DEFAULT_FALLBACK_BUDGET);
        no_reserve.record_quota(QuotaSnapshot {
            limit: 99,
            remaining: 0,
            reset_at_ms: Some(reset),
        });
        assert!(matches!(no_reserve.admit(T0), Admission::PausedUntil { .. }));
    }

    #[test]
    fn reserve_pause_resumes_once_the_window_resets() {
        let governor = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        let reset = T0 + 60_000;
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 10,
            reset_at_ms: Some(reset),
        });
        assert!(matches!(governor.admit(T0), Admission::PausedUntil { .. }));
        // At the reset instant the stale view is dropped and the request proceeds.
        assert_eq!(governor.admit(reset), Admission::Proceed);
    }

    #[test]
    fn reserve_pause_without_a_reset_header_materializes_a_resume_horizon() {
        let governor = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        governor.record_quota(QuotaSnapshot { limit: 100, remaining: 10, reset_at_ms: None });
        let expected_resume = T0 + RESET_FALLBACK_MS;
        assert_eq!(governor.admit(T0), Admission::PausedUntil {
            resume_at_ms: expected_resume,
            reason: PauseReason::QuotaReserve,
        });
        // The horizon sticks (re-asking earlier keeps the same resume, not a sliding one) …
        assert_eq!(governor.admit(T0 + 1), Admission::PausedUntil {
            resume_at_ms: expected_resume,
            reason: PauseReason::QuotaReserve,
        });
        // … and past it the pause ends instead of renewing forever.
        assert_eq!(governor.admit(expected_resume), Admission::Proceed);
    }

    #[test]
    fn budget_fallback_counts_requests_and_rolls_the_window() {
        let budget = BudgetPolicy { max_requests: 3, window_ms: 10_000 };
        let governor = governor(0.35, budget);
        for i in 0..3 {
            assert_eq!(governor.admit(T0 + i), Admission::Proceed, "request {i} within budget");
        }
        assert_eq!(governor.admit(T0 + 3), Admission::PausedUntil {
            resume_at_ms: T0 + 10_000,
            reason: PauseReason::RequestBudget,
        });
        // The next window admits again.
        assert_eq!(governor.admit(T0 + 10_000), Admission::Proceed);
    }

    #[test]
    fn a_live_header_view_supersedes_the_fallback_budget() {
        // Exhaust a tiny budget, then let headers arrive: header quota is the ground truth, so
        // requests proceed even though the local budget counter is spent.
        let governor = governor(0.35, BudgetPolicy { max_requests: 1, window_ms: 10_000 });
        assert_eq!(governor.admit(T0), Admission::Proceed);
        assert!(matches!(governor.admit(T0 + 1), Admission::PausedUntil { .. }));
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 90,
            reset_at_ms: Some(T0 + 60_000),
        });
        assert_eq!(governor.admit(T0 + 2), Admission::Proceed);
    }

    #[test]
    fn a_retry_after_hold_wins_over_healthy_quota_and_expires() {
        let governor = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 90,
            reset_at_ms: Some(T0 + 60_000),
        });
        governor.record_hold(T0 + 5_000);
        assert_eq!(governor.admit(T0), Admission::PausedUntil {
            resume_at_ms: T0 + 5_000,
            reason: PauseReason::RetryAfter
        });
        // A shorter later hold never shortens the earlier one.
        governor.record_hold(T0 + 1_000);
        assert_eq!(governor.admit(T0), Admission::PausedUntil {
            resume_at_ms: T0 + 5_000,
            reason: PauseReason::RetryAfter
        });
        assert_eq!(governor.admit(T0 + 5_000), Admission::Proceed);
    }

    #[test]
    fn admissions_retain_their_quota_debit_without_a_response_snapshot() {
        // 37 remaining, threshold 35: only TWO requests may be admitted — a third would spend
        // the reserve. Completion without a newer snapshot must not refund either debit.
        let governor = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        let reset = T0 + 60_000;
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 37,
            reset_at_ms: Some(reset),
        });
        assert_eq!(governor.admit(T0), Admission::Proceed, "effective 37");
        assert_eq!(governor.admit(T0), Admission::Proceed, "effective 36");
        assert!(
            matches!(governor.admit(T0), Admission::PausedUntil {
                reason: PauseReason::QuotaReserve,
                ..
            }),
            "effective 35 <= reserve — a concurrent third must stand down"
        );
        // An out-of-order/header-only completion cannot raise the local view or reopen admission.
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 37,
            reset_at_ms: Some(reset),
        });
        assert!(matches!(governor.admit(T0 + 1), Admission::PausedUntil { .. }));

        // A genuinely new provider window replaces the debited view and restores capacity.
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 100,
            reset_at_ms: Some(reset + 60_000),
        });
        assert_eq!(governor.admit(T0 + 1), Admission::Proceed);
    }

    #[test]
    fn same_window_snapshots_merge_monotonically_and_new_windows_replace() {
        let governor = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        let reset = T0 + 60_000;
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 30,
            reset_at_ms: Some(reset),
        });
        // An out-of-order OLDER response from the same window cannot raise `remaining` back.
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 80,
            reset_at_ms: Some(reset),
        });
        assert!(matches!(governor.admit(T0), Admission::PausedUntil {
            reason: PauseReason::QuotaReserve,
            ..
        }));
        // A late snapshot from a PREVIOUS window (earlier reset) is dropped entirely.
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 90,
            reset_at_ms: Some(reset - 30_000),
        });
        assert!(matches!(governor.admit(T0), Admission::PausedUntil { .. }));
        // A NEWER window replaces the view outright.
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 95,
            reset_at_ms: Some(reset + 3_600_000),
        });
        assert_eq!(governor.admit(T0), Admission::Proceed);
    }

    #[test]
    fn resetless_and_nearby_delta_reset_snapshots_merge_monotonically() {
        let resetless = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        resetless.record_quota(QuotaSnapshot { limit: 100, remaining: 30, reset_at_ms: None });
        resetless.record_quota(QuotaSnapshot { limit: 100, remaining: 80, reset_at_ms: None });
        assert!(matches!(resetless.admit(T0), Admission::PausedUntil { .. }));

        let governor = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 30,
            reset_at_ms: Some(T0 + 60_000),
        });
        // Same provider window, but a delayed delta-reset response computes a horizon 3s later.
        governor.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 80,
            reset_at_ms: Some(T0 + 63_000),
        });
        assert!(matches!(governor.admit(T0), Admission::PausedUntil { .. }));
    }

    #[test]
    fn resetless_and_reset_anchored_snapshots_share_the_live_window() {
        let reset = T0 + 60_000;
        let anchored_first = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        anchored_first.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 30,
            reset_at_ms: Some(reset),
        });
        anchored_first.record_quota(QuotaSnapshot { limit: 100, remaining: 80, reset_at_ms: None });
        assert_eq!(anchored_first.admit(T0), Admission::PausedUntil {
            resume_at_ms: reset,
            reason: PauseReason::QuotaReserve,
        });

        let resetless_first = governor(0.35, DEFAULT_FALLBACK_BUDGET);
        resetless_first.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 30,
            reset_at_ms: None,
        });
        resetless_first.record_quota(QuotaSnapshot {
            limit: 100,
            remaining: 80,
            reset_at_ms: Some(reset),
        });
        assert_eq!(resetless_first.admit(T0), Admission::PausedUntil {
            resume_at_ms: reset,
            reason: PauseReason::QuotaReserve,
        });
    }

    #[test]
    fn refund_restores_header_quota_without_exceeding_the_limit() {
        let governor = governor(0.0, DEFAULT_FALLBACK_BUDGET);
        governor.record_quota(QuotaSnapshot { limit: 10, remaining: 8, reset_at_ms: None });
        assert_eq!(governor.admit(T0), Admission::Proceed);
        governor.refund_admission();
        governor.refund_admission();
        governor.refund_admission();
        let state = governor.state.lock().unwrap();
        assert_eq!(state.quota.as_ref().unwrap().remaining, 10);
    }

    #[test]
    fn backoff_grows_exponentially_and_retry_after_wins_when_longer() {
        assert_eq!(backoff_delay_ms(0, None, 1_000), 1_000);
        assert_eq!(backoff_delay_ms(1, None, 1_000), 2_000);
        assert_eq!(backoff_delay_ms(2, None, 1_000), 4_000);
        // Retry-After longer than the schedule → Retry-After wins.
        assert_eq!(backoff_delay_ms(0, Some(30_000), 1_000), 30_000);
        // Retry-After shorter than the schedule → the schedule holds the floor.
        assert_eq!(backoff_delay_ms(3, Some(1_000), 1_000), 8_000);
        // A zero base can never spin a hot retry loop.
        assert!(backoff_delay_ms(0, None, 0) >= 1);
        // The shift clamp keeps huge attempts finite instead of overflowing.
        assert!(backoff_delay_ms(63, None, 1_000) > 0);
    }

    #[test]
    fn governor_key_fingerprints_tokens_without_storing_them() {
        let with_token =
            GovernorKey::new("github", "api.github.com", Some("ghp_secret_token"), "core");
        let same_token =
            GovernorKey::new("github", "api.github.com", Some("ghp_secret_token"), "core");
        let other_token =
            GovernorKey::new("github", "api.github.com", Some("ghp_other_token"), "core");
        let anonymous = GovernorKey::new("github", "api.github.com", None, "core");
        let search =
            GovernorKey::new("github", "api.github.com", Some("ghp_secret_token"), "search");
        assert_eq!(with_token, same_token);
        assert_ne!(with_token, other_token);
        assert_ne!(with_token, search, "provider quota lanes are independent");
        assert_eq!(anonymous.token_fingerprint, "anonymous");
        assert!(!with_token.token_fingerprint.contains("secret"), "never the token itself");
        assert_eq!(with_token.token_fingerprint.len(), 16, "8 digest bytes as hex");
        // Blank tokens govern as anonymous, not as a distinct pool.
        assert_eq!(
            GovernorKey::new("github", "api.github.com", Some("  "), "core").token_fingerprint,
            "anonymous"
        );
    }

    #[test]
    fn registry_shares_one_governor_per_key() {
        let registry = GovernorRegistry::default();
        let key = GovernorKey::new("github", "api.github.com", Some("tok"), "core");
        let config = GovernorConfig::default();
        let first = registry.governor(key.clone(), &config);
        let second = registry.governor(key, &config);
        assert!(Arc::ptr_eq(&first, &second), "same key → same governor");
        let other =
            registry.governor(GovernorKey::new("github", "api.github.com", None, "core"), &config);
        assert!(!Arc::ptr_eq(&first, &other), "different token → separate accounting");
    }
}
