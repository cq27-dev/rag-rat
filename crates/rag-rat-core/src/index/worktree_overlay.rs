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

use super::*;

/// Repo-relative paths whose linked-worktree state differs from the base scope, split into files to
/// index from the linked checkout (`readable`) and files to shadow with a tombstone (`tombstones` —
/// present in the base, absent from the linked working tree: branch/working-tree deletes and the
/// old side of a rename).
#[derive(Debug, Default)]
pub(crate) struct WorktreeOverlayDelta {
    pub(crate) readable: Vec<PathBuf>,
    pub(crate) tombstones: Vec<PathBuf>,
    /// Whether the linked working-tree status was read in full. When `false` (a status read error
    /// silently dropped the working-tree portion), the delta is PARTIAL and the caller must NOT
    /// prune — pruning against a partial `shadowing_paths` would delete valid overlay rows (#219
    /// review). The committed-diff portion is always complete (it errors hard).
    pub(crate) status_complete: bool,
}

impl WorktreeOverlayDelta {
    /// Every path the overlay is authoritative for (readable + tombstone); anything else falls
    /// through to the base scope. Used to prune overlay rows that no longer differ from the base.
    pub(crate) fn shadowing_paths(&self) -> BTreeSet<PathBuf> {
        self.readable.iter().chain(&self.tombstones).cloned().collect()
    }
}

#[derive(Debug, Default)]
pub struct WorktreeOverlayReport {
    /// The overlay scope's `worktree_id`; empty when `linked_path` was not a valid linked sibling
    /// (the pass was skipped).
    pub worktree_id: String,
    pub indexed: usize,
    pub tombstoned: usize,
    pub pruned: usize,
}

