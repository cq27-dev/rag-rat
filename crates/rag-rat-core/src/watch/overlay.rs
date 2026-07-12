use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::Config;
use crate::index::IndexDatabase;
use crate::index::ai::ReconcileOptions;

/// The canonical worktree id string [`crate::index::live_worktree_contexts`] reports. When `root`
/// is a repo SUBDIR (`<repo>/crate`), the enclosing worktree root is `<repo>` — which is the
/// spelling the main checkout contributes to `live_worktree_contexts`. Filtering live worktrees by
/// `worktree_id_of(root)` (the subdir path) instead would never match that entry, so the main
/// checkout would be misread as a LINKED overlay. Falls back to `root`'s own id outside a git
/// worktree (#219 review).
pub(crate) fn enclosing_worktree_id(root: &Path) -> String {
    crate::index::git_history::worktree_root(root)
        .map_or_else(|| crate::index::worktree_id_of(root), |wt| crate::index::worktree_id_of(&wt))
}

/// Which linked-worktree overlays a maintenance pass refreshes (#577). The per-worktree refresh
/// is write-idle on an unchanged worktree but never FREE: each one pays a base↔linked tree diff,
/// a full working-tree status walk, and an `IgnoreMatcher` compile. Sweeping every live worktree
/// on every pass made the pass cost scale with the whole worktree fleet instead of with what
/// changed — on a repo with several active agent worktrees that was ~3 s of overlay sweep per
/// otherwise-idle pass, all day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayScope {
    /// Refresh every live linked worktree: startup catch-up, the periodic sweep (the backstop for
    /// missed events), gc passes, and the CLI/hook `maintenance` command.
    All,
    /// Refresh the listed checkout roots (the ones the watcher attributed events to since the
    /// last dispatch) — plus any worktree whose recorded refresh basis no longer matches, so a
    /// base or linked commit is never missed just because no file event named the checkout. An
    /// empty set is a base-only pass: discovery covers the base scope regardless of this value.
    Linked(BTreeSet<PathBuf>),
}

impl OverlayScope {
    /// Whether `worktree_id` (the canonical id `live_worktree_contexts` reports) is listed.
    /// Scope roots are event/checkout paths; compare via `worktree_id_of` so the event spelling
    /// and the overlay key can't drift (the same canonicalization every scope consumer uses).
    pub(crate) fn lists(&self, worktree_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Linked(roots) =>
                roots.iter().any(|root| crate::index::worktree_id_of(root) == worktree_id),
        }
    }

    /// Fold another event's contribution into the scope accumulated while the debounce is armed:
    /// attributable roots union; an unattributable contribution widens the whole pass to `All`.
    pub(crate) fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::Linked(mut roots), Self::Linked(more)) => {
                roots.extend(more);
                Self::Linked(roots)
            },
        }
    }
}

