use rag_rat_base::config::PapertrailConfig;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::{MirrorContinuation, ResolvedTracker, load_mirror_continuation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleDecision {
    Skip,
    Probe,
    Incremental,
    Full,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BindingScheduleState {
    pub last_attempt_ms: Option<i64>,
    pub last_successful_probe_ms: Option<i64>,
    pub last_successful_mirror_ms: Option<i64>,
    pub last_full_walk_ms: Option<i64>,
    pub retry_not_before_ms: Option<i64>,
    pub continuation: MirrorContinuation,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PapertrailErrorClass {
    Authentication,
    Network,
    RateLimited,
    Provider,
    Storage,
    Unknown,
}

impl PapertrailErrorClass {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::Network => "network",
            Self::RateLimited => "rate_limited",
            Self::Provider => "provider",
            Self::Storage => "storage",
            Self::Unknown => "unknown",
        }
    }

    fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "authentication" => Some(Self::Authentication),
            "network" => Some(Self::Network),
            "rate_limited" => Some(Self::RateLimited),
            "provider" => Some(Self::Provider),
            "storage" => Some(Self::Storage),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

pub(crate) fn decide_schedule(
    now_ms: i64,
    config: &PapertrailConfig,
    state: BindingScheduleState,
    change_detected: bool,
) -> ScheduleDecision {
    let elapsed = |then: Option<i64>, interval_secs: u64| {
        then.is_none_or(|then| now_ms.saturating_sub(then) >= millis(interval_secs))
    };
    if state.retry_not_before_ms.is_some_and(|resume_at_ms| now_ms < resume_at_ms) {
        return ScheduleDecision::Skip;
    }
    if !elapsed(state.last_attempt_ms, config.sync_min_interval_secs) {
        return ScheduleDecision::Skip;
    }
    match state.continuation {
        MirrorContinuation::Full => return ScheduleDecision::Full,
        MirrorContinuation::Incremental => return ScheduleDecision::Incremental,
        MirrorContinuation::None => {},
    }
    if elapsed(state.last_full_walk_ms, config.full_sync_interval_secs) {
        return ScheduleDecision::Full;
    }
    if change_detected {
        return ScheduleDecision::Incremental;
    }
    if elapsed(state.last_successful_probe_ms, config.probe_interval_secs) {
        return ScheduleDecision::Probe;
    }
    ScheduleDecision::Skip
}

fn millis(seconds: u64) -> i64 {
    i64::try_from(seconds.saturating_mul(1_000)).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuccessfulOperation {
    Probe,
    IncrementalMirror,
    FullMirror,
}

/// A coalescible automatic-sync trigger. The variant order IS the strength order (`Ord`): when
/// triggers coalesce — in the in-process worker queue or through the cross-process pending
/// marker — the merged request is the maximum, so a daily/full trigger is never weakened by a
/// later incremental or evaluation tick.
///
/// - `Evaluate` — a timer tick: let [`decide_schedule`] pick per binding (probe when due, the full
///   backstop when overdue, resuming interrupted work first).
/// - `Incremental` — an external change signal (a git hook fired): bindings due for any work run a
///   delta walk without waiting for probe cadence.
/// - `Full` — a healing walk was explicitly requested; bindings not gated by pauses or the minimum
///   attempt interval run a full re-walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AutosyncRequest {
    Evaluate,
    Incremental,
    Full,
}

impl AutosyncRequest {
    /// The exact token persisted in the cross-process pending marker file.
    pub fn as_marker_str(self) -> &'static str {
        match self {
            Self::Evaluate => "evaluate",
            Self::Incremental => "incremental",
            Self::Full => "full",
        }
    }

    /// Parse a pending-marker token. Unknown content (a torn write, a future token) degrades to
    /// `Evaluate`: the scheduling policy re-decides everything an evaluation can, and interrupted
    /// work resumes from the persisted cursor regardless.
    pub fn from_marker_str(value: &str) -> Self {
        match value.trim() {
            "incremental" => Self::Incremental,
            "full" => Self::Full,
            _ => Self::Evaluate,
        }
    }
}

pub(crate) fn record_attempt(
    conn: &Connection,
    binding: &ResolvedTracker,
    at_ms: i64,
) -> anyhow::Result<()> {
    ensure_health_row(conn, binding)?;
    update_health(conn, binding, "last_attempt_ms=?4", at_ms)
}

pub(crate) fn record_success(
    conn: &Connection,
    binding: &ResolvedTracker,
    operation: SuccessfulOperation,
    at_ms: i64,
) -> anyhow::Result<()> {
    ensure_health_row(conn, binding)?;
    let assignment = match operation {
        SuccessfulOperation::Probe => "last_successful_probe_ms=?4",
        SuccessfulOperation::IncrementalMirror =>
            "last_successful_probe_ms=?4, last_successful_mirror_ms=?4",
        SuccessfulOperation::FullMirror =>
            "last_successful_probe_ms=?4, last_successful_mirror_ms=?4, last_full_sync_ms=?4",
    };
    update_health(
        conn,
        binding,
        &format!("{assignment}, retry_not_before_ms=NULL, error_class=NULL, error_detail=NULL"),
        at_ms,
    )
}

pub(crate) fn record_pause(
    conn: &Connection,
    binding: &ResolvedTracker,
    resume_at_ms: i64,
) -> anyhow::Result<()> {
    ensure_health_row(conn, binding)?;
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    conn.execute(
        "UPDATE papertrail_sync_cursor
         SET retry_not_before_ms=?4, error_class='rate_limited', error_detail=NULL
         WHERE repo_id=?1 AND tracker=?2 AND project=?3",
        params![repo_id, binding.provider.as_db_str(), binding.project, resume_at_ms],
    )?;
    Ok(())
}

pub(crate) fn record_failure(
    conn: &Connection,
    binding: &ResolvedTracker,
    class: PapertrailErrorClass,
    detail: Option<&str>,
) -> anyhow::Result<()> {
    ensure_health_row(conn, binding)?;
    let detail = detail.map(sanitize_error_detail);
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    conn.execute(
        "UPDATE papertrail_sync_cursor
         SET retry_not_before_ms=NULL, error_class=?4, error_detail=?5
         WHERE repo_id=?1 AND tracker=?2 AND project=?3",
        params![repo_id, binding.provider.as_db_str(), binding.project, class.as_db_str(), detail,],
    )?;
    Ok(())
}

fn ensure_health_row(conn: &Connection, binding: &ResolvedTracker) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO papertrail_sync_cursor(tracker, project, repo_id)
         VALUES (?1, ?2, ?3)",
        params![binding.provider.as_db_str(), binding.project, repo_id],
    )?;
    Ok(())
}

