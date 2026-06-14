//! The pure throttle gate for the background auto-fresh oracle (Phase 5, opt-in / default OFF).
//!
//! SCIP production takes minutes; index edits arrive in seconds. Edge-collapsing a reindex burst is
//! NOT enough — the server would still kick off a minutes-long pass on the first edit of an active
//! session. So the decision is a **two-gate throttle**: a long *quiet-period* debounce (run only
//! after the index has been still for a while) AND a *minimum-interval* floor (run at most once
//! every few hours). Both gates are evaluated against plain timestamps sourced by the caller — the
//! index's last change time and the last successful oracle run's start time — so the decision
//! itself is pure and fully unit-testable without a clock, a database, or rust-analyzer.
//!
//! Mirrors the shape of [`crate::version_check::needs_refresh`]: inputs are values, the decision is
//! a closed enum, the clock lives outside.

/// Plain-value inputs to [`auto_run_decision`] — sourced by the caller (config, index meta, the
/// latest `oracle_runs` row) so the gate has no I/O of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoRunInputs {
    /// `[oracle] auto_run`. When false the gate short-circuits to [`AutoRunDecision::Disabled`].
    pub enabled: bool,
    /// Now, Unix-epoch ms (injected).
    pub now_ms: i64,
    /// When the active checkout's index last changed, Unix-epoch ms (`indexed_at_ms` meta).
    pub last_index_change_ms: i64,
    /// When the most recent successful oracle run for the active checkout STARTED, or `None` when
    /// no run exists yet (`oracle_runs.started_at`).
    pub last_run_ms: Option<i64>,
    /// `[oracle] auto_run_quiet_period_secs`, in ms.
    pub quiet_period_ms: i64,
    /// `[oracle] auto_run_min_interval_secs`, in ms.
    pub min_interval_ms: i64,
}

/// The throttle verdict. Every non-`Run` arm names the gate that held it back so a caller (or a log
/// line) can explain why a pass didn't fire. Closed enum — exhaustive matching keeps a future gate
/// from being silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoRunDecision {
    /// `auto_run = false` — the feature is opt-in and was not enabled.
    Disabled,
    /// The index has NOT changed since the last successful run — fresh verdicts already cover it.
    NotStale,
    /// The index changed too recently — still inside the quiet-period debounce (active editing).
    NotQuiet,
    /// A run started too recently — inside the minimum-interval floor.
    TooSoon,
    /// All gates clear: the index is stale, has been quiet long enough, and the floor has elapsed.
    Run,
}

/// The two-gate throttle, pure. Order matters: a disabled feature never inspects timestamps; a
/// not-stale index never schedules; the quiet-period debounce precedes the min-interval floor.
///
/// - `enabled == false` → [`AutoRunDecision::Disabled`].
/// - index NOT stale (no change since the last successful run) → [`AutoRunDecision::NotStale`].
/// - last index change within `quiet_period_ms` → [`AutoRunDecision::NotQuiet`] (still editing).
/// - last run within `min_interval_ms` → [`AutoRunDecision::TooSoon`].
/// - else → [`AutoRunDecision::Run`].
///
/// "Stale" means the index changed AFTER the last successful run started (or no run has ever
/// happened). `saturating_sub` keeps a clock that went backwards from underflowing into a spurious
/// `Run`.
pub fn auto_run_decision(p: AutoRunInputs) -> AutoRunDecision {
    if !p.enabled {
        return AutoRunDecision::Disabled;
    }
    // Stale = the index moved since the last run began. No prior run ⇒ always stale.
    let stale = match p.last_run_ms {
        Some(last_run_ms) => p.last_index_change_ms > last_run_ms,
        None => true,
    };
    if !stale {
        return AutoRunDecision::NotStale;
    }
    // Quiet-period debounce: don't kick off a minutes-long pass while the working tree is churning.
    if p.now_ms.saturating_sub(p.last_index_change_ms) < p.quiet_period_ms {
        return AutoRunDecision::NotQuiet;
    }
    // Minimum-interval floor: cap how often the pass can run regardless of churn.
    if let Some(last_run_ms) = p.last_run_ms
        && p.now_ms.saturating_sub(last_run_ms) < p.min_interval_ms
    {
        return AutoRunDecision::TooSoon;
    }
    AutoRunDecision::Run
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A baseline that clears every gate (stale + quiet + past the floor), so each test perturbs
    /// one dimension to isolate the gate it exercises.
    fn runnable() -> AutoRunInputs {
        AutoRunInputs {
            enabled: true,
            now_ms: 100_000,
            last_index_change_ms: 50_000, // changed 50s ago
            last_run_ms: Some(10_000),    // ran 90s ago, before the change
            quiet_period_ms: 30_000,      // 30s quiet window
            min_interval_ms: 60_000,      // 60s floor
        }
    }

    #[test]
    fn disabled_short_circuits() {
        let p = AutoRunInputs { enabled: false, ..runnable() };
        assert_eq!(auto_run_decision(p), AutoRunDecision::Disabled);
    }

    #[test]
    fn not_stale_when_run_is_newer_than_last_change() {
        // The last run started AFTER the last index change → verdicts already cover the index.
        let p = AutoRunInputs { last_run_ms: Some(60_000), ..runnable() };
        assert_eq!(auto_run_decision(p), AutoRunDecision::NotStale);
    }

    #[test]
    fn stale_but_within_quiet_period_is_not_quiet() {
        // Index changed 5s ago (now 100_000, change 95_000) — inside the 30s quiet window.
        let p = AutoRunInputs { last_index_change_ms: 95_000, ..runnable() };
        assert_eq!(auto_run_decision(p), AutoRunDecision::NotQuiet);
    }

    #[test]
    fn stale_and_quiet_but_within_min_interval_is_too_soon() {
        // Index changed 35s ago (65_000) — past the 30s quiet window, so quiet. The last run was
        // 50s ago (50_000) — before the change (so still stale) but inside the 60s floor → TooSoon.
        let p =
            AutoRunInputs { last_index_change_ms: 65_000, last_run_ms: Some(50_000), ..runnable() };
        assert_eq!(auto_run_decision(p), AutoRunDecision::TooSoon);
    }

    #[test]
    fn stale_quiet_and_past_min_interval_runs() {
        assert_eq!(auto_run_decision(runnable()), AutoRunDecision::Run);
    }

    #[test]
    fn no_prior_run_stale_and_quiet_runs() {
        // First-ever run: stale by definition; the min-interval floor doesn't apply without a prior
        // run.
        let p = AutoRunInputs { last_run_ms: None, ..runnable() };
        assert_eq!(auto_run_decision(p), AutoRunDecision::Run);
    }

    #[test]
    fn no_prior_run_but_not_quiet_is_debounced() {
        let p = AutoRunInputs { last_run_ms: None, last_index_change_ms: 95_000, ..runnable() };
        assert_eq!(auto_run_decision(p), AutoRunDecision::NotQuiet);
    }
}