/// Refresh the branch overlay of live LINKED worktrees of `config.root`'s repo (#219), so a
/// `worktree`-scoped query stays current without a manual `index --worktree`. Returns whether any
/// overlay actually changed. `index_worktree_overlay` is delta-only and idle-safe (a static
/// worktree writes nothing), and the connection is restored to the base scope afterward so the rest
/// of the pass (reconcile / gc / memory-validate) runs unscoped as before. Best-effort per worktree
/// — a failure on one worktree is logged and doesn't abort the pass.
///
/// `reconcile` (when `Some`): after a CHANGED overlay is indexed — while the connection is STILL
/// scoped to that overlay — reconcile its embeddings, so worktree-scoped `semantic_search` is not
/// BM25-only for branch content. The pass's trailing reconcile runs AFTER this returns, when the
/// connection is back on the base scope (the `files` view = base rows), so it never sees overlay
/// chunks; reconciling here is the only point the overlay scope is active (#219 review). Embeddings
/// for a NEW/MODIFIED overlay chunk are written keyed by chunk id (shared `chunk_embeddings`
/// table), which the overlay scope reads through its own `files` view. `None` skips overlay
/// reconcile (the caller has no embedder/options).
///
/// `scope` (#577): which worktrees to refresh. The watcher's event-driven passes name the
/// checkouts events came from; everything else (`All`) sweeps the fleet. A worktree outside the
/// scope is still refreshed when its recorded basis — the (base HEAD, linked HEAD) pair its
/// overlay was last computed against — no longer matches: a BASE commit moves the diff basis for
/// every worktree at once, and a LINKED commit arrives with no file event for the checkout (hooks,
/// or edits made while no watcher ran). Working-tree edits in a skipped worktree surface via
/// events, or at latest via the next `All` sweep (`periodic_sweep_secs`) — the same missed-event
/// backstop the watcher already relies on.
///
/// `pub` so the hook-driven CLI `maintenance` command shares this exact path: the git hooks invoke
/// `rag-rat maintenance` (not the foreground watcher), so without calling this a commit/checkout/
/// merge in a linked worktree would index the base `config.root` but leave that worktree's overlay
/// stale until a watcher pass or a manual `index --worktree` (#219 review).
pub fn refresh_worktree_overlays(
    db: &mut IndexDatabase,
    config: &Config,
    reconcile: Option<&ReconcileBudget>,
    scope: &OverlayScope,
) -> bool {
    let (_, worktrees) = crate::index::live_worktree_contexts(&config.root);
    // The base id is the ENCLOSING worktree root, not `config.root` itself — see
    // `enclosing_worktree_id` (a repo-SUBDIR `config.root` would otherwise mis-classify the main
    // checkout as a linked overlay and re-index it as one) (#219 review).
    let base_id = enclosing_worktree_id(&config.root);
    // The basis every refresh in this pass records: the base HEAD the overlay delta is computed
    // from. Read once — a base commit landing mid-pass records the pre-pass sha, which mismatches
    // on the next pass and re-refreshes (the safe direction).
    let base_sha = crate::index::head_sha(&config.root);
    let sweep = matches!(scope, OverlayScope::All);
    let mut changed = false;
    for worktree in worktrees {
        if worktree == base_id {
            continue; // the rooted checkout is the base scope, not an overlay
        }
        // The linked HEAD is read BEFORE the refresh, so a commit racing the refresh records the
        // pre-commit head — mismatching (and re-refreshing) next pass rather than skipping.
        let linked_head = crate::index::head_sha(Path::new(&worktree));
        if !scope.lists(&worktree)
            && db.worktree_overlay_basis(&worktree).ok().flatten()
                == Some((base_sha.clone(), linked_head.clone()))
        {
            continue; // not implicated by events and the diff basis is unchanged (#577)
        }
        // Refresh the overlay with the LINKED worktree's OWN config targets, not the sweeping
        // process's. A branch whose `rag-rat.toml` ADDS a target (e.g. `extra/`) would otherwise be
        // filtered against the sweeper's targets, and a complete-status pass would PRUNE the
        // overlay rows a branch-launched hook indexed for it. `for_linked_worktree_overlay`
        // keeps the shared base `root`/`database` but swaps in the branch's target set
        // (#219 review).
        let overlay_config = config.for_linked_worktree_overlay(Path::new(&worktree));
        match db.index_worktree_overlay(&overlay_config, Path::new(&worktree), &mut |_| {}) {
            Ok(report) => {
                let this_changed = report.indexed > 0 || report.tombstoned > 0 || report.pruned > 0;
                changed |= this_changed;
                match overlay_basis_action(&report) {
                    // Record the refresh basis so later scoped passes can prove "unchanged" from
                    // two head reads instead of re-computing the delta (#577). Best-effort: a
                    // failed write just means the next pass refreshes again.
                    OverlayBasisAction::Record => {
                        let _ =
                            db.record_worktree_overlay_basis(&worktree, &base_sha, &linked_head);
                    },
                    OverlayBasisAction::Clear => {
                        let _ = db.clear_worktree_overlay_basis(&worktree);
                    },
                    OverlayBasisAction::Keep => {},
                }
                // Embed the overlay's chunks NOW, while the connection is still scoped to this
                // overlay (index_worktree_overlay left it there) — the trailing base reconcile
                // won't see them (#219 review). Run when the overlay CHANGED, OR — on an `All`
                // sweep only — when it has a BACKLOG of un-embedded chunks: an earlier pass's
                // inline reconcile may have returned `Partial` (the shared time budget ran out
                // mid-pass), leaving overlay chunks un-embedded. The next pass sees the overlay
                // rows as unchanged and would skip the embed forever, so a worktree-scoped
                // `semantic_search` would stay BM25-only for that branch content until an
                // unrelated file change; the sweep's `pending_embedding_jobs_with_options` count
                // (active overlay scope, SQL-only — no embedder acquisition, so an idle pass makes
                // no embed request) retries it within one `periodic_sweep_secs` (#219 review,
                // #577). A backlogged reconcile with the embedder unavailable defers inside
                // `reconcile_with_options_progress` itself (`provision_remote=false`).
                // `budget.next_options()` recomputes `max_seconds` from the time left in the
                // SHARED budget so overlays + base can't each spend the full `--max-seconds`;
                // `None` → budget exhausted, skip and let the NEXT pass retry.
                let needs_embed = overlay_needs_embed(this_changed, sweep, reconcile, |options| {
                    db.pending_embedding_jobs_with_options(options).is_ok_and(|pending| pending > 0)
                });
                if needs_embed
                    && let Some(budget) = reconcile
                    && let Some(options) = budget.next_options()
                    && let Err(err) = db.reconcile_with_options_progress(options, |_| {})
                {
                    eprintln!("watch: worktree overlay reconcile failed for {worktree}: {err}");
                }
            },
            Err(err) => {
                // A failed refresh may have left the overlay stale while both heads still match
                // (a dirty edit moves no HEAD) — drop the skip proof so scoped passes keep
                // refreshing this worktree until a pass completes (#577 review).
                let _ = db.clear_worktree_overlay_basis(&worktree);
                eprintln!("watch: worktree overlay refresh failed for {worktree}: {err}");
            },
        }
    }
    // Restore the base scope for the rest of the pass (index_worktree_overlay leaves the connection
    // scoped to the last worktree it touched).
    let _ = db.use_worktree_scope(&config.root, None);
    changed
}

