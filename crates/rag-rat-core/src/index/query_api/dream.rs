//! Dream Mode worklist on `IndexDatabase` (#122): a thin pass-through to `crate::dream`. Computes
//! the deterministic memory-maintenance worklist (coverage gaps + stale references), syncs it into
//! `dream_findings`, and returns the open worklist. Writes ONLY to `dream_findings` — never mutates
//! a `repo_memories` row.

use super::*;
use crate::dream::{DreamOptions, DreamReport};

impl IndexDatabase {
    pub fn dream_run(&self, opts: DreamOptions) -> anyhow::Result<DreamReport> {
        Ok(crate::dream::dream_run(self.storage.connection(), opts)?)
    }
}
