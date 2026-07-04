//! Dream Mode worklist on `IndexDatabase` (#122): a thin pass-through to `crate::dream`. Computes
//! the deterministic memory-maintenance worklist (coverage gaps + stale references), syncs it into
//! `dream_findings`, and returns the open worklist. Writes ONLY to `dream_findings` — never mutates
//! a `repo_memories` row.

use super::*;
use crate::dream::{DreamOptions, DreamReport, VerdictPass};

impl IndexDatabase {
    pub fn dream_run(&self, opts: DreamOptions) -> anyhow::Result<DreamReport> {
        Ok(crate::dream::dream_run(self.storage.connection(), opts)?)
    }

    /// [`Self::dream_run`] plus the phase-B model verdict pass — the CLI passes `Some(pass)` only
    /// when `[dream.model] enabled = true` and `--verify` is set; `None` is byte-identical to
    /// [`Self::dream_run`].
    pub fn dream_run_with_verdict(
        &self,
        opts: DreamOptions,
        verdict_pass: Option<VerdictPass<'_>>,
    ) -> anyhow::Result<DreamReport> {
        crate::dream::dream_run_with_verdict(self.storage.connection(), opts, verdict_pass)
    }
}
