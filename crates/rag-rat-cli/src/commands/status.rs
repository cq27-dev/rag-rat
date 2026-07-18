//! `rag-rat status`: a read-only, cross-repo inventory of the consolidated global store — a roster
//! of every registered repo with its index freshness, worktree overlays, memory / papertrail / and
//! content counts, plus the whole-file health rollup. Complements `doctor` (one active repo, deep)
//! with a machine-wide overview. A thin shim: the aggregation lives in the read layer
//! (`IndexDatabase::global_status`); this just renders it in the process-wide output format (TOON
//! by default, JSON under the global `--json`).

use std::path::Path;

use rag_rat_core::IndexDatabase;

use crate::render::print_output;

pub(crate) fn status(database: &Path) -> anyhow::Result<()> {
    print_output(&IndexDatabase::global_status(database)?)
}
