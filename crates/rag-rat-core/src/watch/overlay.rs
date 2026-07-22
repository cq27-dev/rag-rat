use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rag_rat_base::config::Config;

use crate::index::ai::ReconcileOptions;
use crate::index::{
    IndexDatabase, IndexProgress, OverlayBasisUpdate, OverlayLogicalRebuild, OverlayRefreshTail,
};

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
        // The tail (#819/#824): defer the repo-global logical-symbol rebuild to ONE run after
        // this loop, and let the refresh maintain the #577 basis INSIDE its own transaction —
        // record on complete, clear on partial — instead of a separate autocommit per worktree.
        // (A non-sibling refresh never opens the transaction and leaves any basis untouched.)
        let tail = OverlayRefreshTail {
            logical_rebuild: OverlayLogicalRebuild::Deferred,
            basis: Some(OverlayBasisUpdate { base_sha: &base_sha, linked_head_sha: &linked_head }),
        };
        match db.index_worktree_overlay_with_tail(
            &overlay_config,
            Path::new(&worktree),
            tail,
            &mut |_| {},
        ) {
            Ok(report) => {
                let this_changed = report.indexed > 0 || report.tombstoned > 0 || report.pruned > 0;
                changed |= this_changed;
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
    // ONE repo-global logical-symbol rebuild for the whole batch (#819): each changed overlay
    // transaction above marked it pending instead of paying the full DELETE-all + re-derive per
    // worktree (only the last rebuild's output would survive anyway). Runs even when a worktree
    // errored mid-loop — the marker rode the transactions that DID commit. Best-effort like the
    // per-worktree refresh: on failure the marker survives its rollback and the next pass
    // retries.
    if let Err(err) = db.apply_pending_logical_rebuild() {
        eprintln!("watch: batch logical-symbol rebuild failed: {err}");
    }
    // Restore the base scope for the rest of the pass (index_worktree_overlay leaves the connection
    // scoped to the last worktree it touched).
    let _ = db.use_worktree_scope(&config.root, None);
    changed
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

/// The `index --paths` orchestration (#659) — the edit-driven-reindex substrate. Reconciles exactly
/// the supplied candidate `paths`, ROUTING each to the index that owns it:
/// - a path under `config.root`'s own checkout goes through a scoped base
///   [`IndexDatabase::index_paths_with_progress`] pass, which reconciles EXACTLY those paths
///   (content-hash decides staleness; ignored / out-of-target / unchanged paths are no-ops; a
///   vanished path is tombstoned);
/// - a path under a LINKED worktree goes through that checkout's OVERLAY instead. This is
///   load-bearing: `find_config` re-anchors `config.root` to the MAIN checkout, so a linked edit is
///   not in the base tree — a base pass would `strip_prefix` it away and silently drop it. NOTE the
///   overlay pass is per-CHECKOUT, not per-path: [`IndexDatabase::index_worktree_overlay`]
///   reindexes the worktree's whole base↔branch delta (there is no path-scoped overlay primitive
///   today — see #679), so a linked `index --paths` also refreshes any OTHER pending change in that
///   checkout. A failure PROPAGATES (unlike the best-effort watcher/maintenance sweep), so an edit
///   hook can retry instead of exiting 0 on a drop.
///
/// The per-repo `WriteLock` is held across BOTH halves (the base pass re-acquires it reentrantly),
/// mirroring the maintenance path. Returns the base db so the caller can read base-scoped status.
/// #427 first-time-empty deferral is inherited from the base pass — an unregistered base surfaces
/// `EmptyIndexRefused` for the caller to defer on (a linked overlay is a delta vs the base, so the
/// base must exist first).
pub fn reindex_paths<F>(
    config: &Config,
    paths: &[PathBuf],
    mut progress: F,
) -> anyhow::Result<IndexDatabase>
where
    F: FnMut(IndexProgress),
{
    let lock_repo = rag_rat_base::locks::write_lock_repo_id(config);
    let _lock = rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;

    let WorktreePartition { base_paths, linked, manifest_roots } =
        partition_paths_by_worktree(config, paths);
    // Always run the base pass: it opens the db (returned for status + the overlay refresh) and
    // enforces #427. With no base paths it is a no-op scoped pass over an empty change set.
    let mut db = IndexDatabase::index_paths_with_progress(config, &base_paths, &mut progress)?;
    let overlay_result = (|| -> anyhow::Result<()> {
        for (checkout, checkout_paths) in &linked {
            // Index EXACTLY the supplied paths of each touched checkout's overlay (#679), with the
            // LINKED branch's OWN targets (a branch that adds/narrows targets must be indexed
            // by its own config). Path-scoped, not the whole base↔branch delta: a single-file
            // linked edit no longer pulls in unrelated in-flight changes in the same worktree —
            // the base `Paths` exact-path semantics on the linked route. `?` PROPAGATES a
            // failure — the caller (an edit hook) must be able to tell the reindex did not land
            // and retry.
            //
            // EXCEPT a branch `rag-rat.toml` or `.gitignore` edit: both change the indexable set
            // across the WHOLE overlay — a config edit re-languages / adds / drops
            // targets; a `.gitignore` edit flips OTHER files' ignore status — and
            // neither is itself a source target, so the path-scoped route would no-op
            // on them and leave the drift for a later sweep. Run the whole-delta pass,
            // which reconciles both (`overlay_target_config_reconcile` for target
            // drift, `expand_candidates_for_ignore_only_flips` for ignore flips) (#679 review).
            // Such edits are rare, so paying the full tree-diff there is fine.
            let overlay_config = config.for_linked_worktree_overlay(checkout);
            let overlay_wide_edit = checkout_paths
                .iter()
                .any(|path| is_config_path(path) || super::placement::is_gitignore_path(path));
            // Defer the repo-global logical-symbol rebuild to ONE run after this loop (#819) — an
            // `index --paths` batch spanning several checkouts would otherwise pay one full
            // DELETE-all + re-derive per checkout. The basis pair is not maintained in-txn here:
            // this flow never RECORDS a basis (it captures no head pair), it only clears below.
            let report = if overlay_wide_edit {
                db.index_worktree_overlay_with_tail(
                    &overlay_config,
                    checkout,
                    OverlayRefreshTail {
                        logical_rebuild: OverlayLogicalRebuild::Deferred,
                        basis: None,
                    },
                    &mut progress,
                )?
            } else {
                db.index_worktree_overlay_paths(
                    &overlay_config,
                    checkout,
                    checkout_paths,
                    OverlayLogicalRebuild::Deferred,
                    &mut progress,
                )?
            };
            // A path-scoped pass is never a complete overlay refresh (`status_complete = false`),
            // so — like the watcher on a partial read — CLEAR this worktree's overlay
            // basis so the next full sweep re-refreshes anything else in the branch
            // instead of skipping on a still-matching basis (#659/#679).
            if !report.status_complete && !report.worktree_id.is_empty() {
                let _ = db.clear_worktree_overlay_basis(&report.worktree_id);
            }
            // A SUPPLIED `Cargo.toml` must refresh the overlay's package map even when it is CLEAN/
            // committed (so it produced no source rows and the overlay's status-derived signal
            // missed it) — the base flow refreshes on a named manifest regardless of
            // dirtiness, and the linked route must honor the same "also sees committed
            // changes" contract (#659 review).
            if manifest_roots.contains(checkout) {
                db.refresh_worktree_overlay_packages(&overlay_config, checkout)?;
            }
        }
        Ok(())
    })();
    // Run the batch's ONE deferred logical-symbol rebuild (#819) BEFORE propagating a mid-loop
    // error: the checkouts that already committed marked it pending, and skipping it would leave
    // their newly indexed symbols unresolvable until another pass. When the loop failed, the loop
    // error wins; a rebuild failure here leaves the pending marker committed, so the next
    // maintenance pass retries it.
    let rebuild_result = db.apply_pending_logical_rebuild();
    overlay_result?;
    rebuild_result?;
    // The overlay passes / the manifest refresh leave the connection scoped to the LAST overlay;
    // restore the base scope so the returned db reads base-scoped status.
    if !linked.is_empty() {
        let (base_sha, base_id) = crate::index::resolve_git_context(&config.root);
        db.set_context(&base_sha, &base_id)?;
    }
    Ok(db)
}

/// Whether `path` is a package manifest (`Cargo.toml`) — a file the scoped reindex refreshes the
/// package map for (see [`reindex_paths`]'s manifest handling), but that the file watcher's event
/// filter (configured targets + `.gitignore`) never fires on. The PostToolUse edit hook uses this
/// to still reindex a manifest edit when a watcher is live — the watcher would silently miss it.
pub fn is_manifest_path(path: &Path) -> bool {
    path.file_name() == Some(std::ffi::OsStr::new("Cargo.toml"))
}

/// Whether `path` is a `rag-rat.toml` config file. A branch config edit can re-language / add /
/// drop targets across the whole overlay, so `reindex_paths` routes it to the WHOLE-delta overlay
/// pass (which runs the target-drift reconcile) rather than the path-scoped one (#679).
fn is_config_path(path: &Path) -> bool {
    path.file_name() == Some(std::ffi::OsStr::new("rag-rat.toml"))
}

/// Candidate paths split by which checkout owns them (see [`partition_paths_by_worktree`]).
pub(crate) struct WorktreePartition {
    /// Paths under the base checkout (and non-git trees) — the scoped base pass reconciles these.
    pub(crate) base_paths: Vec<PathBuf>,
    /// Live linked-worktree checkout root → the supplied paths under it. Each root gets a
    /// PATH-SCOPED overlay pass over exactly its paths (#679), not the whole checkout delta.
    pub(crate) linked: BTreeMap<PathBuf, Vec<PathBuf>>,
    /// Linked checkout roots that had a supplied `Cargo.toml`. Their overlay package map must be
    /// refreshed even for a CLEAN/committed manifest — the overlay's own signal is status-derived
    /// (dirty-only), so this carries the base flow's supplied-manifest signal to the linked route.
    pub(crate) manifest_roots: BTreeSet<PathBuf>,
}

/// Split absolute candidate paths by owning checkout. A path is a linked-worktree path when its
/// (canonicalized) location lives under a live linked checkout root; everything else — including
/// non-git trees and paths already under the base checkout — is a base path. The linked roots are
/// the [`crate::index::live_worktree_contexts`] spellings (canonicalized, base excluded), so the
/// base pass and the overlay refresh key on the exact same identities the rest of the engine uses.
/// Each linked path is bucketed under its root so the overlay pass reconciles exactly those paths.
pub(crate) fn partition_paths_by_worktree(config: &Config, paths: &[PathBuf]) -> WorktreePartition {
    let (_, worktrees) = crate::index::live_worktree_contexts(&config.root);
    let base = enclosing_worktree_id(&config.root);
    let linked: Vec<PathBuf> =
        worktrees.into_iter().filter(|w| *w != base).map(PathBuf::from).collect();
    let mut partition = WorktreePartition {
        base_paths: Vec::new(),
        linked: BTreeMap::new(),
        manifest_roots: BTreeSet::new(),
    };
    for path in paths {
        match enclosing_linked_root(path, &linked) {
            Some(root) => {
                if is_manifest_path(path) {
                    partition.manifest_roots.insert(root.clone());
                }
                partition.linked.entry(root).or_default().push(path.clone());
            },
            None => partition.base_paths.push(path.clone()),
        }
    }
    partition
}

/// The live linked checkout root `path` lives under, or `None` if it is under the base checkout (or
/// no linked worktree). Compares against the canonicalized `linked` roots after canonicalizing
/// `path` itself — via the shared [`crate::index::canonicalize_nearest_ancestor`], which walks up
/// to the nearest EXISTING ancestor (a deletion may have removed the file AND its parent dir, e.g.
/// `rm -rf wt/src`) so a symlinked checkout dir (`/tmp` → `/private/tmp`) is never misrouted to the
/// base. Falls back to the lexical path only when nothing on the chain resolves. Picks the MOST
/// SPECIFIC (longest) matching root, not the first: a worktree NESTED under another worktree's dir
/// makes `path` a prefix-match of BOTH, and the nested checkout is the real owner — a first-match
/// would refresh the outer overlay and leave the nested one stale.
fn enclosing_linked_root(path: &Path, linked: &[PathBuf]) -> Option<PathBuf> {
    let canonical =
        crate::index::canonicalize_nearest_ancestor(path).unwrap_or_else(|| path.to_path_buf());
    linked
        .iter()
        .filter(|root| canonical.starts_with(root))
        .max_by_key(|root| root.components().count())
        .cloned()
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
    #[cfg(unix)]
    fn enclosing_linked_root_picks_the_most_specific_nested_worktree() {
        // A worktree NESTED under another worktree's dir makes a path a prefix-match of BOTH roots;
        // the nested checkout is the real owner. Longest match must win regardless of the order
        // `live_worktree_contexts` returns the roots in (#659 review). (Non-existent paths → the
        // canonicalize fallback is the lexical path, so this white-boxes the selection.)
        let outer = PathBuf::from("/wt/outer");
        let inner = PathBuf::from("/wt/outer/inner");
        let path = PathBuf::from("/wt/outer/inner/src/x.rs");
        assert_eq!(
            enclosing_linked_root(&path, &[outer.clone(), inner.clone()]),
            Some(inner.clone()),
            "outer-first: the nested root still wins",
        );
        assert_eq!(
            enclosing_linked_root(&path, &[inner.clone(), outer.clone()]),
            Some(inner.clone()),
            "inner-first: the nested root still wins",
        );
        // A path under only the outer root routes to the outer.
        assert_eq!(
            enclosing_linked_root(Path::new("/wt/outer/top.rs"), &[outer.clone(), inner]),
            Some(outer),
        );
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
