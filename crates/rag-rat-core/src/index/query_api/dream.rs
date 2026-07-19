//! Dream Mode worklist on `IndexDatabase` (#122): a thin pass-through to `rag_rat_dream`. Computes
//! the deterministic memory-maintenance worklist (coverage gaps + stale references), syncs it into
//! `dream_findings`, and returns the open worklist. Writes ONLY to `dream_findings` — never mutates
//! a `repo_memories` row.

use rag_rat_dream::{CompactPass, DreamOptions, DreamReport, VerdictPass};

use super::*;

impl IndexDatabase {
    pub fn dream_run(&self, opts: DreamOptions) -> anyhow::Result<DreamReport> {
        let conn = self.storage.connection();
        // #767 review: fail closed when the active repo was `rag-rat rm`-removed after this
        // connection resolved its scope (a stale MCP `dream` writer). The findings sync below
        // would otherwise INSERT fresh `dream_findings` rows for the removed `repo_id` after the
        // purge reported success — the table intentionally carries no FK to `repos`.
        let active_repo_id = rag_rat_db::schema::active_repo_id(conn)?;
        crate::index::remove::assert_repo_not_removed(conn, &active_repo_id)?;
        rag_rat_dream::dream_run(conn, opts)
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
        rag_rat_dream::dream_run_with_passes(
            self.storage.connection(),
            opts,
            verdict_pass,
            compact_pass,
        )
    }

    /// Whether the model passes have pending work (the zero-work guard for ephemeral
    /// `[llm.dream.remote]`): peek the verify/compact churn-skip queues without touching the model,
    /// considering current model-specific failure annotations, so the CLI skips cold-starting a
    /// paid GPU box when the queues are already drained. See [`rag_rat_dream::model_work_pending`].
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
                rag_rat_dream::model_work_pending(
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

    /// Render one memory's evidence pack into the exact text the verdict model is shown — the
    /// generator behind the memory-compaction eval's `verify-packs` corpus (#695). See
    /// [`rag_rat_dream::render_evidence_pack`].
    pub fn dream_render_pack(&self, memory_id: &str) -> anyhow::Result<String> {
        rag_rat_dream::render_evidence_pack(self.storage.connection(), memory_id)
    }

    /// Apply a human review verdict (accept / dismiss / reset) to a dream finding by id or prefix —
    /// the `rag-rat dream <id> --accept|--dismiss|--reset` surface. Repo-scoped; only a
    /// non-terminal finding is reviewable. See [`rag_rat_dream::review_dream_finding`].
    pub fn review_dream_finding(
        &self,
        id_or_prefix: &str,
        verdict: rag_rat_dream::ReviewVerdict,
        now_ms: i64,
    ) -> anyhow::Result<rag_rat_dream::ReviewedFinding> {
        rag_rat_dream::review_dream_finding(
            self.storage.connection(),
            id_or_prefix,
            verdict,
            now_ms,
        )
    }
}
