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

use std::collections::BTreeSet;

use super::*;

/// Repo-relative paths whose linked-worktree state differs from the base scope, split into files to
/// index from the linked checkout (`readable`) and files to shadow with a tombstone (`tombstones` —
/// present in the base, absent from the linked working tree: branch/working-tree deletes and the
/// old side of a rename).
#[derive(Debug, Default)]
pub(crate) struct WorktreeOverlayDelta {
    pub(crate) readable: Vec<PathBuf>,
    pub(crate) tombstones: Vec<PathBuf>,
}

impl WorktreeOverlayDelta {
    /// Every path the overlay is authoritative for (readable + tombstone); anything else falls
    /// through to the base scope. Used to prune overlay rows that no longer differ from the base.
    pub(crate) fn shadowing_paths(&self) -> BTreeSet<PathBuf> {
        self.readable.iter().chain(&self.tombstones).cloned().collect()
    }
}

// Fields are read by the #219 stage-2 acceptance tests and by callers wired in stages 3-5; the lib
// build has no non-test reader yet.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub(crate) struct WorktreeOverlayReport {
    /// The overlay scope's `worktree_id`; empty when `linked_path` was not a valid linked sibling
    /// (the pass was skipped).
    pub(crate) worktree_id: String,
    pub(crate) indexed: usize,
    pub(crate) tombstoned: usize,
    pub(crate) pruned: usize,
}

/// Compute the overlay delta of `linked_path` (a linked worktree of `config.root`'s repo) against
/// the base scope. Candidate paths = the committed branch diff (base HEAD tree ↔ linked HEAD tree)
/// UNION the linked worktree's working-tree status (dirty + untracked + deleted). Each candidate's
/// FINAL category is decided by its on-disk state in the LINKED checkout — present → read it
/// (readable); absent but present in the base tree → tombstone — which correctly merges committed
/// and working-tree changes (and maps a rename to delete-old + add-new). Only target-matching paths
/// are kept: the base wouldn't index the rest, so there is nothing to shadow.
pub(crate) fn compute_linked_worktree_delta(
    config: &Config,
    linked_path: &Path,
) -> anyhow::Result<WorktreeOverlayDelta> {
    let base_repo = git_context::discover_repo(&config.root)?;
    let linked_repo = git_context::discover_repo(linked_path)?;

    // Resolve BOTH trees through `base_repo` so the cross-tree diff shares one object store — the
    // worktrees share the same `.git`, so the linked HEAD's tree is reachable from base_repo by
    // oid.
    let base_tree = base_repo.head_id()?.object()?.peel_to_tree()?;
    let linked_head = linked_repo.head_id()?.detach();
    let linked_tree = base_repo.find_object(linked_head)?.peel_to_tree()?;

    let mut candidates: BTreeSet<PathBuf> = BTreeSet::new();

    // Committed branch diff. Rename detection OFF: a rename becomes delete(old)+add(new), which the
    // on-disk categorization below resolves to tombstone(old) + readable(new).
    base_tree
        .changes()?
        .options(|opts| {
            opts.track_path().track_rewrites(None);
        })
        .for_each_to_obtain_tree(&linked_tree, |change| {
            candidates.insert(change_location_path(&change));
            Ok::<_, std::convert::Infallible>(gix::object::tree::diff::Action::Continue(()))
        })?;

    // Linked working-tree status (vs the linked HEAD): dirty edits, untracked files, deletes.
    if let Ok(platform) = linked_repo.status(gix::progress::Discard)
        && let Ok(items) =
            platform.untracked_files(UntrackedFiles::Files).into_iter(None::<gix::bstr::BString>)
    {
        for item in items.flatten() {
            candidates.insert(PathBuf::from(item.location().to_str_lossy().as_ref()));
        }
    }

    let mut delta = WorktreeOverlayDelta::default();
    for rel in candidates {
        // Only paths the base scope would index can be shadowed/overlaid.
        if target_for_path(config, &rel).is_none() {
            continue;
        }
        if linked_path.join(&rel).is_file() {
            delta.readable.push(rel);
        } else if base_tree.lookup_entry_by_path(&rel).ok().flatten().is_some() {
            delta.tombstones.push(rel);
        }
    }
    Ok(delta)
}

fn change_location_path(change: &gix::object::tree::diff::Change<'_, '_, '_>) -> PathBuf {
    use gix::object::tree::diff::Change;
    let location = match change {
        Change::Addition { location, .. }
        | Change::Deletion { location, .. }
        | Change::Modification { location, .. }
        | Change::Rewrite { location, .. } => *location,
    };
    PathBuf::from(location.to_str_lossy().as_ref())
}

