//! Dream Mode worklist on `IndexDatabase` (#122): a thin pass-through to `crate::dream`. Computes
//! the deterministic memory-maintenance worklist (coverage gaps + stale references), syncs it into
//! `dream_findings`, and returns the open worklist. Writes ONLY to `dream_findings` — never mutates
//! a `repo_memories` row.

use super::*;
use crate::dream::{CompactPass, DreamOptions, DreamReport, VerdictPass};

impl IndexDatabase {
    pub fn dream_run(&self, opts: DreamOptions) -> anyhow::Result<DreamReport> {
        crate::dream::dream_run(self.storage.connection(), opts)
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
        // #582 review: the model passes rank chunk_fts (evidence-pack probes) MID-RUN, after
        // model side effects — a blanket retry would replay them. PRE-FLIGHT the probe-and-heal
        // instead so the run starts on healthy mirrors; on a clean index the probe is four
        // ranked LIMIT-1 reads. A DEFERRED repair (staged rebuild in flight) must postpone the
        // run: proceeding would pay the model/provisioning side effects only to hit the same
        // corruption mid-pass. The plain worklist (no passes) stays byte-identical and
        // write-free — it never ranks, so it gets no probe and no heal.
        if verdict_pass.is_some() || compact_pass.is_some() {
            let preflight = self.heal_fts_if_corrupt()?;
            anyhow::ensure!(
                preflight.deferred.is_empty(),
                "dream postponed: FTS mirrors {:?} are corrupt and their repair is deferred \
                 behind an in-flight staged rebuild; rerun once it completes (or after gc sweeps \
                 an abandoned staging)",
                preflight.deferred
            );
        }
        crate::dream::dream_run_with_passes(
            self.storage.connection(),
            opts,
            verdict_pass,
            compact_pass,
        )
    }

    /// Whether the model passes have pending work (the zero-work guard for ephemeral
    /// `[llm.dream.remote]`): peek the verify/compact churn-skip queues without touching the model,
    /// considering current model-specific failure annotations, so the CLI skips cold-starting a
    /// paid GPU box when the queues are already drained. See [`crate::dream::model_work_pending`].
    pub fn dream_model_work_pending(
        &self,
        opts: DreamOptions,
        budget: usize,
        verify: bool,
        compact: bool,
        model_id: &str,
    ) -> anyhow::Result<bool> {
        // #582 review: the zero-work guard ranks chunk_fts (`dream::verify::text_probe`) —
        // read-only, so heal-and-retry is safe.
        crate::index::retry_once_on_fts_corruption(
            || {
                crate::dream::model_work_pending(
                    self.storage.connection(),
                    opts,
                    budget,
                    verify,
                    compact,
                    model_id,
                )
            },
            || self.heal_corrupt_fts(),
        )
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
