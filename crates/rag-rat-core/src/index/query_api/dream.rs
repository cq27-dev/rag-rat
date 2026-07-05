//! Dream Mode worklist on `IndexDatabase` (#122): a thin pass-through to `crate::dream`. Computes
//! the deterministic memory-maintenance worklist (coverage gaps + stale references), syncs it into
//! `dream_findings`, and returns the open worklist. Writes ONLY to `dream_findings` — never mutates
//! a `repo_memories` row.

use super::*;
use crate::dream::{CompactPass, DreamOptions, DreamReport, VerdictPass};

impl IndexDatabase {
    pub fn dream_run(&self, opts: DreamOptions) -> anyhow::Result<DreamReport> {
        Ok(crate::dream::dream_run(self.storage.connection(), opts)?)
    }

    /// [`Self::dream_run`] plus the phase-B model verdict pass and the phase-C model compaction
    /// pass — the CLI supplies each `Some(pass)` only when `[llm.dream] enabled = true` and the
    /// matching flag (`--verify` / `--compact`) is set; `None`/`None` is byte-identical to
    /// [`Self::dream_run`].
    pub fn dream_run_with_passes(
        &self,
        opts: DreamOptions,
        verdict_pass: Option<VerdictPass<'_>>,
        compact_pass: Option<CompactPass<'_>>,
    ) -> anyhow::Result<DreamReport> {
        crate::dream::dream_run_with_passes(
            self.storage.connection(),
            opts,
            verdict_pass,
            compact_pass,
        )
    }

    /// Whether the model passes have pending work (the zero-work guard for ephemeral
    /// `[llm.dream.remote]`): peek the verify/compact churn-skip queues without touching the model,
    /// so the CLI skips cold-starting a paid GPU box when the queues are already drained. See
    /// [`crate::dream::model_work_pending`].
    pub fn dream_model_work_pending(
        &self,
        opts: DreamOptions,
        budget: usize,
        verify: bool,
        compact: bool,
    ) -> anyhow::Result<bool> {
        crate::dream::model_work_pending(self.storage.connection(), opts, budget, verify, compact)
    }

    /// Apply a human review verdict (accept / dismiss / reset) to a dream finding by id or prefix —
    /// the `rag-rat dream <id> --accept|--dismiss|--reset` surface. Repo-scoped; only a
    /// non-terminal finding is reviewable. See [`crate::dream::review_dream_finding`].
    pub fn review_dream_finding(
        &self,
        id_or_prefix: &str,
        verdict: crate::dream::ReviewVerdict,
        now_ms: i64,
    ) -> anyhow::Result<crate::dream::ReviewedFinding> {
        crate::dream::review_dream_finding(self.storage.connection(), id_or_prefix, verdict, now_ms)
    }
}