impl IndexDatabase {
    /// Index a linked worktree's branch/working-tree delta as overlay rows that shadow the base
    /// scope, and tombstone the files it removed (#219 stage 2). No-op (empty `worktree_id` in the
    /// report) when `linked_path` is not a valid linked sibling of `config.root`'s repo. Leaves the
    /// connection scope set to the overlay; callers re-`set_context` if they need another scope.
    #[allow(dead_code)] // Wired into the query/watcher paths in #219 stages 3-5.
    pub(crate) fn index_worktree_overlay<F>(
        &mut self,
        config: &Config,
        linked_path: &Path,
        progress: &mut F,
    ) -> anyhow::Result<WorktreeOverlayReport>
    where
        F: FnMut(IndexProgress),
    {
        let (base_sha, worktree_id) =
            git_context::resolve_worktree_scope(&config.root, Some(linked_path));
        // Fell back to base → not a valid linked sibling; nothing to overlay.
        if worktree_id == git_context::worktree_id_of(&config.root) {
            return Ok(WorktreeOverlayReport::default());
        }
        // Scope the connection to the overlay (base commit + linked worktree id) so context-
        // dependent steps (tombstones, FTS, edge resolution) operate in the linked scope.
        self.set_context(&base_sha, &worktree_id)?;

        let delta = compute_linked_worktree_delta(config, linked_path)?;
        let scope = FileScope::worktree(worktree_id.clone());
        let indexed = self.index_explicit_paths_from_root(
            config,
            linked_path,
            &delta.readable,
            &scope,
            progress,
        )?;
        for path in &delta.tombstones {
            self.write_tombstone_in_scope(path, &worktree_id)?;
        }
        let pruned =
            self.prune_overlay_rows_not_in_delta(&worktree_id, &delta.shadowing_paths())?;
        // The overlay inserted edges unresolved (apply_incremental_file_plan); resolve them in the
        // now-active overlay scope so the linked symbols are graph-connected.
        self.resolve_edges()?;

        Ok(WorktreeOverlayReport {
            worktree_id,
            indexed,
            tombstoned: delta.tombstones.len(),
            pruned,
        })
    }

    /// Index an EXPLICIT set of repo-relative `paths`, reading bytes from `source_root` (which may
    /// differ from `config.root` — e.g. a sibling linked worktree) but keying every row by the
    /// repo-relative logical path, with `scope`. The reusable primitive under
    /// `index_worktree_overlay` — it knows nothing about worktree deltas, just "index these
    /// paths from this root into this scope", applying the same target include/exclude policy
    /// as discovery and reusing the per-file prepare/insert pipeline
    /// (`apply_incremental_file_plan`).
    #[allow(dead_code)] // Reached via index_worktree_overlay (wired in #219 stages 3-5).
    pub(super) fn index_explicit_paths_from_root<F>(
        &self,
        config: &Config,
        source_root: &Path,
        paths: &[PathBuf],
        scope: &FileScope,
        progress: &mut F,
    ) -> anyhow::Result<usize>
    where
        F: FnMut(IndexProgress),
    {
        let mut files = Vec::new();
        for rel in paths {
            let full_path = source_root.join(rel);
            if !full_path.is_file() {
                continue;
            }
            let Some((language, kind)) = target_for_path(config, rel) else {
                continue;
            };
            files.push(IndexFile {
                full_path,
                relative_path: rel.clone(),
                language,
                kind,
                commit_sha: scope.commit_sha.clone(),
                worktree_id: scope.worktree_id.clone(),
            });
        }
        self.apply_incremental_file_plan(files, BTreeSet::new(), progress)
    }

    /// Remove overlay rows of `worktree_id` whose path is no longer in the delta (the file matches
    /// the base again), so the scope view falls back to the base row for them. Returns the count.
    fn prune_overlay_rows_not_in_delta(
        &self,
        worktree_id: &str,
        shadowing: &BTreeSet<PathBuf>,
    ) -> anyhow::Result<usize> {
        let existing: Vec<String> = {
            let conn = self.storage.connection();
            let mut stmt = conn.prepare(
                "SELECT path FROM main.files WHERE worktree_id = ?1 AND worktree_id != ''",
            )?;
            let rows = stmt.query_map([worktree_id], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut pruned = 0usize;
        for path in existing {
            if !shadowing.contains(Path::new(&path)) {
                self.remove_file_in_scope(Path::new(&path), "", worktree_id)?;
                pruned += 1;
            }
        }
        Ok(pruned)
    }
}
