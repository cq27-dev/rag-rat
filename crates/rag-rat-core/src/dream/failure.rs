//! Persisted model-pass failures for dream verification / compaction.
//!
//! A missing `memory_reality` / `memory_summaries` row used to mean both "never tried" and "the
//! model result was rejected", so deterministic guard failures were retried every run. This table
//! records the latter explicitly, with stable enum tokens and the same freshness stamps the
//! producer queues already use.

use rusqlite::{Connection, OptionalExtension};

/// Which model pass attempted a memory. Persisted in `memory_model_failures.pass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DreamModelPass {
    Verify,
    Compact,
}

impl DreamModelPass {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 2] = [Self::Verify, Self::Compact];

    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::Verify => "verify",
            Self::Compact => "compact",
        }
    }

    #[cfg(test)]
    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "verify" => Some(Self::Verify),
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }
}

/// Why a model attempt failed. Persisted in `memory_model_failures.reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DreamFailureReason {
    /// Transport/server failure. Audited, but not used to suppress future model calls because it is
    /// often transient.
    ModelCallFailed,
    /// The verdict completion did not contain a supported `current` / `diverged` answer.
    MalformedVerdict,
    /// The verdict cited evidence absent from the deterministic pack, twice.
    FabricatedEvidence,
    /// The compaction completion failed the deterministic shape/reference guards, twice.
    SummaryGuardRejected,
}

impl DreamFailureReason {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 4] = [
        Self::ModelCallFailed,
        Self::MalformedVerdict,
        Self::FabricatedEvidence,
        Self::SummaryGuardRejected,
    ];

    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::ModelCallFailed => "model_call_failed",
            Self::MalformedVerdict => "malformed_verdict",
            Self::FabricatedEvidence => "fabricated_evidence",
            Self::SummaryGuardRejected => "summary_guard_rejected",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "model_call_failed" => Some(Self::ModelCallFailed),
            "malformed_verdict" => Some(Self::MalformedVerdict),
            "fabricated_evidence" => Some(Self::FabricatedEvidence),
            "summary_guard_rejected" => Some(Self::SummaryGuardRejected),
            _ => None,
        }
    }

    /// Whether an unchanged input should be considered already annotated for this model.
    pub(crate) fn blocks_model_work(self) -> bool {
        !matches!(self, Self::ModelCallFailed)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DreamModelFailure {
    pub(crate) reason: DreamFailureReason,
    pub(crate) detail: Option<String>,
}

impl DreamModelFailure {
    pub(crate) fn new(reason: DreamFailureReason) -> Self {
        Self { reason, detail: None }
    }

    pub(crate) fn with_detail(reason: DreamFailureReason, detail: impl Into<String>) -> Self {
        Self { reason, detail: Some(detail.into()) }
    }
}

pub(crate) struct FailureStamp<'a> {
    pub(crate) memory_id: &'a str,
    pub(crate) repo_id: &'a str,
    pub(crate) pass: DreamModelPass,
    pub(crate) content_hash: &'a str,
    pub(crate) checked_inputs_hash: Option<&'a str>,
    pub(crate) prompt_version: &'a str,
    pub(crate) model_id: &'a str,
}

pub(crate) struct RecordFailure<'a> {
    pub(crate) stamp: FailureStamp<'a>,
    pub(crate) failure: &'a DreamModelFailure,
    pub(crate) now_ms: i64,
}