fn update_health(
    conn: &Connection,
    binding: &ResolvedTracker,
    assignment: &str,
    at_ms: i64,
) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let sql = format!(
        "UPDATE papertrail_sync_cursor SET {assignment}
         WHERE repo_id=?1 AND tracker=?2 AND project=?3"
    );
    conn.execute(&sql, params![repo_id, binding.provider.as_db_str(), binding.project, at_ms])?;
    Ok(())
}

fn sanitize_error_detail(detail: &str) -> String {
    detail
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(512)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) type PersistedHealth =
    (BindingScheduleState, Option<PapertrailErrorClass>, Option<String>, String);

pub(crate) fn load_persisted_health(
    conn: &Connection,
    repo_id: &str,
    binding: &ResolvedTracker,
) -> anyhow::Result<PersistedHealth> {
    let persisted = conn
        .query_row(
            "SELECT last_attempt_ms, last_successful_probe_ms, last_successful_mirror_ms,
                    last_full_sync_ms, retry_not_before_ms,
                    error_class, error_detail, filter_fingerprint
             FROM papertrail_sync_cursor
             WHERE repo_id=?1 AND tracker=?2 AND project=?3",
            params![repo_id, binding.provider.as_db_str(), binding.project],
            |row| {
                let error: Option<String> = row.get(5)?;
                Ok((
                    BindingScheduleState {
                        last_attempt_ms: row.get(0)?,
                        last_successful_probe_ms: row.get(1)?,
                        last_successful_mirror_ms: row.get(2)?,
                        last_full_walk_ms: row.get(3)?,
                        retry_not_before_ms: row.get(4)?,
                        continuation: MirrorContinuation::None,
                    },
                    error.as_deref().and_then(PapertrailErrorClass::from_db_str),
                    row.get(6)?,
                    row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                ))
            },
        )
        .optional()?;
    let Some((mut state, error, detail, fingerprint)) = persisted else {
        return Ok(PersistedHealth::default());
    };
    state.continuation = load_mirror_continuation(conn, binding)?;
    Ok((state, error, detail, fingerprint))
}

