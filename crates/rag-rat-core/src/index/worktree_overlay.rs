//! Linked-worktree overlay indexing (#219 stage 2).
//!
//! A linked worktree (a sibling working tree over the same `.git`, usually on a feature branch) has
//! files that differ from the base scope (the rooted checkout's HEAD). This pass indexes ONLY those
//! differences as worktree-scoped OVERLAY rows that shadow the base — and writes tombstones for
//! files the branch removed, so the scope view HIDES them instead of falling through to the base
//! row.
//!
//! Control flow is a dedicated, named pass (`index_worktree_overlay`); the actual read/parse/chunk/
//! symbol/edge work reuses the existing per-file pipeline through the lower-level
//! `index_explicit_paths_from_root` primitive (which knows nothing about worktree deltas — just
//! "index these paths from this root into this scope"). Delta selection is NOT normal discovery
//! ("what changed under config.root?") — it answers "what differs between the base scope and this
//! sibling checkout?" — so it lives here, not in the incremental discovery path.

use std::collections::{BTreeSet, HashMap};

use rag_rat_base::hash::hex_sha256;
use rag_rat_base::paths::path_string;

use super::*;

mod discovery;
mod lifecycle;
mod materialize;
mod query_adaptation;

pub use discovery::WorktreeOverlayReport;
#[cfg(test)]
use discovery::fold_status_candidates;
use discovery::resolve_overlay_scope;
pub(crate) use discovery::{
    CommittedDeltaSource, ResolvedOverlayScope, compute_linked_worktree_delta, linked_source_root,
};

/// `repo_meta` key prefix for a linked worktree's overlay refresh basis (#577): one key per
/// `worktree_id`, value `"<base_sha>\n<linked_head_sha>\n<refreshed_at_ms>"` — the (base HEAD,
/// linked HEAD) pair the overlay delta was last computed against, plus when that COMPLETE refresh
/// recorded it (epoch ms, the #822 quiet-window anchor). A scoped watcher pass skips a worktree
/// not implicated by events ONLY while this pair still matches; either head moving (a base commit
/// re-basing every overlay, a linked commit with no file event) forces the refresh. A pre-#822
/// two-line value still yields the pair — its missing timestamp just disarms the quiet window
/// (never the safe-direction refresh). Kept per worktree in the per-repo kv rather than a
/// dedicated table: it is a marker, not queried relationally, and the
/// `watch_shutdown_reconcile_pending` marker set the pattern.
const WORKTREE_OVERLAY_BASIS_META_PREFIX: &str = "worktree_overlay_basis:";

/// A parsed refresh-basis value: the #577 skip-proof pair plus, when recorded by a #822-aware
/// build, the recording refresh's timestamp. Internal to the two projection readers so the meta
/// value is parsed in exactly one place.
struct RecordedOverlayBasis {
    base_sha: String,
    linked_head_sha: String,
    refreshed_at_ms: Option<i64>,
}

/// `repo_meta` key marking that a committed overlay refresh deferred the repo-global
/// `logical_symbols` rebuild to its batch's tail (#819). Set INSIDE each overlay transaction that
/// changes source rows under [`OverlayLogicalRebuild::Deferred`]; consumed by the batch tail
/// ([`IndexDatabase::apply_pending_logical_rebuild`]). Persisted rather
/// than tracked in memory so a crash between a committed overlay transaction and the batch tail
/// leaves the obligation visible: the next pass must run the rebuild even though every overlay
/// row is unchanged (idle-skipped) by then — otherwise a newly added overlay file's symbols would
/// stay unresolvable until an unrelated change triggered a rebuild. `rebuild_logical_symbols` is
/// the sole CLEARER: any successful rebuild — the batch tail, an inline overlay refresh, a heal,
/// an incremental or full pass — satisfies the obligation in the same transaction.
pub(super) const OVERLAY_LOGICAL_REBUILD_PENDING_META: &str = "overlay_logical_rebuild_pending";

/// Whether an overlay refresh runs the repo-global logical-symbol rebuild inside its own
/// transaction, or defers it to one batch-tail rebuild (#819). `logical_symbols` is repo-scoped
/// but scope-INDEPENDENT (see `rebuild_logical_symbols`), so when one pass refreshes K worktrees
/// only the LAST rebuild's output survives — K inline rebuilds are K−1 wholesale
/// DELETE-all + re-derive passes of pure write amplification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayLogicalRebuild {
    /// Rebuild inside this refresh's transaction — atomic with the overlay rows. The standalone
    /// single-checkout shape (CLI `index --worktree`, tests): nothing to deduplicate.
    Inline,
    /// Skip the rebuild and mark it pending in the same transaction. The batch caller MUST follow
    /// up with [`IndexDatabase::apply_pending_logical_rebuild`] after its loop.
    Deferred,
}

/// The `(base HEAD, linked HEAD)` pair a watcher pass wants recorded as the worktree's #577
/// skip-proof refresh basis. Both heads are read by the CALLER around the refresh (base once per
/// pass, linked before the delta) so a commit racing the refresh records the pre-refresh head —
/// mismatching (and re-refreshing) next pass rather than skipping a stale overlay.
#[derive(Debug, Clone, Copy)]
pub struct OverlayBasisUpdate<'a> {
    pub base_sha: &'a str,
    pub linked_head_sha: &'a str,
}

/// Caller-owned handling of an overlay refresh's repo-global tail (#819/#824): how the
/// logical-symbol rebuild runs, and whether the refresh maintains the worktree's #577 refresh
/// basis inside its own transaction.
#[derive(Debug, Clone, Copy)]
pub struct OverlayRefreshTail<'a> {
    pub logical_rebuild: OverlayLogicalRebuild,
    /// `Some` = maintain the #577 skip-proof basis in the refresh transaction (#824): record the
    /// pair on a COMPLETE refresh, clear the worktree's recorded basis on a PARTIAL one. (A
    /// FAILED refresh rolls the transaction back — the caller clears after the rollback, where a
    /// transactional clear cannot survive.) `None` = leave any recorded basis untouched.
    pub basis: Option<OverlayBasisUpdate<'a>>,
}

impl OverlayRefreshTail<'_> {
    /// The standalone, self-contained refresh: rebuild logical symbols inline (atomic with the
    /// overlay transaction) and leave any recorded refresh basis untouched.
    pub const STANDALONE: OverlayRefreshTail<'static> =
        OverlayRefreshTail { logical_rebuild: OverlayLogicalRebuild::Inline, basis: None };
}

/// One overlay refresh's row-change counts — the gate for its finalize tail (any change means the
/// logical-symbol/package/edge/FTS refresh runs; none means the pass stays write-free).
struct OverlayChangeCounts {
    indexed: usize,
    tombstoned: usize,
    pruned: usize,
}

impl OverlayChangeCounts {
    fn any_changed(&self) -> bool {
        self.indexed > 0 || self.tombstoned > 0 || self.pruned > 0
    }
}

#[cfg(test)]
mod tests;