/// The `config.root` subdir prefix (relative to the repo's workdir) and the LINKED checkout's
/// equivalent of `config.root`. The subdir is derived from the BASE workdir (both worktrees share
/// the same layout); the linked root is the linked WORKDIR joined with that subdir — NOT the raw
/// `linked_path`, which may be a subdir of the checkout (e.g. `--worktree .` from `/wt/src`) or the
/// git dir (a hook). Falls back to the linked workdir / `linked_path` when the subdir can't be
/// derived. Shared by the delta computation (path rebasing) and the read step (source root) so the
/// two can't drift (#219 review).
fn linked_config_subdir_and_root(
    config: &Config,
    base_repo: &gix::Repository,
    linked_repo: &gix::Repository,
    linked_path: &Path,
) -> (PathBuf, PathBuf) {
    let linked_workdir =
        linked_repo.workdir().map(Path::to_path_buf).unwrap_or_else(|| linked_path.to_path_buf());
    // `config.root` is canonicalized (by `Config::load`'s `normalize_existing_dir`), but gix's
    // `workdir()` may not be; canonicalize the base workdir so the subdir prefix strips cleanly.
    let config_subdir = base_repo
        .workdir()
        .map(|base_workdir| {
            base_workdir.canonicalize().unwrap_or_else(|_| base_workdir.to_path_buf())
        })
        .and_then(|base_workdir| {
            config.root.strip_prefix(&base_workdir).ok().map(Path::to_path_buf)
        })
        .unwrap_or_default();
    let linked_config_root = linked_workdir.join(&config_subdir);
    (config_subdir, linked_config_root)
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
    // `config.root` may be a SUBDIR of the repo. Tree-diff and status entries are repo-relative
    // (e.g. `crate/src/lib.rs`), but `target_for_path` / the overlay path keys are config-root-
    // relative (e.g. `src/lib.rs`), and the readable files are read from the LINKED checkout's
    // equivalent of `config.root`. `config_subdir` is the prefix to strip; `linked_config_root` is
    // the source root overlay bytes are read from (#219 review).
    let (config_subdir, linked_config_root) =
        linked_config_subdir_and_root(config, &base_repo, &linked_repo, linked_path);

    let mut candidates: BTreeSet<PathBuf> = BTreeSet::new();

    // Resolve BOTH trees through `base_repo` so the cross-tree diff shares one object store (the
    // worktrees share the same `.git`). Each is OPTIONAL: an unborn HEAD (a fresh `git worktree add
    // --orphan`, zero commits) has no tree. Without tolerating that, `head_id()?` errored the whole
    // pass, so the watcher logged a failure for an orphan worktree every pass (#219 review). The
    // committed branch diff is computed only when both trees exist; the working-tree status below
    // still captures an orphan worktree's files.
    let base_tree = base_repo
        .head_id()
        .ok()
        .and_then(|id| id.object().ok())
        .and_then(|o| o.peel_to_tree().ok());
    let linked_tree = linked_repo
        .head_id()
        .ok()
        .and_then(|id| base_repo.find_object(id.detach()).ok())
        .and_then(|o| o.peel_to_tree().ok());
    if let (Some(base_tree), Some(linked_tree)) = (base_tree.as_ref(), linked_tree.as_ref()) {
        // Rename detection OFF: a rename becomes delete(old)+add(new), which the on-disk
        // categorization below resolves to tombstone(old) + readable(new).
        base_tree
            .changes()?
            .options(|opts| {
                opts.track_path().track_rewrites(None);
            })
            .for_each_to_obtain_tree(linked_tree, |change| {
                candidates.insert(change_location_path(&change));
                Ok::<_, std::convert::Infallible>(gix::object::tree::diff::Action::Continue(()))
            })?;
    }

    // Linked working-tree status (vs the linked HEAD): dirty edits, untracked files, deletes. Track
    // whether it was read in FULL — a silently-dropped status read yields a PARTIAL delta (missing
    // untracked / working-tree-deleted paths), and the caller must skip the prune on a partial
    // delta or it would delete valid overlay rows (#219 review).
    let mut status_complete = false;
    if let Ok(platform) = linked_repo.status(gix::progress::Discard)
        && let Ok(items) =
            platform.untracked_files(UntrackedFiles::Files).into_iter(None::<gix::bstr::BString>)
    {
        status_complete = fold_status_candidates(&mut candidates, items, |item| {
            PathBuf::from(item.location().to_str_lossy().as_ref())
        });
    }

    // Honor the worktree's `.gitignore` for files PRESENT in the worktree, so the overlay indexes
    // the same set the base walker would. Reuse the base's IgnoreMatcher (the `ignore` crate)
    // compiled for the linked checkout — using THIS, not a separate gitignore engine,
    // guarantees the overlay and base classify a path identically (no drift). Recompiled each
    // call, so a worktree `.gitignore` edit (which fires a pass) takes effect immediately.
    // Tombstones are NOT ignore-filtered: a branch-deleted file must shadow its base row
    // regardless of ignore rules.
    let ignore =
        ignore_rules::IgnoreMatcher::compile(&linked_config_root, &config.target_directories());
    let mut delta = WorktreeOverlayDelta { status_complete, ..Default::default() };
    for repo_rel in candidates {
        // Candidates are repo-relative; the overlay keys rows config-root-relative (matching the
        // base rows + `target_for_path`). A candidate OUTSIDE the config subdir has no base row to
        // shadow, so it can't strip the prefix → skip it.
        let Ok(rel) = repo_rel.strip_prefix(&config_subdir) else {
            continue;
        };
        let rel = rel.to_path_buf();
        // Only paths the base scope would index can be shadowed/overlaid.
        if target_for_path(config, &rel).is_none() {
            continue;
        }
        // Whether the base HEAD tree carries this path — i.e. there's a base row the overlay must
        // shadow when the branch's indexable view no longer contains the file.
        let shadows_base_file = || {
            base_tree
                .as_ref()
                .and_then(|t| t.lookup_entry_by_path(&repo_rel).ok().flatten())
                .is_some()
        };
        let absolute = linked_config_root.join(&rel);
        let indexable_in_worktree = absolute.is_file() && !ignore.is_ignored(&absolute, false);
        if indexable_in_worktree {
            delta.readable.push(rel);
        } else if shadows_base_file() {
            // The branch no longer presents an indexable file here (deleted, OR an IGNORED file now
            // sits at the path — the base walker wouldn't index either), but a base row exists.
            // Write a tombstone so the overlay shadows the base file; without it the
            // scope falls through to the base row and queries return a file the
            // branch's view dropped (#219 review).
            delta.tombstones.push(rel);
        }
    }
    Ok(delta)
}