#[cfg(test)]
mod tests {
    use rag_rat_base::config::Tracker;
    use rag_rat_db::schema;

    use super::*;
    use crate::TrackerAuthentication;

    fn config() -> PapertrailConfig {
        PapertrailConfig::default()
    }

    fn binding() -> ResolvedTracker {
        ResolvedTracker {
            provider: Tracker::Github,
            project: "o/r".to_string(),
            base_url: None,
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        }
    }

    #[test]
    fn full_backstop_has_priority() {
        let state = BindingScheduleState { last_full_walk_ms: Some(0), ..Default::default() };
        assert_eq!(decide_schedule(86_400_000, &config(), state, false), ScheduleDecision::Full);
    }

    #[test]
    fn minimum_interval_suppresses_triggered_incremental_work() {
        let state = BindingScheduleState {
            last_attempt_ms: Some(10_000),
            last_full_walk_ms: Some(10_000),
            ..Default::default()
        };
        assert_eq!(decide_schedule(20_000, &config(), state, true), ScheduleDecision::Skip);
    }

    #[test]
    fn minimum_interval_suppresses_failed_initial_full_retry() {
        let state = BindingScheduleState { last_attempt_ms: Some(10_000), ..Default::default() };
        assert_eq!(decide_schedule(20_000, &config(), state, false), ScheduleDecision::Skip);
        assert_eq!(decide_schedule(910_000, &config(), state, false), ScheduleDecision::Full);
    }

    #[test]
    fn due_probe_and_detected_change_are_explicit() {
        let state = BindingScheduleState {
            last_attempt_ms: Some(0),
            last_successful_probe_ms: Some(0),
            last_full_walk_ms: Some(1),
            ..Default::default()
        };
        assert_eq!(decide_schedule(900_000, &config(), state, false), ScheduleDecision::Probe);
        assert_eq!(decide_schedule(900_000, &config(), state, true), ScheduleDecision::Incremental);
    }

    #[test]
    fn persisted_provider_pause_gates_all_work_until_resume_time() {
        let state = BindingScheduleState {
            last_attempt_ms: Some(0),
            retry_not_before_ms: Some(10_000),
            ..Default::default()
        };
        assert_eq!(decide_schedule(9_999, &config(), state, true), ScheduleDecision::Skip);
        assert_eq!(decide_schedule(900_000, &config(), state, true), ScheduleDecision::Full);
    }

