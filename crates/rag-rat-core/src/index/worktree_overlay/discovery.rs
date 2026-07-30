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
    /// Whether a `Cargo.toml` appeared in the linked WORKING-TREE STATUS (a dirty manifest edit).
    /// A manifest is not a target file, so it never lands in `readable`/`tombstones` and
    /// produces no source-row change — but the branch's package/import scope must still
    /// refresh for it, so the overlay's finalize refreshes packages on this signal alone (#659
    /// review). Derived from STATUS (not the committed base↔branch diff) so it SELF-CLEARS on
    /// commit, matching the base flow's git-status manifest signal and keeping idle overlay
    /// passes write-free — a committed branch manifest persists in the tree-diff and would
    /// otherwise rewrite `packages` every pass.
    pub(crate) manifest_changed: bool,
}

impl WorktreeOverlayDelta {
    /// Every path the overlay is authoritative for (readable + tombstone); anything else falls
    /// through to the base scope. Used to prune overlay rows that no longer differ from the base.
    pub(crate) fn shadowing_paths(&self) -> BTreeSet<PathBuf> {
        self.readable.iter().chain(&self.tombstones).cloned().collect()
    }
}

/// Whether a refresh's [`WorktreeOverlayReport::changed_paths`] accounts for EVERY path whose
/// effective content it may have changed.
///
/// Distinct from [`WorktreeOverlayReport::status_complete`], which answers a different question —
/// "may this refresh's outcome be recorded as a skip-proof basis?" A path-scoped refresh is
/// deliberately `status_complete = false` (it never reconciles the whole overlay, so it must not
/// arm the #577 skip) while its path list is exactly what the caller asked for, hence `Complete`
/// here. Deriving one from the other would make every event-driven pass — the hot path — look
/// lossy and push consumers into a needless whole-checkout fallback.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ChangedPathsCoverage {
    /// Every path this refresh may have changed is listed.
    Complete,
    /// The list may be MISSING paths: the linked working-tree status read failed, so dirty,
    /// untracked, and working-tree-deleted files never became candidates. A consumer must treat
    /// the whole checkout as potentially stale rather than trusting the list. The default, so a
    /// skipped or defaulted report never reads as authoritative.
    #[default]
    Partial,
}

#[derive(Debug, Default)]
pub struct WorktreeOverlayReport {
    /// The overlay scope's `worktree_id`; empty when `linked_path` was not a valid linked sibling
    /// (the pass was skipped).
    pub worktree_id: String,
    /// The directory [`Self::changed_paths`] are relative to: this checkout's equivalent of
    /// `config.root`. NOT the checkout root — when `config.root` is a repo SUBDIR the two differ
    /// (`<linked>/crate` vs `<linked>`), and a consumer that joined at the checkout root would
    /// resolve `src/lib.rs` instead of `crate/src/lib.rs`. Empty when the pass was skipped.
    ///
    /// Returned rather than rebasing the paths onto the checkout root so they keep the one
    /// spelling the overlay rows and `target_for_path` already use — a consumer writing rows and
    /// a consumer opening files both stay in the same coordinate system.
    pub source_root: PathBuf,
    pub indexed: usize,
    /// The config-root-relative paths whose effective indexed content this refresh may have
    /// changed: files it WROTE (readable), files it SHADOWED (tombstoned), and files it
    /// UNSHADOWED (pruned — dropping an overlay row makes the base version visible to this
    /// checkout again, which changes what a query serves just as much as a write does).
    ///
    /// A SUPERSET of what changed, deliberately and soundly: a consumer that only needs "which of
    /// this checkout's files may be stale" (the live oracle's per-checkout worklist, #1010)
    /// filters it further anyway, and over-reporting costs a skipped candidate rather than a
    /// missed one. Read [`Self::coverage`] first — a `Partial` list is not a superset.
    pub changed_paths: Vec<PathBuf>,
    /// Whether [`Self::changed_paths`] is the complete set. See [`ChangedPathsCoverage`].
    pub coverage: ChangedPathsCoverage,
    pub tombstoned: usize,
    pub pruned: usize,
    /// Whether the working-tree status portion of the delta was read in FULL (see
    /// [`WorktreeOverlayDelta::status_complete`]). A partial refresh may have missed dirty/
    /// untracked/deleted paths, so the watcher must not record it as a skip-proof basis (#577):
    /// with the periodic sweep disabled, later scoped passes would skip the stale overlay
    /// indefinitely on the strength of matching heads.
    pub status_complete: bool,
}

