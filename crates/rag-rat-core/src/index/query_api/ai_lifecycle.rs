//! On-device embedding-model lifecycle on `IndexDatabase`: status, model install/list, and the
//! reconcile (re-embed) entry points. Thin pass-throughs to `crate::index::ai`; kept together so
//! the model-management surface reads as one unit.

use super::*;

impl IndexDatabase {
    pub fn llm_status(&self) -> anyhow::Result<LlmStatus> {
        ai::status(self.storage.connection())
    }

    pub fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        ai::models(self.storage.connection())
    }

    /// Install/activate an embedding model. `remote` carries the `[llm.embedding.remote]`
    /// connection params and is REQUIRED for the Ollama backend (the install is a reachability +
    /// dim probe against that endpoint); every other backend ignores it. Callers pass
    /// `config.llm.embedding.remote.as_ref()` — `None` for the local backends.
    pub fn install_model(
        &self,
        model_id: &str,
        remote: Option<&rag_rat_base::config::RemoteEmbeddingConfig>,
    ) -> anyhow::Result<ModelInfo> {
        ai::install_model(self.storage.connection(), model_id, remote)
    }

    pub fn reconcile(
        &self,
        limit: Option<u32>,
        batch_size: Option<u32>,
    ) -> anyhow::Result<ReconcileReport> {
        ai::reconcile(self.storage.connection(), limit, batch_size)
    }

    /// Plan the embedding reconcile at the DEFAULT char cap. The CLI uses the cap-aware
    /// [`reconcile_plan_with_cap`](Self::reconcile_plan_with_cap) so `--plan` classifies against
    /// the same cap the run will; this default-cap form stays for callers without a configured
    /// cap.
    pub fn reconcile_plan(&self) -> anyhow::Result<ReconcilePlan> {
        self.reconcile_plan_with_cap(ai::DEFAULT_MAX_EMBEDDING_CHARS)
    }

    /// `max_embedding_chars` is the caller's configured/overridden cap, so the plan classifies
    /// against the SAME cap the reconcile it previews will use.
    pub fn reconcile_plan_with_cap(
        &self,
        max_embedding_chars: usize,
    ) -> anyhow::Result<ReconcilePlan> {
        ai::reconcile_plan(self.storage.connection(), max_embedding_chars)
    }

    pub fn reconcile_with_progress(
        &self,
        limit: Option<u32>,
        batch_size: Option<u32>,
        force: bool,
        progress: impl FnMut(ai::ReconcileProgress),
    ) -> anyhow::Result<ReconcileReport> {
        ai::reconcile_with_progress(self.storage.connection(), limit, batch_size, force, progress)
    }

    pub fn reconcile_with_options_progress(
        &self,
        options: ai::ReconcileOptions,
        progress: impl FnMut(ai::ReconcileProgress),
    ) -> anyhow::Result<ReconcileReport> {
        let report =
            ai::reconcile_with_options_progress(self.storage.connection(), options, progress)?;
        self.heal_memory_oplog_ghosts()?;
        Ok(report)
    }

    /// Idle-repo op-log ghost backstop (#583, follow-up to #541). The per-node op-log reconcile
    /// (#541) heals a "ghost" memory/edge — a row present in `repo_memories`/`repo_node_edges` but
    /// absent from the signed projection, left by a pre-#532 binary or a raw writer such as the
    /// `dream` passes — on the next MEMORY mutation. A repo with no subsequent memory mutation
    /// would carry the ghost indefinitely, so a reconcile pass runs the same idempotent
    /// reconcile: both `rag-rat reconcile` and the watcher's incremental pass route through
    /// [`Self::reconcile_with_options_progress`], so this one seam covers every idle-repo trigger.
    ///
    /// Cheap and safe on the hot path: [`backfill_memory_oplog`] probes with two indexed anti-joins
    /// that return empty in steady state and take NO write lock when nothing is missing, and it is
    /// a no-op under an absent/unstable scope. Runs after the embedding reconcile has
    /// committed, so its authored write (a durable `IMMEDIATE` txn only when a ghost exists)
    /// never nests inside it.
    pub(crate) fn heal_memory_oplog_ghosts(&self) -> anyhow::Result<()> {
        crate::memory_write::backfill_memory_oplog(
            self.storage.connection(),
            rag_rat_base::time::now_ms(),
        )
    }

    pub fn current_embedding_count(&self, model_id: &str) -> anyhow::Result<u64> {
        ai::current_embedding_count(self.storage.connection(), model_id)
    }

    /// Chunks in the ACTIVE scope still awaiting embedding — the watcher uses this to retry an
    /// overlay whose inline reconcile was cut short by the shared time budget (returned `Partial`),
    /// even on a later pass where the overlay rows themselves did not change (#219 review).
    pub fn pending_embedding_jobs(&self) -> anyhow::Result<u64> {
        ai::pending_embedding_jobs(self.storage.connection())
    }

    /// [`Self::pending_embedding_jobs`] with the caller's candidate sizing — SQL-only, no embedder
    /// acquisition (no probe request), so the watcher's `All`-sweep overlay backlog check is free
    /// of network work on an idle pass (#577). Whether the backlog can actually drain is decided
    /// inside the reconcile itself.
    pub(crate) fn pending_embedding_jobs_with_options(
        &self,
        options: &ai::ReconcileOptions,
    ) -> anyhow::Result<u64> {
        ai::pending_embedding_jobs_with_options(self.storage.connection(), options)
    }

    pub(crate) fn pending_embedding_jobs_with_available_incremental_embedder(
        &self,
        options: &ai::ReconcileOptions,
    ) -> anyhow::Result<u64> {
        ai::pending_embedding_jobs_with_available_incremental_embedder(
            self.storage.connection(),
            options,
        )
    }

    /// One-time upgrade: re-encode any `chunk_embeddings` row still stored in the legacy f32 format
    /// to the compact int8 format (#312), gated by a meta key so later maintenance passes skip the
    /// table scan. A format-only conversion (decode f32 → encode int8), no model inference. Returns
    /// the number of rows converted this call (`0` once the gate is set).
    ///
    /// `deadline` bounds the conversion so it honors the maintenance time budget: a deadline stop
    /// leaves the gate unset and persists a keyset cursor, so the next pass resumes without a
    /// rescan (the gate is set only on full completion). `None` runs to completion. See
    /// [`ai::reencode_legacy_vectors_if_needed`].
    pub fn reencode_legacy_vectors_if_needed(
        &self,
        deadline: Option<std::time::Instant>,
    ) -> anyhow::Result<usize> {
        ai::reencode_legacy_vectors_if_needed(self.storage.connection(), deadline)
    }

    /// FORCE the legacy-f32 → int8 re-encode (#312), ignoring the run-once meta gate at the start —
    /// for users who want it now on a huge index without waiting for the next maintenance pass.
    /// Sets the gate on success so a later maintenance pass doesn't redo the table scan.
    /// Idempotent: converts only rows still in f32.
    ///
    /// `deadline` bounds the work so `reconcile --reencode-vectors --max-seconds N` can cap it; a
    /// deadline stop leaves the gate unset and persists a keyset cursor, so a follow-up run resumes
    /// from there. `None` runs to completion. Returns the number of rows converted.
    pub fn reencode_legacy_vectors_now(
        &self,
        deadline: Option<std::time::Instant>,
    ) -> anyhow::Result<usize> {
        ai::reencode_legacy_vectors_now_within(self.storage.connection(), deadline)
    }
}
