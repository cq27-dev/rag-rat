//! On-device embedding-model lifecycle on `IndexDatabase`: status, model install/list, and the
//! reconcile (re-embed) entry points. Thin pass-throughs to `crate::index::ai`; kept together so
//! the model-management surface reads as one unit.

use super::*;

impl IndexDatabase {
    pub fn local_ai_status(&self) -> anyhow::Result<LocalAiStatus> {
        ai::status(self.storage.connection())
    }

    pub fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        ai::models(self.storage.connection())
    }

    pub fn install_model(&self, model_id: &str) -> anyhow::Result<ModelInfo> {
        ai::install_model(self.storage.connection(), model_id)
    }

    pub fn reconcile(
        &self,
        limit: Option<u32>,
        batch_size: Option<u32>,
    ) -> anyhow::Result<ReconcileReport> {
        ai::reconcile(self.storage.connection(), limit, batch_size)
    }

    pub fn reconcile_plan(&self) -> anyhow::Result<ReconcilePlan> {
        ai::reconcile_plan(self.storage.connection())
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
        ai::reconcile_with_options_progress(self.storage.connection(), options, progress)
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