/// The `config.root` subdir prefix (relative to the repo's workdir) and the LINKED checkout's
/// equivalent of `config.root`. The subdir is derived from the BASE workdir (both worktrees share
/// the same layout); the linked root is the linked WORKDIR joined with that subdir — NOT the raw
/// `linked_path`, which may be a subdir of the checkout (e.g. `--worktree .` from `/wt/src`) or the
/// git dir (a hook). Shared by the delta computation (path rebasing) and the read step (source
/// root) so the two can't drift (#219 review).
///
/// BOTH sides of the strip are canonicalized first. `Config::load` guarantees a canonical root, but
/// gix's `workdir()` makes no such promise, and a `Config` assembled in-process (a fixture, an
/// embedding caller) can carry any spelling of the root — a symlinked `$PWD`, macOS's `/var` for
/// `/private/var`, a Windows 8.3 name. Normalizing here is a no-op for a root that already holds
/// the invariant and repairs one that does not.
///
/// `ignore_rules::base_under_worktree` solves the same representation mismatch for the gitignore
/// base, but deliberately returns the ancestor in the ROOT's own spelling (its callers need a
/// textual prefix of the paths they strip). This one wants the CANONICAL subdir instead: the subdir
/// is re-joined onto the LINKED checkout's workdir, and git reports that checkout's tree-diff and
/// status paths in their real form — a symlinked segment of the base root has no meaning there.
///
/// ERRORS rather than falling back when the subdir still cannot be derived (#1027). The former
/// fallback — an empty subdir, the linked workdir as the source root — makes the whole refresh
/// scope the repo root instead of the config root: every candidate path keeps a `crate/` prefix
/// the targets don't match, the delta comes out empty, nothing is written, no client is
/// invalidated, and `index_worktree_overlay` still returns `Ok(())`. A caller cannot tell that
/// from a genuinely unchanged worktree, which is what made the original report expensive to
/// localize. There is no correct scope to fall back to here, so say so.
fn linked_config_subdir_and_root(
    config_root: &Path,
    base_repo: &gix::Repository,
    linked_repo: &gix::Repository,
    linked_path: &Path,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let linked_workdir =
        linked_repo.workdir().map(Path::to_path_buf).unwrap_or_else(|| linked_path.to_path_buf());
    let base_workdir = base_repo.workdir().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot scope a worktree overlay for {}: its repository has no working tree",
            config_root.display(),
        )
    })?;
    let base_workdir = canonical_or_raw(base_workdir);
    let config_subdir = canonical_or_raw(config_root)
        .strip_prefix(&base_workdir)
        .map_err(|_| {
            anyhow::anyhow!(
                "cannot scope a worktree overlay: the index root {} does not resolve to a path \
                 inside its repository's working tree {}",
                config_root.display(),
                base_workdir.display(),
            )
        })?
        .to_path_buf();
    let linked_config_root = linked_workdir.join(&config_subdir);
    Ok((config_subdir, linked_config_root))
}

