use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::ResolvedTracker;
use crate::config::{PapertrailConfig, Tracker};

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
    if !elapsed(state.last_attempt_ms, config.sync_min_interval_secs) {
        return ScheduleDecision::Skip;
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
    #[allow(dead_code, reason = "the watcher probe worker lands in the dependent #592 slice")]
    Probe,
    IncrementalMirror,
    FullMirror,
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
        SuccessfulOperation::IncrementalMirror => "last_successful_mirror_ms=?4",
        SuccessfulOperation::FullMirror => "last_successful_mirror_ms=?4, last_full_sync_ms=?4",
    };
    update_health(
        conn,
        binding,
        &format!("{assignment}, error_class=NULL, error_detail=NULL"),
        at_ms,
    )
}

pub(crate) fn record_failure(
    conn: &Connection,
    binding: &ResolvedTracker,
    class: PapertrailErrorClass,
    detail: Option<&str>,
) -> anyhow::Result<()> {
    ensure_health_row(conn, binding)?;
    let detail = detail.map(sanitize_error_detail);
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    conn.execute(
        "UPDATE papertrail_sync_cursor SET error_class=?4, error_detail=?5
         WHERE repo_id=?1 AND tracker=?2 AND project=?3",
        params![repo_id, binding.provider.as_db_str(), binding.project, class.as_db_str(), detail,],
    )?;
    Ok(())
}

fn ensure_health_row(conn: &Connection, binding: &ResolvedTracker) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
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
    let repo_id = crate::index::schema::active_repo_id(conn)?;
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
    (BindingScheduleState, Option<PapertrailErrorClass>, Option<String>);

pub(crate) fn load_persisted_health(
    conn: &Connection,
    repo_id: &str,
    tracker: Tracker,
    project: &str,
) -> anyhow::Result<PersistedHealth> {
    Ok(conn
        .query_row(
            "SELECT last_attempt_ms, last_successful_probe_ms, last_successful_mirror_ms,
                    last_full_sync_ms, error_class, error_detail
             FROM papertrail_sync_cursor
             WHERE repo_id=?1 AND tracker=?2 AND project=?3",
            params![repo_id, tracker.as_db_str(), project],
            |row| {
                let error: Option<String> = row.get(4)?;
                Ok((
                    BindingScheduleState {
                        last_attempt_ms: row.get(0)?,
                        last_successful_probe_ms: row.get(1)?,
                        last_successful_mirror_ms: row.get(2)?,
                        last_full_walk_ms: row.get(3)?,
                    },
                    error.as_deref().and_then(PapertrailErrorClass::from_db_str),
                    row.get(5)?,
                ))
            },
        )
        .optional()?
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> PapertrailConfig {
        PapertrailConfig::default()
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
    fn error_detail_is_bounded_and_single_line() {
        let detail = format!("token\n{}", "x".repeat(600));
        let sanitized = sanitize_error_detail(&detail);
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.chars().count() <= 512);
    }
}