/// What a refresh outcome does to the worktree's skip-proof basis (#577).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OverlayBasisAction {
    /// A COMPLETE refresh of a real linked sibling: the heads captured around it prove the
    /// overlay current.
    Record,
    /// A PARTIAL refresh (the working-tree status read failed midway): dirty/untracked/deleted
    /// paths may be missing while neither HEAD moved, so a previously recorded basis would keep
    /// matching and scoped passes would skip the stale overlay until an `All` pass. Drop it so
    /// they keep refreshing until a complete pass lands.
    Clear,
    /// Not a linked sibling — there is no overlay to prove anything about.
    Keep,
}

pub(crate) fn overlay_basis_action(
    report: &crate::index::WorktreeOverlayReport,
) -> OverlayBasisAction {
    if report.worktree_id.is_empty() {
        OverlayBasisAction::Keep
    } else if report.status_complete {
        OverlayBasisAction::Record
    } else {
        OverlayBasisAction::Clear
    }
}

pub(crate) fn overlay_needs_embed(
    this_changed: bool,
    sweep_backlog_probe: bool,
    reconcile: Option<&ReconcileBudget>,
    pending_embedding_jobs: impl FnOnce(&ReconcileOptions) -> bool,
) -> bool {
    if this_changed {
        return true;
    }
    // The backlog probe is an O(scope) candidate scan; it belongs to the `All` sweep, not to
    // every event-scoped pass over an unchanged worktree (#577). A `Partial` drain therefore
    // heals within one `periodic_sweep_secs` instead of being re-probed per pass.
    if !sweep_backlog_probe {
        return false;
    }
    reconcile
        .and_then(ReconcileBudget::next_options)
        .is_some_and(|options| pending_embedding_jobs(&options))
}

/// A time budget shared across the per-overlay embedding reconciles AND the trailing base reconcile
/// of one maintenance/watcher pass. Each reconcile call starts its own `Instant` timer against its
/// `max_seconds`, so handing every overlay (and the base) the same `ReconcileOptions` would let
/// each spend the FULL advertised budget — N overlays + base = (N+1)×`max_seconds` of held write
/// lock. `next_options` recomputes `max_seconds` from the time remaining since `start`, so the
/// whole pass stays within a single budget (#219 review). A budget with no `max_seconds` cap
/// (`None`) is unbounded and every `next_options` returns the base options unchanged.
pub struct ReconcileBudget {
    options: ReconcileOptions,
    start: Instant,
    total_seconds: Option<u64>,
}

impl ReconcileBudget {
    /// Build a shared budget. `start` is the pass's clock origin — pass the SAME instant the
    /// surrounding command measured its own setup against (so discovery time already spent counts
    /// toward the budget); the per-call `max_seconds` is `options.max_seconds` minus elapsed.
    pub fn new(options: ReconcileOptions, start: Instant) -> Self {
        let total_seconds = options.max_seconds;
        Self { options, start, total_seconds }
    }

    /// The options for the NEXT reconcile in this pass, with `max_seconds` reduced to the time left
    /// in the shared budget. `None` when the budget is exhausted (so the caller skips the reconcile
    /// entirely rather than running it with a zero budget). An uncapped budget always yields the
    /// base options.
    pub fn next_options(&self) -> Option<ReconcileOptions> {
        let Some(total) = self.total_seconds else {
            return Some(self.options.clone());
        };
        let remaining = total.saturating_sub(self.start.elapsed().as_secs());
        if remaining == 0 {
            return None;
        }
        let mut options = self.options.clone();
        options.max_seconds = Some(remaining);
        Some(options)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::index::ai::ReconcileOptions;

    #[test]
    fn overlay_scope_lists_all_and_linked_roots() {
        let linked = OverlayScope::Linked(BTreeSet::from([PathBuf::from("/tmp/wt-a")]));
        let id = crate::index::worktree_id_of(Path::new("/tmp/wt-a"));
        assert!(OverlayScope::All.lists(&id));
        assert!(linked.lists(&id));
        assert!(!linked.lists("some-other-worktree"));
    }

    #[test]
    fn overlay_scope_merge_unions_or_widens_to_all() {
        let a = OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/a")]));
        let b = OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/b")]));
        let merged = a.clone().merge(b);
        assert_eq!(
            merged,
            OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/a"), PathBuf::from("/wt/b")]))
        );
        assert_eq!(a.merge(OverlayScope::All), OverlayScope::All);
    }

    #[test]
    fn enclosing_worktree_id_falls_back_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("crate");
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(
            enclosing_worktree_id(&root),
            crate::index::worktree_id_of(&root),
            "non-git roots use their own worktree id"
        );
    }

    #[test]
    fn reconcile_budget_returns_none_when_exhausted() {
        let options = ReconcileOptions { max_seconds: Some(5), ..ReconcileOptions::default() };
        let spent = ReconcileBudget::new(options, Instant::now() - Duration::from_secs(10));
        assert!(spent.next_options().is_none());
    }
}