/// `path` canonicalized, or `path` itself when it cannot be resolved (a vanished directory) — the
/// caller then fails on the strip rather than on the canonicalization.
fn canonical_or_raw(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn linked_source_root(
    config_root: &Path,
    linked_path: &Path,
) -> anyhow::Result<PathBuf> {
    let base_repo = rag_rat_base::repo_discover::discover_repo(config_root)?;
    let linked_repo = rag_rat_base::repo_discover::discover_repo(linked_path)?;
    Ok(linked_config_subdir_and_root(config_root, &base_repo, &linked_repo, linked_path)?.1)
}

/// A linked-worktree overlay's resolved identity plus the opened repositories it was resolved
/// against. The repo handles are part of the resolution on purpose (#825): the delta computation
/// used to re-discover both repos after this had already opened them — four `discover_repo`
/// walks per refresh instead of two — so the handles are threaded through instead.
pub(crate) struct ResolvedOverlayScope {
    pub(crate) base_sha: String,
    pub(crate) worktree_id: String,
    /// `config.root`'s prefix relative to the repo workdir (both checkouts share the layout).
    pub(crate) config_subdir: PathBuf,
    /// The LINKED checkout's equivalent of `config.root` — overlay bytes are read from here.
    pub(crate) source_root: PathBuf,
    pub(crate) base_repo: gix::Repository,
    pub(crate) linked_repo: gix::Repository,
}

/// Resolve a linked-worktree overlay's scope, or `None` when `linked_path` is not a valid linked
/// sibling of `config.root`'s repo (its scope fell back to base). Shared by
/// [`IndexDatabase::index_worktree_overlay`] and
/// [`IndexDatabase::refresh_worktree_overlay_packages`] so both derive the SAME scope + source
/// root.
pub(super) fn resolve_overlay_scope(
    config: &Config,
    linked_path: &Path,
) -> anyhow::Result<Option<ResolvedOverlayScope>> {
    let (base_sha, worktree_id) =
        git_context::resolve_worktree_scope(&config.root, Some(linked_path));
    if worktree_id == git_context::worktree_id_of(&config.root) {
        return Ok(None);
    }
    let base_repo = rag_rat_base::repo_discover::discover_repo(&config.root)?;
    let linked_repo = rag_rat_base::repo_discover::discover_repo(linked_path)?;
    let (config_subdir, source_root) =
        linked_config_subdir_and_root(&config.root, &base_repo, &linked_repo, linked_path)?;
    Ok(Some(ResolvedOverlayScope {
        base_sha,
        worktree_id,
        config_subdir,
        source_root,
        base_repo,
        linked_repo,
    }))
}

/// How an overlay refresh sources the COMMITTED half of its candidate set (#825) — the base↔linked
/// tree diff, or (when provably identical) the recorded outcome of the last complete refresh.
pub(crate) enum CommittedDeltaSource {
    /// Walk the base HEAD tree ↔ linked HEAD tree diff. The default: a head moved, or there is no
    /// complete-refresh proof to reuse.
    TreeDiff,
    /// Both HEADs still equal the recorded #577 basis: the committed diff is a pure function of
    /// that pair, so its candidate outcome is already materialized as the worktree's current
    /// overlay rows — source rows AND tombstones, config-root-relative
    /// ([`IndexDatabase::list_overlay_shadowed_paths`]). Seed those instead of paying the tree
    /// walk; the working-tree status walk still runs, since dirty edits are the only thing that
    /// can differ while both heads hold still.
    ///
    /// Accepted (self-healing) divergence from [`Self::TreeDiff`]: a previously-dirty file
    /// reverted to content the base already has keeps its (now content-identical, query-invisible)
    /// overlay row until the next tree-diff refresh prunes it — the row seed can't distinguish
    /// "row because committed diff" from "row because then-dirty".
    UnchangedSinceBasis { shadowed_paths: Vec<PathBuf> },
}

/// Compute the overlay delta of `scope`'s linked worktree against the base scope. Candidate paths
/// = the committed branch diff (per `committed` — the tree diff, or the recorded rows when the
/// heads are unchanged, #825) UNION the linked worktree's working-tree status (dirty + untracked +
/// deleted). Each candidate's FINAL category is decided by its on-disk state in the LINKED
/// checkout — present → read it (readable); absent but present in the base tree → tombstone —
/// which correctly merges committed and working-tree changes (and maps a rename to delete-old +
/// add-new). Only target-matching paths are kept: the base wouldn't index the rest, so there is
/// nothing to shadow.
pub(crate) fn compute_linked_worktree_delta(
    config: &Config,
    scope: &ResolvedOverlayScope,
    committed: CommittedDeltaSource,
) -> anyhow::Result<WorktreeOverlayDelta> {
    // `config.root` may be a SUBDIR of the repo. Tree-diff and status entries are repo-relative
    // (e.g. `crate/src/lib.rs`), but `target_for_path` / the overlay path keys are config-root-
    // relative (e.g. `src/lib.rs`), and the readable files are read from the LINKED checkout's
    // equivalent of `config.root`. `config_subdir` is the prefix to strip; `linked_config_root` is
    // the source root overlay bytes are read from (#219 review). Both were derived when the scope
    // was resolved (the same repos this delta reads — #825 threads the opened handles through
    // instead of re-discovering them here).
    let config_subdir = &scope.config_subdir;
    let linked_config_root = &scope.source_root;

    let mut candidates: BTreeSet<PathBuf> = BTreeSet::new();

    // Linked working-tree status (vs the linked HEAD): dirty edits, untracked files, deletes —
    // read FIRST so the committed-source decision below can see a dirty `.gitignore` edit. Track
    // whether it was read in FULL — a silently-dropped status read yields a PARTIAL delta (missing
    // untracked / working-tree-deleted paths), and the caller must skip the prune on a partial
    // delta or it would delete valid overlay rows (#219 review).
    let mut status_complete = false;
    // A `Cargo.toml` among the STATUS entries (a DIRTY manifest edit) must refresh the package/
    // import scope even though a manifest is not a target file (so it never reaches the delta's
    // readable set). Detect it DURING the fold — status is a one-shot iterator. `Cell` because
    // `fold_status_candidates` takes an `Fn` locator, not `FnMut`.
    let manifest_in_status = std::cell::Cell::new(false);
    if let Ok(platform) = scope.linked_repo.status(gix::progress::Discard)
        && let Ok(items) =
            platform.untracked_files(UntrackedFiles::Files).into_iter(None::<gix::bstr::BString>)
    {
        status_complete = fold_status_candidates(&mut candidates, items, |item| {
            let path = PathBuf::from(item.location().to_str_lossy().as_ref());
            if path_is_manifest_under_subdir(&path, config_subdir) {
                manifest_in_status.set(true);
            }
            path
        });
    }

    // The base tree is needed by BOTH committed sources: `shadows_base_file()` below and the
    // ignore-flip expansion read it. OPTIONAL, like the linked tree: an unborn HEAD (a fresh
    // `git worktree add --orphan`, zero commits) has no tree. Without tolerating that,
    // `head_id()?` errored the whole pass, so the watcher logged a failure for an orphan worktree
    // every pass (#219 review).
    let base_tree = scope
        .base_repo
        .head_id()
        .ok()
        .and_then(|id| id.object().ok())
        .and_then(|o| o.peel_to_tree().ok());

    // A DIRTY `.gitignore` edit forces the full diff even when the heads are unchanged: an
    // ignore flip can make a committed branch-only file indexable for the first time — such a
    // file produced NO row on the last refresh (not indexable then) and is not in status (it is
    // tracked and clean), so the row seed provably cannot surface it. Ignore edits are rare;
    // paying the tree walk for them keeps the skip equivalence exact. (The caller already forces
    // the full diff for the analogous branch-config/target-drift case.)
    let dirty_ignore_edit =
        candidates.iter().any(|path| path.file_name() == Some(std::ffi::OsStr::new(".gitignore")));
    let committed = match committed {
        CommittedDeltaSource::UnchangedSinceBasis { .. } if dirty_ignore_edit =>
            CommittedDeltaSource::TreeDiff,
        other => other,
    };
    match committed {
        CommittedDeltaSource::TreeDiff => {
            // Resolve the linked tree through `base_repo` so the cross-tree diff shares one
            // object store (the worktrees share the same `.git`). The committed branch diff is
            // computed only when both trees exist; the working-tree status above still captures
            // an orphan worktree's files.
            let linked_tree = scope
                .linked_repo
                .head_id()
                .ok()
                .and_then(|id| scope.base_repo.find_object(id.detach()).ok())
                .and_then(|o| o.peel_to_tree().ok());
            if let (Some(base_tree), Some(linked_tree)) = (base_tree.as_ref(), linked_tree.as_ref())
            {
                // Rename detection OFF: a rename becomes delete(old)+add(new), which the on-disk
                // categorization below resolves to tombstone(old) + readable(new).
                base_tree
                    .changes()?
                    .options(|opts| {
                        opts.track_path().track_rewrites(None);
                    })
                    .for_each_to_obtain_tree(linked_tree, |change| {
                        candidates.insert(change_location_path(&change));
                        Ok::<_, std::convert::Infallible>(
                            gix::object::tree::diff::Action::Continue(()),
                        )
                    })?;
            }
        },
        CommittedDeltaSource::UnchangedSinceBasis { shadowed_paths } => {
            // Overlay rows are keyed config-root-relative; candidates are repo-relative until the
            // categorization loop strips the subdir back off.
            candidates.extend(shadowed_paths.iter().map(|rel| config_subdir.join(rel)));
        },
    }

    // Honor the worktree's `.gitignore` for files PRESENT in the worktree, so the overlay indexes
    // the same set the base walker would. Reuse the base's IgnoreMatcher (the `ignore` crate)
    // compiled for the linked checkout — using THIS, not a separate gitignore engine,
    // guarantees the overlay and base classify a path identically (no drift). Recompiled each
    // call, so a worktree `.gitignore` edit (which fires a pass) takes effect immediately.
    // Tombstones are NOT ignore-filtered: a branch-deleted file must shadow its base row
    // regardless of ignore rules.
    let ignore =
        ignore_rules::IgnoreMatcher::compile(linked_config_root, &config.target_directories());

    // Ignore-ONLY change expansion. When the branch's only change under a directory is a
    // `.gitignore` edit, the tree-diff/status candidates contain just `.gitignore` itself — the
    // UNCHANGED target files whose indexability the new rule FLIPS are never visited, so a base
    // file the rule now ignores keeps falling through to its (stale-visible) base row. For each
    // changed `.gitignore`, enumerate the BASE tree's target files under its directory and add
    // those now NON-indexable in the linked checkout as candidates — the loop below tombstones
    // them. Still- indexable base files are NOT added (they'd duplicate the shared base row as
    // overlay rows; the overlay only carries DIFFERENCES). The unignore direction (a
    // now-readable file) is already covered: an unignored untracked file reappears in the
    // status read on this same pass, and an unignored tracked file is in the committed diff or
    // status when its content differs (#219 review).
    expand_candidates_for_ignore_only_flips(
        &mut candidates,
        base_tree.as_ref(),
        config_subdir,
        linked_config_root,
        &ignore,
        config,
    );

    let mut delta = WorktreeOverlayDelta {
        status_complete,
        manifest_changed: manifest_in_status.get(),
        ..Default::default()
    };
    for repo_rel in candidates {
        // Candidates are repo-relative; the overlay keys rows config-root-relative (matching the
        // base rows + `target_for_path`). A candidate OUTSIDE the config subdir has no base row to
        // shadow, so it can't strip the prefix → skip it.
        let Ok(rel) = repo_rel.strip_prefix(config_subdir) else {
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

/// Add the base-tree target files whose indexability an ignore-ONLY change flipped to NON-indexable
/// (so they need a tombstone) to `candidates`. Bounded to the directory subtree of each changed
/// `.gitignore` already in `candidates` — a `.gitignore` rule only affects its own directory and
/// below. No-op when no `.gitignore` is among the candidates, or there is no base tree (orphan
/// linked HEAD has no base files to shadow). Adds REPO-relative paths, matching the candidate set.
fn expand_candidates_for_ignore_only_flips(
    candidates: &mut BTreeSet<PathBuf>,
    base_tree: Option<&gix::Tree<'_>>,
    config_subdir: &Path,
    linked_config_root: &Path,
    ignore: &ignore_rules::IgnoreMatcher,
    config: &Config,
) {
    let Some(base_tree) = base_tree else {
        return;
    };
    // The directory subtrees affected by an ignore change: the parent dir of each changed
    // `.gitignore`, REPO-relative. A repo-root `.gitignore` (no parent) affects the whole tree.
    let ignore_dirs: Vec<PathBuf> = candidates
        .iter()
        .filter(|p| p.file_name().is_some_and(|name| name == ".gitignore"))
        .map(|p| p.parent().map(Path::to_path_buf).unwrap_or_default())
        .collect();
    if ignore_dirs.is_empty() {
        return;
    }
    // Enumerate every base-tree file once, keep those under an affected subtree that the linked
    // checkout no longer indexes, and add them so the delta loop tombstones them.
    for repo_rel in base_tree_files(base_tree) {
        if !ignore_dirs.iter().any(|dir| repo_rel.starts_with(dir)) {
            continue;
        }
        let Ok(rel) = repo_rel.strip_prefix(config_subdir) else {
            continue; // outside the config subdir — no base row to shadow
        };
        if target_for_path(config, rel).is_none() {
            continue;
        }
        let absolute = linked_config_root.join(rel);
        let indexable = absolute.is_file() && !ignore.is_ignored(&absolute, false);
        if !indexable {
            // Newly ignored (or absent) under the changed rule — shadow the base row. The
            // categorization loop re-confirms `shadows_base_file()` before tombstoning.
            candidates.insert(repo_rel);
        }
    }
}

/// Every file path in `tree`, REPO-relative, via a diff against the empty tree (all entries appear
/// as additions) — reusing the same change-walk machinery as the base↔linked diff rather than a
/// separate recursion. Errors collapse to an empty set: the ignore-flip expansion is best-effort
/// recall, never a hard failure of the overlay pass.
fn base_tree_files(tree: &gix::Tree<'_>) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    let empty = tree.repo.empty_tree();
    let walked = (|| -> anyhow::Result<()> {
        empty
            .changes()?
            .options(|opts| {
                opts.track_path().track_rewrites(None);
            })
            .for_each_to_obtain_tree(tree, |change| {
                files.insert(change_location_path(&change));
                Ok::<_, std::convert::Infallible>(gix::object::tree::diff::Action::Continue(()))
            })?;
        Ok(())
    })();
    if walked.is_ok() { files } else { BTreeSet::new() }
}

/// Fold a worktree-status iterator of `Result<Item, E>` into `candidates`, returning whether the
/// read COMPLETED. A per-item error must NOT be flattened away: dropping a path while reporting
/// "complete" yields a partial candidate set the prune then treats as authoritative, deleting valid
/// overlay rows for the skipped paths. The first error stops the fold and returns `false`
/// (incomplete) so the caller skips the prune; an empty stream is complete (#219 review). Generic +
/// pure so the completeness decision is unit-testable without provoking a real gix status error.
pub(super) fn fold_status_candidates<T, E>(
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

/// Whether `repo_rel` (a repo-relative status path) is a `Cargo.toml` under `config_subdir` — i.e.
/// a manifest whose change should refresh THIS config's package map. A manifest outside the config
/// subdir belongs to a different part of the repo (when `config.root` is a subdir) and is not this
/// overlay's concern; `refresh_packages` scans manifests under the config's source root only.
fn path_is_manifest_under_subdir(repo_rel: &Path, config_subdir: &Path) -> bool {
    repo_rel
        .strip_prefix(config_subdir)
        .is_ok_and(|rel| rel.file_name() == Some(std::ffi::OsStr::new("Cargo.toml")))
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