/// True when a current failure row exists and its reason is deterministic enough to suppress
/// another model call for the same memory/pass/input/model.
pub(crate) fn blocking_failure_is_current(
    conn: &Connection,
    stamp: &FailureStamp<'_>,
) -> rusqlite::Result<bool> {
    let row: Option<(String, Option<String>, String, String, String)> = conn
        .query_row(
            "SELECT content_hash, checked_inputs_hash, prompt_version, model_id, reason FROM \
             memory_model_failures WHERE repo_id = ?1 AND memory_id = ?2 AND pass = ?3",
            rusqlite::params![stamp.repo_id, stamp.memory_id, stamp.pass.as_db_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((content_hash, inputs_hash, prompt_version, model_id, reason)) = row else {
        return Ok(false);
    };
    let Some(reason) = DreamFailureReason::from_db_str(&reason) else {
        return Ok(false);
    };
    Ok(reason.blocks_model_work()
        && content_hash == stamp.content_hash
        && inputs_hash.as_deref() == stamp.checked_inputs_hash
        && prompt_version == stamp.prompt_version
        && model_id == stamp.model_id)
}

pub(crate) fn record_failure(conn: &Connection, r: RecordFailure<'_>) -> rusqlite::Result<()> {
    let detail = r.failure.detail.as_deref().map(bounded_detail);
    conn.execute(
        "INSERT INTO memory_model_failures(memory_id, repo_id, pass, content_hash, \
         checked_inputs_hash, model_id, prompt_version, reason, detail, failed_at_ms, attempts) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1) ON CONFLICT(repo_id, memory_id, pass) DO \
         UPDATE SET attempts = CASE WHEN memory_model_failures.content_hash = \
         excluded.content_hash AND memory_model_failures.checked_inputs_hash IS \
         excluded.checked_inputs_hash AND memory_model_failures.model_id = excluded.model_id AND \
         memory_model_failures.prompt_version = excluded.prompt_version AND \
         memory_model_failures.reason = excluded.reason THEN memory_model_failures.attempts + 1 \
         ELSE 1 END, content_hash = excluded.content_hash, checked_inputs_hash = \
         excluded.checked_inputs_hash, model_id = excluded.model_id, prompt_version = \
         excluded.prompt_version, reason = excluded.reason, detail = excluded.detail, \
         failed_at_ms = excluded.failed_at_ms",
        rusqlite::params![
            r.stamp.memory_id,
            r.stamp.repo_id,
            r.stamp.pass.as_db_str(),
            r.stamp.content_hash,
            r.stamp.checked_inputs_hash,
            r.stamp.model_id,
            r.stamp.prompt_version,
            r.failure.reason.as_db_str(),
            detail.as_deref(),
            r.now_ms,
        ],
    )?;
    Ok(())
}

pub(crate) fn clear_failure(conn: &Connection, stamp: &FailureStamp<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM memory_model_failures WHERE repo_id = ?1 AND memory_id = ?2 AND pass = ?3",
        rusqlite::params![stamp.repo_id, stamp.memory_id, stamp.pass.as_db_str()],
    )?;
    Ok(())
}

fn bounded_detail(detail: &str) -> String {
    let trimmed = detail.trim();
    let mut out = trimmed.chars().take(500).collect::<String>();
    if trimmed.chars().count() > 500 {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dream::tests::mem_db;

    #[test]
    fn persisted_enums_round_trip() {
        for pass in DreamModelPass::ALL {
            assert_eq!(DreamModelPass::from_db_str(pass.as_db_str()), Some(pass));
        }
        assert_eq!(DreamModelPass::from_db_str("nope"), None);

        for reason in DreamFailureReason::ALL {
            assert_eq!(DreamFailureReason::from_db_str(reason.as_db_str()), Some(reason));
        }
        assert_eq!(DreamFailureReason::from_db_str("nope"), None);
    }

    #[test]
    fn current_deterministic_failures_block_but_model_call_failures_do_not() {
        let c = mem_db();
        let stamp = FailureStamp {
            memory_id: "m1",
            repo_id: "r",
            pass: DreamModelPass::Verify,
            content_hash: "content",
            checked_inputs_hash: Some("inputs"),
            prompt_version: "prompt",
            model_id: "model",
        };
        let deterministic = DreamModelFailure::new(DreamFailureReason::FabricatedEvidence);
        record_failure(&c, RecordFailure { stamp, failure: &deterministic, now_ms: 1 }).unwrap();
        let stamp = FailureStamp {
            memory_id: "m1",
            repo_id: "r",
            pass: DreamModelPass::Verify,
            content_hash: "content",
            checked_inputs_hash: Some("inputs"),
            prompt_version: "prompt",
            model_id: "model",
        };
        assert!(blocking_failure_is_current(&c, &stamp).unwrap());

        let transient = DreamModelFailure::new(DreamFailureReason::ModelCallFailed);
        record_failure(&c, RecordFailure { stamp, failure: &transient, now_ms: 2 }).unwrap();
        let stamp = FailureStamp {
            memory_id: "m1",
            repo_id: "r",
            pass: DreamModelPass::Verify,
            content_hash: "content",
            checked_inputs_hash: Some("inputs"),
            prompt_version: "prompt",
            model_id: "model",
        };
        assert!(!blocking_failure_is_current(&c, &stamp).unwrap());
    }

    #[test]
    fn failure_rows_are_repo_scoped() {
        let c = mem_db();
        let sibling_stamp = FailureStamp {
            memory_id: "m1",
            repo_id: "sibling",
            pass: DreamModelPass::Compact,
            content_hash: "content",
            checked_inputs_hash: None,
            prompt_version: "prompt",
            model_id: "model",
        };
        let failure = DreamModelFailure::new(DreamFailureReason::SummaryGuardRejected);
        record_failure(&c, RecordFailure { stamp: sibling_stamp, failure: &failure, now_ms: 1 })
            .unwrap();

        let active_stamp = FailureStamp {
            memory_id: "m1",
            repo_id: "active",
            pass: DreamModelPass::Compact,
            content_hash: "content",
            checked_inputs_hash: None,
            prompt_version: "prompt",
            model_id: "model",
        };
        assert!(
            !blocking_failure_is_current(&c, &active_stamp).unwrap(),
            "a sibling repo's failure row must not suppress active-repo work"
        );
    }
}
