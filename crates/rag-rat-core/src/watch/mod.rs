//! Background file watcher: keeps the active index (and dirty-worktree overlay) fresh as files
//! change, so graph/symbol queries reflect uncommitted edits without waiting for a commit.
//!
//! - **One watcher per worktree** via the election lock; **one writer at a time per DB** via the
//!   write lock (see [`rag_rat_base::locks`]).
//! - Watches the configured target *directories* and their non-ignored subtrees (so **new files**
//!   are seen) — placing a watch per non-ignored directory rather than one recursive watch, so a
//!   gitignored build/dependency tree can't exhaust `fs.inotify.max_user_watches` (issue #331).
//!   Classifies events through the target globs to decide whether to fire, and debounces bursts
//!   with a max-latency cap so sustained writes can't starve a pass.
//! - Each pass runs the existing pipeline: discover → reconcile → (rate-limited) gc →
//!   memory_validate. Discover handles additions/edits/deletions; the pass is idempotent. A pass
//!   whose only change is a linked-worktree overlay skips the base reconcile / clone stages and
//!   runs just memory_validate (#817).
//! - Passes execute on a dedicated worker thread, never on the event loop (#506): a long stage
//!   (cold embedding backlog, clone-graph rebuild) or a blocked write-lock acquisition must not
//!   stop events from classifying or the fleet hot-upgrade trigger from firing. One pass in flight
//!   at a time; fire conditions that arrive mid-pass coalesce into the armed debounce.
//! - Papertrail auto-sync (#592) runs on its OWN worker with its own periodic deadline: the
//!   freshness probe and daily full-walk backstop fire even on a filesystem-idle watcher and are
//!   never postponed by an in-flight maintenance pass.

mod event_loop;
mod overlay;
mod papertrail;
mod pass;
mod placement;

pub use event_loop::Watcher;
#[cfg(test)]
pub(crate) use event_loop::{EventLoop, flush_watch_placement_failures, shutdown_discover};
#[cfg(test)]
pub(crate) use overlay::{
    OverlayBasisAction, overlay_basis_action, overlay_needs_embed, partition_paths_by_worktree,
};
pub use overlay::{
    OverlayScope, ReconcileBudget, is_manifest_path, refresh_worktree_overlays, reindex_paths,
};
#[cfg(test)]
pub(crate) use papertrail::{PapertrailClock, PapertrailScheduler, papertrail_tick_interval};
pub use pass::{CLONE_GRAPH_QUIET_MS, maintenance_pass, maintenance_pass_or_skip};
#[cfg(test)]
pub(crate) use pass::{
    Debounce, GC_EVERY_PASSES, LoopMsg, PassCooldown, PassRequest, PassScheduler,
    STARTUP_CATCHUP_RUN_GC, SweepClock, base_embedding_backlog_needs_tail,
    base_tail_forced_by_state, maybe_checkpoint_wal, should_run_base_tail, spawn_pass_worker,
    startup_catchup_pass,
};
#[cfg(test)]
pub(crate) use placement::{
    CreatedDirPlacement, LinkedWorktreeWatches, WatchPlacementCounters, WorktreeEventHint,
    created_dir_placement, event_is_relevant, event_requests_maintenance, event_touches_worktree,
    gitignore_rule_watch_dirs, gitignore_watch_dirs, is_gitignore_path, kind_is_mutation,
    missing_config_root_bootstrap_dirs, place_initial_watch_state,
    recompile_ignore_and_place_watches, sync_linked_worktrees_after_pass, watch_created_dirs,
    watch_linked_worktrees, watch_tree_pruned, worktree_watch_targets,
};

#[cfg(test)]
mod tests;