/// Fold a worktree-status iterator of `Result<Item, E>` into `candidates`, returning whether the
/// read COMPLETED. A per-item error must NOT be flattened away: dropping a path while reporting
/// "complete" yields a partial candidate set the prune then treats as authoritative, deleting valid
/// overlay rows for the skipped paths. The first error stops the fold and returns `false`
/// (incomplete) so the caller skips the prune; an empty stream is complete (#219 review). Generic +
/// pure so the completeness decision is unit-testable without provoking a real gix status error.
fn fold_status_candidates<T, E>(
    candidates: &mut BTreeSet<PathBuf>,
    items: impl IntoIterator<Item = Result<T, E>>,
    locate: impl Fn(&T) -> PathBuf,
) -> bool {
    for item in items {
        match item {
            Ok(item) => {
                candidates.insert(locate(&item));
            },
            Err(_) => return false,
        }
    }
    true
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
    pub fn index_worktree_overlay<F>(
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
        // `delta.readable` is config-root-relative, so the bytes are read from the LINKED
        // checkout's equivalent of `config.root` — not the raw `linked_path` (which may be
        // a subdir of the checkout, e.g. `--worktree .` from `/wt/src`, or the git dir from
        // a hook) (#219 review).
        let base_repo = git_context::discover_repo(&config.root)?;
        let linked_repo = git_context::discover_repo(linked_path)?;
        let (_, source_root) =
            linked_config_subdir_and_root(config, &base_repo, &linked_repo, linked_path);
        let scope = FileScope::worktree(worktree_id.clone());
        // ONE transaction around the whole overlay update — incremental file replacement, tombstone
        // writes, the prune, AND the global logical-symbol/package/edge/FTS refresh — mirroring the
        // incremental pass (`index_incremental_with_progress`). Without it a concurrent reader can
        // observe partially replaced overlay rows or the globally cleared `logical_symbols` table
        // mid-rebuild, and an error midway leaves the overlay half-applied. BEGIN IMMEDIATE
        // acquires the write lock up front so a racing writer waits out busy_timeout
        // instead of failing the deferred read→write upgrade with SQLITE_BUSY; ROLLBACK on
        // any error (#219 review).
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<(usize, usize, usize)> {
            let indexed = self.index_explicit_paths_from_root(
                config,
                &source_root,
                &delta.readable,
                &scope,
                progress,
            )?;
            // Write a tombstone only when one isn't already present, so a re-run on a static
            // worktree writes nothing (idle-safety, like the readable sha-skip).
            let mut tombstoned = 0;
            for path in &delta.tombstones {
                let exists: bool = self.storage.connection().query_row(
                    "SELECT EXISTS(SELECT 1 FROM main.files WHERE path = ?1 AND commit_sha = '' \
                     AND worktree_id = ?2 AND kind = 'deleted')",
                    params![path_string(path), worktree_id],
                    |row| row.get(0),
                )?;
                if !exists {
                    self.write_tombstone_in_scope(path, &worktree_id)?;
                    tombstoned += 1;
                }
            }
            // Prune overlay rows that no longer differ from the base — but ONLY when the delta is
            // complete. A partial delta (the working-tree status read failed → `status_complete`
            // false) is missing untracked / working-tree-deleted paths, so pruning against its
            // `shadowing_paths` would delete valid overlay rows; skip the prune and let the
            // next complete pass reconcile (mirrors gc's empty-live-set guard) (#219 review).
            let pruned = if delta.status_complete {
                self.prune_overlay_rows_not_in_delta(&worktree_id, &delta.shadowing_paths())?
            } else {
                0
            };
            self.finalize_overlay_refresh(&source_root, &worktree_id, indexed, tombstoned, pruned)?;
            Ok((indexed, tombstoned, pruned))
        })();
        let (indexed, tombstoned, pruned) = match result {
            Ok(counts) => {
                self.storage.execute_batch("COMMIT")?;
                counts
            },
            Err(err) => {
                let _ = self.storage.execute_batch("ROLLBACK");
                return Err(err);
            },
        };

        Ok(WorktreeOverlayReport { worktree_id, indexed, tombstoned, pruned })
    }

    /// The post-write finalize tail of `index_worktree_overlay`, run INSIDE its transaction — ONLY
    /// when something changed, so an unchanged worktree refresh is a true no-op (the watcher
    /// refreshes overlays every pass; this keeps idle passes write-free and clear of the
    /// self-sustaining re-index loop):
    /// - rebuild_logical_symbols: symbol_lookup / graph nav resolve through `logical_symbols`, so a
    ///   NEWLY-ADDED overlay file's symbols are invisible until regrouped (a modified file's
    ///   unchanged symbols resolve via the base's logical rows — which is why only added files were
    ///   missing). This is the field-reported bug.
    /// - refresh_packages: write the overlay scope's `packages` rows from the LINKED checkout's
    ///   manifests BEFORE resolving, so the per-package import scope (#61) is correct for the
    ///   branch. The resolver's `load_package_roots_into_scope` reads `packages` at the active
    ///   `(base_sha, linked_worktree_id)` scope — the base rows live at `(base_sha, '')` and are
    ///   invisible to it — so without this the overlay resolves imports against an empty package
    ///   map (every file falls open to the global union → external-dep suppression downgraded /
    ///   wrong for a branch with a new or changed Cargo.toml or path-dep alias) (#219 review).
    /// - resolve_overlay_edges: the overlay inserted edges unresolved. Uses the OVERLAY-SCOPED
    ///   resolver (NOT the base `resolve_edges`): resolution targets span the full overlay view,
    ///   but only the worktree's OWN overlay source rows are re-resolved — a shared committed
    ///   (base) caller's `edges_data`, visible in the overlay view but owned by the base scope,
    ///   must not be rewritten or base `find_callers`/impact would corrupt until a base pass
    ///   resolved it back, flipping by whichever worktree refreshed last (#219 P1).
    /// - sync_fts: so semantic_search (BM25) sees the overlay's chunks.
    fn finalize_overlay_refresh(
        &self,
        source_root: &Path,
        worktree_id: &str,
        indexed: usize,
        tombstoned: usize,
        pruned: usize,
    ) -> anyhow::Result<()> {
        if indexed > 0 || tombstoned > 0 || pruned > 0 {
            self.rebuild_logical_symbols()?;
            self.refresh_packages(source_root)?;
            self.resolve_overlay_edges(worktree_id)?;
            self.sync_fts()?;
        }
        Ok(())
    }

    /// Index an EXPLICIT set of repo-relative `paths`, reading bytes from `source_root` (which may
    /// differ from `config.root` — e.g. a sibling linked worktree) but keying every row by the
    /// repo-relative logical path, with `scope`. The reusable primitive under
    /// `index_worktree_overlay` — it knows nothing about worktree deltas, just "index these
    /// paths from this root into this scope", applying the same target include/exclude policy
    /// as discovery and reusing the per-file prepare/insert pipeline
    /// (`apply_incremental_file_plan`).
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
        // Existing rows in this scope (path → sha) so an UNCHANGED file is skipped: re-running the
        // overlay on a static worktree then writes nothing, so the watcher can refresh overlays
        // every maintenance pass without churn — preserving the idle backstop (#63) and not
        // tripping the self-sustaining re-index loop.
        let existing = self.scope_file_shas(&scope.commit_sha, &scope.worktree_id)?;
        let mut files = Vec::new();
        for rel in paths {
            let full_path = source_root.join(rel);
            let Ok(bytes) = std::fs::read(&full_path) else {
                continue; // not a readable regular file
            };
            if existing.get(path_string(rel).as_str()) == Some(&hex_sha256(&bytes)) {
                continue; // unchanged since the last overlay index
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

    /// Existing file rows in a scope as `path → sha256` — for the idle-safe skip above.
    fn scope_file_shas(
        &self,
        commit_sha: &str,
        worktree_id: &str,
    ) -> anyhow::Result<HashMap<String, String>> {
        let conn = self.storage.connection();
        let mut stmt = conn.prepare(
            "SELECT path, sha256 FROM main.files WHERE commit_sha = ?1 AND worktree_id = ?2",
        )?;
        let rows = stmt.query_map(params![commit_sha, worktree_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>().map_err(Into::into)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_status_candidates_marks_complete_on_a_clean_read() {
        let mut candidates = BTreeSet::new();
        let items: Vec<Result<&str, ()>> = vec![Ok("src/a.rs"), Ok("src/b.rs")];
        let complete = fold_status_candidates(&mut candidates, items, |s| PathBuf::from(s));
        assert!(complete, "a clean status read is complete");
        assert_eq!(
            candidates,
            BTreeSet::from([PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]),
        );
    }

    #[test]
    fn fold_status_candidates_marks_incomplete_on_a_per_item_error() {
        // The bug the #219 review caught: `flatten()` dropped the erroring item but left the read
        // looking complete, so the prune treated a partial candidate set as authoritative and could
        // delete valid overlay rows. A per-item error must mark the delta INCOMPLETE.
        let mut candidates = BTreeSet::new();
        let items: Vec<Result<&str, ()>> = vec![Ok("src/a.rs"), Err(()), Ok("src/c.rs")];
        let complete = fold_status_candidates(&mut candidates, items, |s| PathBuf::from(s));
        assert!(
            !complete,
            "a per-item status error makes the delta incomplete → caller skips prune"
        );
        // Stops at the error (the trailing path after it is not authoritative either way).
        assert!(candidates.contains(Path::new("src/a.rs")));
        assert!(!candidates.contains(Path::new("src/c.rs")));
    }

    #[test]
    fn fold_status_candidates_empty_stream_is_complete() {
        let mut candidates = BTreeSet::new();
        let items: Vec<Result<&str, ()>> = vec![];
        assert!(fold_status_candidates(&mut candidates, items, |s| PathBuf::from(s)));
        assert!(candidates.is_empty());
    }
}
