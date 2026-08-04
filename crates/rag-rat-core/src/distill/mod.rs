//! Distilled-record pipeline substrate. The deterministic extraction half (#703) reads the
//! papertrail mirror and git history, assembles numbered units, mines mechanical fix/anchor data,
//! and enqueues eligible threads. The model boundary (#704) is isolated behind a strict typed
//! output contract and an explicit, observable response ladder; no queue drain runs implicitly.
//!
//! Crate placement: this lives in `rag-rat-core` (not `rag-rat-papertrail`) because anchor mining
//! reads the symbol index, and `rag-rat-query` — where symbol reads live — depends on
//! `rag-rat-papertrail`; a distill module in papertrail would be a dependency cycle. The persisted
//! enums it writes live in `rag-rat-papertrail` (the lowest common crate the Phase-3 readers
//! share).

mod candidates;
mod drain;
mod extract;
// Stable rung tokens are also the durable run-column vocabulary; not every conversion is needed by
// the runtime yet, but the round-trip contract remains tested.
#[allow(dead_code)]
mod output;
mod prompts;
mod run_stats;
mod units;
mod validate;

pub use drain::DistillDrainReport;
pub(crate) use drain::{drain, pending_count};
pub use extract::ExtractReport;
pub(crate) use extract::{enqueue_eligible, extract};

/// Advance the per-repo papertrail Lens lanes a distill write feeds — the aggregate enrichment
/// clock and the papertrail lane.
///
/// This is the explicit replacement for the `papertrail_distill` revision triggers V108 dropped:
/// the table syncs on `distill/1`, and a trigger firing on a whole-row-LWW apply is a device-local
/// side effect the sync apply must not have. So the extract/drain writers advance the lanes here
/// and the sync apply advances them at its own site. Call it on the SAME connection as the write,
/// once per pass (matching the old trigger's per-write bump collapses to per-pass — the lane is a
/// monotonic counter). Gated on repo registration: an ungated `bump_lens_revisions` throws on the
/// `repo_meta`→`repos` foreign key, and it avoids phantom `'__unassigned__'` rows.
pub(crate) fn bump_papertrail_lens_lanes(
    conn: &rusqlite::Connection,
    repo_id: &str,
) -> rusqlite::Result<()> {
    if rag_rat_db::schema::repo_id_is_registered(conn, repo_id)? {
        rag_rat_db::meta::bump_lens_revisions(conn, repo_id, &[
            rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
            rag_rat_db::meta::LENS_PAPERTRAIL_REVISION_META,
        ])?;
    }
    Ok(())
}
