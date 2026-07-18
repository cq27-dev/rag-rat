//! `distill` command (#703): the deterministic, model-free distillation pass. Opens the index
//! read-write (base-scoped, so anchor mining reads the correct symbol scope) and runs the
//! extraction over the current mirror, reporting what it produced.

use rag_rat_base::config::Config;

use crate::cli::{DistillArgs, DistillCommand};
use crate::open_index;
use crate::render::print_output;

pub(crate) fn distill(config: &Config, args: &DistillArgs) -> anyhow::Result<()> {
    match &args.command {
        DistillCommand::Extract => {
            // `extract` is a WRITER (skeleton records + junctions + queue) that also reads the
            // symbol index for anchors, so it must serialize with indexing under the per-repo write
            // lock like every other CLI writer — otherwise a concurrent generation switch can pin
            // the scope views to a stale generation and mine anchors from stale symbols. Held for
            // the whole pass.
            let lock_repo = rag_rat_base::locks::write_lock_repo_id(config);
            let _lock =
                rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
            let db = open_index(config)?;
            // Route through the shared renderer so the global `--json` flag is honored (the report
            // is `Serialize`); TOON otherwise.
            print_output(&db.distill_extract()?)
        },
    }
}