    #[test]
    fn mirror_success_advances_probe_freshness_and_clears_pause() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        let binding = binding();
        record_pause(&conn, &binding, 20_000).unwrap();
        record_success(&conn, &binding, SuccessfulOperation::IncrementalMirror, 10_000).unwrap();
        let repo_id = schema::active_repo_id(&conn).unwrap();
        let (state, error, _, _) = load_persisted_health(&conn, &repo_id, &binding).unwrap();
        assert_eq!(state.last_successful_probe_ms, Some(10_000));
        assert_eq!(state.last_successful_mirror_ms, Some(10_000));
        assert_eq!(state.retry_not_before_ms, None);
        assert_eq!(error, None);
    }

    #[test]
    fn provider_pause_round_trips_through_binding_health() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        let binding = binding();
        record_pause(&conn, &binding, 20_000).unwrap();
        let repo_id = schema::active_repo_id(&conn).unwrap();
        let (state, error, detail, _) = load_persisted_health(&conn, &repo_id, &binding).unwrap();
        assert_eq!(state.retry_not_before_ms, Some(20_000));
        assert_eq!(error, Some(PapertrailErrorClass::RateLimited));
        assert_eq!(detail, None);
    }

    #[test]
    fn provider_pause_replaces_a_stale_failure() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        let binding = binding();
        record_failure(
            &conn,
            &binding,
            PapertrailErrorClass::Authentication,
            Some("old auth failure"),
        )
        .unwrap();
        record_pause(&conn, &binding, 20_000).unwrap();
        let repo_id = schema::active_repo_id(&conn).unwrap();
        let (_, error, detail, _) = load_persisted_health(&conn, &repo_id, &binding).unwrap();
        assert_eq!(error, Some(PapertrailErrorClass::RateLimited));
        assert_eq!(detail, None);
    }

    #[test]
    fn non_rate_failure_clears_a_stale_provider_pause() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        let binding = binding();
        record_pause(&conn, &binding, 20_000).unwrap();
        record_failure(
            &conn,
            &binding,
            PapertrailErrorClass::Authentication,
            Some("new auth failure"),
        )
        .unwrap();
        let repo_id = schema::active_repo_id(&conn).unwrap();
        let (state, error, detail, _) = load_persisted_health(&conn, &repo_id, &binding).unwrap();
        assert_eq!(state.retry_not_before_ms, None);
        assert_eq!(error, Some(PapertrailErrorClass::Authentication));
        assert_eq!(detail.as_deref(), Some("new auth failure"));
    }

    #[test]
    fn interrupted_full_walk_resumes_after_retry_gates_open() {
        let state = BindingScheduleState {
            last_attempt_ms: Some(0),
            last_full_walk_ms: Some(899_999),
            continuation: MirrorContinuation::Full,
            ..Default::default()
        };
        assert_eq!(decide_schedule(900_000, &config(), state, false), ScheduleDecision::Full);
    }

    #[test]
    fn interrupted_incremental_work_resumes_before_a_fresh_probe() {
        let state = BindingScheduleState {
            last_attempt_ms: Some(0),
            last_successful_probe_ms: Some(899_999),
            last_full_walk_ms: Some(899_999),
            continuation: MirrorContinuation::Incremental,
            ..Default::default()
        };
        assert_eq!(
            decide_schedule(900_000, &config(), state, false),
            ScheduleDecision::Incremental
        );
    }

    #[test]
    fn persisted_cursor_work_and_filter_fingerprint_are_loaded_for_scheduling() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        let binding = binding();
        record_attempt(&conn, &binding, 1).unwrap();
        conn.execute(
            "UPDATE papertrail_sync_cursor
             SET backfill_done=1, item_delta_in_progress=1, filter_fingerprint='old'",
            [],
        )
        .unwrap();
        let repo_id = schema::active_repo_id(&conn).unwrap();
        let (state, _, _, fingerprint) = load_persisted_health(&conn, &repo_id, &binding).unwrap();
        assert_eq!(state.continuation, MirrorContinuation::Incremental);
        assert_eq!(fingerprint, "old");
    }

    #[test]
    fn error_detail_is_bounded_and_single_line() {
        let detail = format!("token\n{}", "x".repeat(600));
        let sanitized = sanitize_error_detail(&detail);
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.chars().count() <= 512);
    }
}
