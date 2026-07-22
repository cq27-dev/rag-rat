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

#[derive(Debug, Default)]
pub struct WorktreeOverlayReport {
    /// The overlay scope's `worktree_id`; empty when `linked_path` was not a valid linked sibling
    /// (the pass was skipped).
    pub worktree_id: String,
    pub indexed: usize,
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

/// Resolve the `(base_sha, worktree_id, source_root)` for a linked-worktree overlay, or `None` when
/// `linked_path` is not a valid linked sibling of `config.root`'s repo (its scope fell back to
/// base). Shared by [`IndexDatabase::index_worktree_overlay`] and
/// [`IndexDatabase::refresh_worktree_overlay_packages`] so both derive the SAME scope + source
/// root.
fn resolve_overlay_scope(
    config: &Config,
    linked_path: &Path,
) -> anyhow::Result<Option<(String, String, PathBuf)>> {
    let (base_sha, worktree_id) =
        git_context::resolve_worktree_scope(&config.root, Some(linked_path));
    if worktree_id == git_context::worktree_id_of(&config.root) {
        return Ok(None);
    }
    let base_repo = rag_rat_base::repo_discover::discover_repo(&config.root)?;
    let linked_repo = rag_rat_base::repo_discover::discover_repo(linked_path)?;
    let (_, source_root) =
        linked_config_subdir_and_root(config, &base_repo, &linked_repo, linked_path);
    Ok(Some((base_sha, worktree_id, source_root)))
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
    let base_repo = rag_rat_base::repo_discover::discover_repo(&config.root)?;
    let linked_repo = rag_rat_base::repo_discover::discover_repo(linked_path)?;
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
    // A `Cargo.toml` among the STATUS entries (a DIRTY manifest edit) must refresh the package/
    // import scope even though a manifest is not a target file (so it never reaches the delta's
    // readable set). Detect it DURING the fold — status is a one-shot iterator. `Cell` because
    // `fold_status_candidates` takes an `Fn` locator, not `FnMut`.
    let manifest_in_status = std::cell::Cell::new(false);
    if let Ok(platform) = linked_repo.status(gix::progress::Discard)
        && let Ok(items) =
            platform.untracked_files(UntrackedFiles::Files).into_iter(None::<gix::bstr::BString>)
    {
        status_complete = fold_status_candidates(&mut candidates, items, |item| {
            let path = PathBuf::from(item.location().to_str_lossy().as_ref());
            if path_is_manifest_under_subdir(&path, &config_subdir) {
                manifest_in_status.set(true);
            }
            path
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
        &config_subdir,
        &linked_config_root,
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

/// `repo_meta` key prefix for a linked worktree's overlay refresh basis (#577): one key per
/// `worktree_id`, value `"<base_sha>\n<linked_head_sha>"` — the (base HEAD, linked HEAD) pair the
/// overlay delta was last computed against. A scoped watcher pass skips a worktree not implicated
/// by events ONLY while this pair still matches; either head moving (a base commit re-basing every
/// overlay, a linked commit with no file event) forces the refresh. Kept per worktree in the
/// per-repo kv rather than a dedicated table: it is a marker, not queried relationally, and the
/// `watch_shutdown_reconcile_pending` marker set the pattern.
const WORKTREE_OVERLAY_BASIS_META_PREFIX: &str = "worktree_overlay_basis:";

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

impl IndexDatabase {
    /// The recorded refresh basis for `worktree_id`: `(base_sha, linked_head_sha)` at the last
    /// successful overlay refresh, or `None` when never refreshed (or written by a pre-#577
    /// build) — the caller then refreshes unconditionally.
    pub(crate) fn worktree_overlay_basis(
        &self,
        worktree_id: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        let key = format!("{WORKTREE_OVERLAY_BASIS_META_PREFIX}{worktree_id}");
        Ok(self.repo_meta(&key)?.and_then(|value| {
            let (base_sha, linked_head) = value.split_once('\n')?;
            Some((base_sha.to_string(), linked_head.to_string()))
        }))
    }

    /// Upsert the refresh basis after a successful overlay refresh. `set_repo_meta_if_changed` so
    /// an unchanged-basis `All` sweep stays write-free (#63 idle backstop).
    pub(crate) fn record_worktree_overlay_basis(
        &self,
        worktree_id: &str,
        base_sha: &str,
        linked_head_sha: &str,
    ) -> anyhow::Result<()> {
        let key = format!("{WORKTREE_OVERLAY_BASIS_META_PREFIX}{worktree_id}");
        self.set_repo_meta_if_changed(&key, &format!("{base_sha}\n{linked_head_sha}"))?;
        Ok(())
    }

    /// Drop the refresh-basis keys of worktrees outside `live_worktrees` (gc, alongside the
    /// overlay-row prune) so a removed checkout's marker doesn't accumulate forever. The rows are
    /// one-per-worktree, so the filtering runs in Rust over the prefix-selected keys.
    pub(crate) fn prune_worktree_overlay_basis_outside(
        &self,
        live_worktrees: &[String],
    ) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        let mut stmt = conn
            .prepare("SELECT key FROM repo_meta WHERE repo_id = ?1 AND key LIKE ?2 ESCAPE '\\'")?;
        // LIKE-escape the prefix's wildcard characters (it carries literal underscores).
        let escaped = WORKTREE_OVERLAY_BASIS_META_PREFIX
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let keys = stmt
            .query_map(params![self.active_repo_id, pattern], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for key in keys {
            let Some(worktree_id) = key.strip_prefix(WORKTREE_OVERLAY_BASIS_META_PREFIX) else {
                continue;
            };
            if !live_worktrees.iter().any(|live| live == worktree_id) {
                rag_rat_db::meta::delete_repo_meta(conn, &self.active_repo_id, &key)?;
            }
        }
        Ok(())
    }

    /// Drop `worktree_id`'s refresh basis. The watcher calls this when a refresh FAILED or was
    /// PARTIAL: neither HEAD may have moved (a dirty edit doesn't), so a previously recorded
    /// basis would keep matching and scoped passes would skip the stale overlay until an `All`
    /// pass (#577 review).
    pub(crate) fn clear_worktree_overlay_basis(&self, worktree_id: &str) -> anyhow::Result<()> {
        let key = format!("{WORKTREE_OVERLAY_BASIS_META_PREFIX}{worktree_id}");
        rag_rat_db::meta::delete_repo_meta(self.storage.connection(), &self.active_repo_id, &key)?;
        Ok(())
    }

    /// Index a linked worktree's branch/working-tree delta as overlay rows that shadow the base
    /// scope, and tombstone the files it removed (#219 stage 2). No-op (empty `worktree_id` in the
    /// report) when `linked_path` is not a valid linked sibling of `config.root`'s repo. Leaves the
    /// connection scope set to the overlay; callers re-`set_context` if they need another scope.
    ///
    /// The standalone shape ([`OverlayRefreshTail::STANDALONE`]): the repo-global logical-symbol
    /// rebuild runs inline, atomic with the overlay transaction, and no #577 basis is maintained.
    /// Callers refreshing SEVERAL worktrees in one pass use
    /// [`Self::index_worktree_overlay_with_tail`] instead, so the batch pays the rebuild once
    /// (#819).
    pub fn index_worktree_overlay<F>(
        &mut self,
        config: &Config,
        linked_path: &Path,
        progress: &mut F,
    ) -> anyhow::Result<WorktreeOverlayReport>
    where
        F: FnMut(IndexProgress),
    {
        self.index_worktree_overlay_with_tail(
            config,
            linked_path,
            OverlayRefreshTail::STANDALONE,
            progress,
        )
    }

    /// [`Self::index_worktree_overlay`] with caller-owned tail handling (#819/#824): the batch
    /// shape. `tail` decides whether the repo-global logical-symbol rebuild runs inline or is
    /// deferred to one [`Self::apply_pending_logical_rebuild`] per batch, and whether the
    /// worktree's #577 refresh basis is maintained inside this refresh's own transaction.
    pub fn index_worktree_overlay_with_tail<F>(
        &mut self,
        config: &Config,
        linked_path: &Path,
        tail: OverlayRefreshTail<'_>,
        progress: &mut F,
    ) -> anyhow::Result<WorktreeOverlayReport>
    where
        F: FnMut(IndexProgress),
    {
        // `source_root` is the LINKED checkout's equivalent of `config.root` — bytes are read from
        // there, not the raw `linked_path` (which may be a subdir of the checkout, e.g. `--worktree
        // .` from `/wt/src`, or the git dir from a hook) (#219 review).
        let Some((base_sha, worktree_id, source_root)) =
            resolve_overlay_scope(config, linked_path)?
        else {
            // Fell back to base → not a valid linked sibling; nothing to overlay.
            return Ok(WorktreeOverlayReport::default());
        };
        // Scope the connection to the overlay (base commit + linked worktree id) so context-
        // dependent steps (tombstones, FTS, edge resolution) operate in the linked scope.
        self.set_context(&base_sha, &worktree_id)?;

        let mut delta = compute_linked_worktree_delta(config, linked_path)?;
        // Fold in TARGET-IDENTITY drift: a branch config change that re-languages or drops a
        // byte-identical file is invisible to the content delta, but the overlay's (language, kind)
        // must still track the branch config, like discovery's staleness (#659 review). This also
        // covers the `index --paths <linked>/foo.rs` case for a clean re-languaged file without
        // threading the supplied paths — the scan sees every base-scope file. GATED on the branch
        // config's targets differing from the base's (fingerprint match → no file can re-language),
        // so the common no-divergent-config worktree does NOT pay an O(base-files) scan on every
        // overlay refresh (#577 event-scoping).
        if self.overlay_targets_may_drift(&config.targets)? {
            let (drift_readable, drift_tombstones) = self.overlay_target_config_reconcile(
                &base_sha,
                config,
                &source_root,
                &delta.shadowing_paths(),
            )?;
            delta.readable.extend(drift_readable);
            delta.tombstones.extend(drift_tombstones);
        }
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
                if !self.overlay_tombstone_exists(path, &worktree_id)? {
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
            self.finalize_overlay_refresh(
                &source_root,
                &worktree_id,
                OverlayChangeCounts { indexed, tombstoned, pruned },
                delta.manifest_changed,
                tail.logical_rebuild,
            )?;
            // #824: the basis write rides the SAME transaction as the rows it proves current —
            // previously a separate autocommit per worktree per pass (an extra WAL-dirtying
            // commit each). Un-gated on the counts: a COMPLETE no-change refresh must still
            // record its basis (that skip proof is the whole point of #577).
            self.apply_overlay_basis_tail(&worktree_id, delta.status_complete, tail.basis)?;
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

        Ok(WorktreeOverlayReport {
            worktree_id,
            indexed,
            tombstoned,
            pruned,
            status_complete: delta.status_complete,
        })
    }

    /// Index EXACTLY the supplied `paths` of a linked worktree as overlay rows — the PATH-SCOPED
    /// twin of [`Self::index_worktree_overlay`] (#679), so a linked-worktree `index --paths`
    /// (the edit hook) refreshes just those paths' overlay rows instead of the checkout's WHOLE
    /// base↔branch delta. Mirrors the base `IndexMode::Paths` exact-path semantics on the
    /// linked route: a single-file edit no longer pulls in unrelated in-flight changes
    /// elsewhere in the same worktree, and pays no full tree-diff / status walk.
    ///
    /// Each supplied path is categorized against the LINKED checkout + branch config: present +
    /// target-matching → readable (indexed with the branch identity; the identity-skip in
    /// [`Self::index_explicit_paths_from_root`] keeps an unchanged file write-free); absent, OR
    /// present but no longer targeted by the branch config, while the BASE scope still has a live
    /// row → tombstone (shadow that base row); nothing to shadow otherwise.
    ///
    /// Does NOT prune (a partial path set is not authoritative over the whole overlay — pruning
    /// would delete valid rows for the paths it didn't inspect) and reports `status_complete =
    /// false`, so the caller ([`crate::watch::reindex_paths`]) clears the overlay basis and the
    /// next full sweep reconciles anything else in the worktree. The supplied-manifest
    /// package-map refresh stays the caller's job (via `refresh_worktree_overlay_packages`),
    /// exactly as on the whole-delta route. No-op (empty `worktree_id`) when `linked_path` is
    /// not a valid linked sibling. Leaves the connection scoped to the overlay.
    ///
    /// `logical_rebuild` (#819): `Deferred` skips the repo-global logical-symbol rebuild and
    /// marks it pending, for callers batching several overlay refreshes — the batch then runs
    /// [`Self::apply_pending_logical_rebuild`] once. Basis maintenance stays caller-side here
    /// (this refresh is never complete, so there is never a pair to record).
    pub fn index_worktree_overlay_paths<F>(
        &mut self,
        config: &Config,
        linked_path: &Path,
        paths: &[PathBuf],
        logical_rebuild: OverlayLogicalRebuild,
        progress: &mut F,
    ) -> anyhow::Result<WorktreeOverlayReport>
    where
        F: FnMut(IndexProgress),
    {
        let Some((base_sha, worktree_id, source_root)) =
            resolve_overlay_scope(config, linked_path)?
        else {
            return Ok(WorktreeOverlayReport::default());
        };
        self.set_context(&base_sha, &worktree_id)?;
        // Classify each supplied path with the SAME symlink-safe, ignore-aware guards the base
        // `IndexMode::Paths` walker applies (#659), since a supplied path may be arbitrary (a
        // crafted `..`-escape, a symlink-crossing spelling, or an ignored file) — reuse the
        // shared primitives (`lexically_normalized_within_root` / `resolves_within_root` /
        // `path_crosses_symlink`) rather than a naive `is_file()`, so this route can't
        // drift from the base one. `ignore` is the LINKED checkout's matcher (a branch
        // `.gitignore` governs the overlay's indexable set), recompiled per call so a
        // branch ignore edit takes effect immediately.
        let canonical_source = source_root.canonicalize().unwrap_or_else(|_| source_root.clone());
        let ignore =
            ignore_rules::IgnoreMatcher::compile(&source_root, &config.target_directories());
        // Present + indexable = a regular, in-root, non-symlink-crossed, NON-ignored file the
        // branch config targets — the base walker's exact set. A closure so the same check
        // RE-VALIDATES a removal inside the transaction (below), where the write lock is
        // held.
        let is_present_indexable = |rel: &Path, full: &Path| {
            resolves_within_root(full, &canonical_source)
                && !path_crosses_symlink(&source_root, rel)
                && full.is_file()
                && !ignore.is_ignored(full, false)
                && target_for_path(config, rel).is_some()
        };
        let mut readable = Vec::new();
        let mut tombstones = Vec::new();
        let mut removal_candidates = Vec::new();
        for path in paths {
            // Rebase to the config-root-relative key (the spelling every overlay row + target match
            // uses), retrying against the CANONICAL source root for a symlinked spelling; then
            // reject a `..`-escape. A path not under the source root is dropped
            // (defensive).
            let raw = match path.strip_prefix(&source_root) {
                Ok(rel) => rel.to_path_buf(),
                Err(_) => {
                    let Some(rel) = canonicalize_nearest_ancestor(path).and_then(|canonical| {
                        canonical.strip_prefix(&canonical_source).ok().map(Path::to_path_buf)
                    }) else {
                        continue;
                    };
                    rel
                },
            };
            let Some(rel) = lexically_normalized_within_root(&raw) else { continue };
            let full = source_root.join(&rel);
            if is_present_indexable(&rel, &full) {
                readable.push(rel);
            } else if self.base_scope_has_path(&base_sha, &rel)? {
                // Non-indexable (delete / ignored-now / de-targeted / symlink-replaced) but the
                // base still has a row → shadow it with a tombstone (mirrors the whole-delta
                // overlay). Carry `full` so the write RE-VALIDATES under the write lock, like
                // removals.
                tombstones.push((rel, full));
            } else {
                // Non-indexable AND no base row to shadow: a BRANCH-ONLY file. If it was overlay-
                // indexed, its stale row must be REMOVED (the whole-delta prune does this; a
                // path-scoped pass skips the prune). Deferred to the transaction, where existence +
                // non-indexability are RE-VALIDATED under the write lock (#679 review).
                removal_candidates.push((rel, full));
            }
        }
        let scope = FileScope::worktree(worktree_id.clone());
        // ONE transaction (see `index_worktree_overlay` for the rationale): index the readable set,
        // write tombstones, then the gated logical-symbol/edge/FTS refresh. BEGIN IMMEDIATE up
        // front; ROLLBACK on any error.
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<(usize, usize, usize)> {
            let indexed = self.index_explicit_paths_from_root(
                config,
                &source_root,
                &readable,
                &scope,
                progress,
            )?;
            let mut tombstoned = 0;
            for (rel, full) in &tombstones {
                // RE-VALIDATE under the write lock (like removals): if the file was recreated /
                // became indexable after classification, do NOT write a tombstone that would hide
                // the now-valid content — `BEGIN IMMEDIATE` freezes DB writers, not
                // the filesystem. The next full sweep reconciles the recreated file
                // (#679 review).
                if !is_present_indexable(rel, full)
                    && !self.overlay_tombstone_exists(rel, &worktree_id)?
                {
                    self.write_tombstone_in_scope(rel, &worktree_id)?;
                    tombstoned += 1;
                }
            }
            // Targeted removal of stale branch-only overlay rows — the per-path equivalent of the
            // whole-delta prune this scoped pass skips. RE-VALIDATED here under the write lock
            // (BEGIN IMMEDIATE froze other writers): only remove a still-non-indexable path whose
            // overlay row still exists, so a concurrent heal that re-indexed the path (it
            // reappeared) between classification and now can't have its fresh row
            // deleted (#679 review).
            let mut pruned = 0;
            for (rel, full) in &removal_candidates {
                if !is_present_indexable(rel, full)
                    && self.overlay_source_row_exists(rel, &worktree_id)?
                {
                    self.remove_file_in_scope(rel, "", &worktree_id)?;
                    pruned += 1;
                }
            }
            // No global prune: a partial path set is not authoritative over the whole overlay. The
            // supplied-manifest package refresh is the caller's job, so `manifest_changed = false`.
            self.finalize_overlay_refresh(
                &source_root,
                &worktree_id,
                OverlayChangeCounts { indexed, tombstoned, pruned },
                false,
                logical_rebuild,
            )?;
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
        Ok(WorktreeOverlayReport {
            worktree_id,
            indexed,
            tombstoned,
            pruned,
            // A path-scoped pass never fully reconciles the overlay — signal incomplete so the
            // caller clears the basis and the next full sweep reconciles the rest
            // (#679).
            status_complete: false,
        })
    }

    /// Whether a live (non-deleted) OVERLAY source row for `path` exists in `worktree_id`'s scope —
    /// the gate for removing a now-non-indexable BRANCH-ONLY overlay row that has no base row to
    /// shadow (#679). Distinct from [`Self::base_scope_has_path`] (which probes the base scope).
    fn overlay_source_row_exists(&self, path: &Path, worktree_id: &str) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM main.files WHERE repo_id = ?1 AND path = ?2 AND \
             commit_sha = '' AND worktree_id = ?3 AND kind != 'deleted' AND generation = ?4)",
            params![self.active_repo_id, path_string(path), worktree_id, self.active_generation],
            |row| row.get(0),
        )?)
    }

    /// Whether a live-generation tombstone for `path` already exists in `worktree_id`'s overlay
    /// scope — the idle-safe guard before writing one, so a re-run on a static worktree writes
    /// nothing. A direct `main.files` probe with explicit `repo_id` (A3) + `generation` (A6)
    /// predicates: only a tombstone at THIS connection's live generation suppresses the write.
    fn overlay_tombstone_exists(&self, path: &Path, worktree_id: &str) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM main.files WHERE repo_id = ?1 AND path = ?2 AND \
             commit_sha = '' AND worktree_id = ?3 AND kind = 'deleted' AND generation = ?4)",
            params![self.active_repo_id, path_string(path), worktree_id, self.active_generation],
            |row| row.get(0),
        )?)
    }

    /// Whether the BASE scope has a live (non-deleted) row for `path` at `base_sha` — the gate for
    /// shadowing a base row with an overlay tombstone (there is nothing to shadow otherwise).
    fn base_scope_has_path(&self, base_sha: &str, path: &Path) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM main.files WHERE repo_id = ?1 AND path = ?2 AND \
             commit_sha = ?3 AND worktree_id = '' AND kind != 'deleted' AND generation = ?4)",
            params![self.active_repo_id, path_string(path), base_sha, self.active_generation],
            |row| row.get(0),
        )?)
    }

    /// Refresh JUST the linked overlay's package/import scope (and re-resolve its edges against the
    /// new map) — the `index --paths <linked>/Cargo.toml` entry point when the supplied manifest is
    /// CLEAN/committed. `index_worktree_overlay` derives its manifest signal from the WORKING-TREE
    /// STATUS (dirty-only, for idle-safety), so a supplied but clean manifest produces no indexed
    /// rows and would leave the package map stale — this honors the base `Paths` flow's
    /// supplied-manifest signal for the linked route (#659 review). Its own `BEGIN IMMEDIATE` (the
    /// caller is not mid-transaction) and idempotent, so re-running it after a dirty-status refresh
    /// is harmless. No-op when `linked_path` is not a valid linked sibling. Leaves the connection
    /// scoped to the overlay.
    pub fn refresh_worktree_overlay_packages(
        &mut self,
        config: &Config,
        linked_path: &Path,
    ) -> anyhow::Result<()> {
        let Some((base_sha, worktree_id, source_root)) =
            resolve_overlay_scope(config, linked_path)?
        else {
            return Ok(());
        };
        self.set_context(&base_sha, &worktree_id)?;
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<()> {
            self.refresh_packages(&source_root)?;
            self.resolve_overlay_edges(&worktree_id)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.storage.execute_batch("COMMIT")?;
                Ok(())
            },
            Err(err) => {
                let _ = self.storage.execute_batch("ROLLBACK");
                Err(err)
            },
        }
    }

    /// The post-write finalize tail of `index_worktree_overlay`, run INSIDE its transaction — ONLY
    /// when something changed, so an unchanged worktree refresh is a true no-op (the watcher
    /// refreshes overlays every pass; this keeps idle passes write-free and clear of the
    /// self-sustaining re-index loop):
    /// - rebuild_logical_symbols: symbol_lookup / graph nav resolve through `logical_symbols`, so a
    ///   NEWLY-ADDED overlay file's symbols are invisible until regrouped (a modified file's
    ///   unchanged symbols resolve via the base's logical rows — which is why only added files were
    ///   missing). This is the field-reported bug. `Deferred` (#819) replaces the rebuild with a
    ///   persisted pending marker in this same transaction; the batch caller runs ONE rebuild for
    ///   all its worktrees via `apply_pending_logical_rebuild`.
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
    ///
    /// A manifest-only change (`manifest_changed`, no source rows) refreshes JUST the package scope
    /// — the logical-symbol/edge/FTS steps depend on source-row changes, so they stay gated on the
    /// counts (#659 review).
    fn finalize_overlay_refresh(
        &self,
        source_root: &Path,
        worktree_id: &str,
        counts: OverlayChangeCounts,
        manifest_changed: bool,
        logical_rebuild: OverlayLogicalRebuild,
    ) -> anyhow::Result<()> {
        if counts.any_changed() {
            match logical_rebuild {
                OverlayLogicalRebuild::Inline => {
                    // Defer the STAMP: an overlay refresh re-parsed only the worktree's own
                    // files, so it must not stamp the logical-key version — the base scope's
                    // drift is still in the future (#493).
                    self.rebuild_logical_symbols(graph_index::KeyVersionStamp::Defer)?;
                },
                OverlayLogicalRebuild::Deferred => {
                    // Mark the repo-global rebuild pending IN THIS transaction (#819).
                    // Committed overlay rows without a follow-up rebuild would leave a newly
                    // added file's symbols unresolvable, and a later pass would idle-skip the
                    // then-unchanged rows — the persisted marker survives a crash between this
                    // commit and the batch tail, so `apply_pending_logical_rebuild` still runs.
                    // `if_changed`: the second changed worktree of a batch finds it already set.
                    self.set_repo_meta_if_changed(OVERLAY_LOGICAL_REBUILD_PENDING_META, "1")?;
                },
            }
            self.refresh_packages(source_root)?;
            self.resolve_overlay_edges(worktree_id)?;
            self.sync_fts()?;
        } else if manifest_changed {
            // A dirty `Cargo.toml` with no source-row change: the base flow's manifest signal
            // refreshes the package map even with zero indexed files, and the overlay must match so
            // the branch resolves imports against its own manifest. `manifest_changed` is
            // status-derived (self-clears on commit), so this does not rewrite `packages` on every
            // idle overlay pass (#659 review).
            self.refresh_packages(source_root)?;
            // Overlay EDGES resolve THROUGH the package/import scope, so a package-map change must
            // re-resolve them too — otherwise callers/callees keep returning targets resolved
            // against the OLD manifest until an unrelated source change triggers a resolve (#659
            // review).
            self.resolve_overlay_edges(worktree_id)?;
        }
        Ok(())
    }

    /// Apply the #577 basis leg of an overlay refresh's tail, INSIDE the refresh transaction
    /// (#824): record the caller's pair on a COMPLETE refresh; clear the worktree's recorded
    /// basis on a PARTIAL one (a dirty edit moves no HEAD, so a stale pair would keep matching
    /// and scoped passes would skip the stale overlay until an `All` sweep — #577 review); no-op
    /// when the caller maintains no basis. FAILED refreshes never reach this: the transaction
    /// rolls back, and the caller clears the basis outside it.
    pub(super) fn apply_overlay_basis_tail(
        &self,
        worktree_id: &str,
        status_complete: bool,
        basis: Option<OverlayBasisUpdate<'_>>,
    ) -> anyhow::Result<()> {
        let Some(basis) = basis else { return Ok(()) };
        if status_complete {
            self.record_worktree_overlay_basis(worktree_id, basis.base_sha, basis.linked_head_sha)
        } else {
            self.clear_worktree_overlay_basis(worktree_id)
        }
    }

    /// Run the batch-deferred repo-global logical-symbol rebuild if one is pending (#819) — the
    /// REQUIRED tail of any batch of [`OverlayLogicalRebuild::Deferred`] refreshes, in its own
    /// `BEGIN IMMEDIATE` (the caller must not hold an open transaction). One rebuild serves the
    /// whole batch: `logical_symbols` is repo-scoped but scope-independent, so per-worktree
    /// rebuilds are redundant — with K changed worktrees only the last one's output survives.
    /// This is also the mid-batch-error / crash backstop: the pending marker committed with each
    /// worktree's rows, so earlier worktrees' committed refreshes get their rebuild even when a
    /// later worktree failed, or when the process died between the overlay transactions and this
    /// tail (the next pass finds the marker and heals). Returns whether a rebuild ran;
    /// `Ok(false)` = nothing pending, write-free (the #63 idle backstop).
    pub fn apply_pending_logical_rebuild(&self) -> anyhow::Result<bool> {
        // Lockless fast path: the common idle pass never opens a write transaction at all.
        if self.repo_meta(OVERLAY_LOGICAL_REBUILD_PENDING_META)?.is_none() {
            return Ok(false);
        }
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<bool> {
            // RE-CHECK under the write transaction: a concurrent tail (another watcher/hook
            // process) may have rebuilt and cleared the marker between the read above and
            // BEGIN IMMEDIATE — proceeding blind would pay a second wholesale rebuild for
            // nothing.
            if self.repo_meta(OVERLAY_LOGICAL_REBUILD_PENDING_META)?.is_none() {
                return Ok(false);
            }
            // Defer the #493 stamp for the same reason each deferred refresh would have: the
            // batch re-parsed only worktree deltas, never the full corpus. The rebuild clears
            // the pending marker itself, in this same transaction — on failure both roll back,
            // so the obligation survives for the next pass to retry.
            self.rebuild_logical_symbols(graph_index::KeyVersionStamp::Defer)?;
            Ok(true)
        })();
        match result {
            Ok(ran) => {
                self.storage.execute_batch("COMMIT")?;
                Ok(ran)
            },
            Err(err) => {
                let _ = self.storage.execute_batch("ROLLBACK");
                Err(err)
            },
        }
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
        // Existing rows in this scope (path → identity) so an UNCHANGED file is skipped: re-running
        // the overlay on a static worktree then writes nothing, so the watcher can refresh overlays
        // every maintenance pass without churn — preserving the idle backstop (#63) and not
        // tripping the self-sustaining re-index loop. The identity is `(sha256, language, kind)`,
        // not sha alone: a branch config change that RE-LANGUAGES a byte-identical file
        // must still rewrite the overlay row, mirroring discovery / the base `Paths` flow's
        // staleness (#659).
        let existing = self.scope_file_identities(&scope.commit_sha, &scope.worktree_id)?;
        let mut files = Vec::new();
        for rel in paths {
            let full_path = source_root.join(rel);
            let Ok(bytes) = std::fs::read(&full_path) else {
                continue; // not a readable regular file
            };
            let Some((language, kind)) = target_for_path(config, rel) else {
                continue;
            };
            if existing.get(path_string(rel).as_str())
                == Some(&(
                    hex_sha256(&bytes),
                    language.as_str().to_string(),
                    kind.as_str().to_string(),
                ))
            {
                continue; // unchanged since the last overlay index (content AND target identity)
            }
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

    /// Existing file rows in a scope as `path → (sha256, language, kind)` — for the identity-aware
    /// idle-safe skip above. A direct `main.files` probe (bypasses the repo-scoped view), so it
    /// carries the `repo_id` predicate explicitly (A3): today's sole caller passes a non-empty
    /// (path-derived, globally unique) `worktree_id`, but this is the documented reusable primitive
    /// — a base-scope caller (`commit_sha`, `''`) would otherwise skip files on the strength of a
    /// fork sibling's sha.
    fn scope_file_identities(
        &self,
        commit_sha: &str,
        worktree_id: &str,
    ) -> anyhow::Result<HashMap<String, (String, String, String)>> {
        let conn = self.storage.connection();
        let mut stmt = conn.prepare(
            "SELECT path, sha256, language, kind FROM main.files
             WHERE repo_id = ?1 AND commit_sha = ?2 AND worktree_id = ?3",
        )?;
        let rows =
            stmt.query_map(params![self.active_repo_id, commit_sha, worktree_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?),
                ))
            })?;
        rows.collect::<Result<HashMap<_, _>, _>>().map_err(Into::into)
    }

    /// Config-aware overlay reconcile: extra `(readable, tombstone)` candidates for files whose
    /// overlay state under `config` (the BRANCH overlay config) differs from the base index for a
    /// reason the content-based delta (`compute_linked_worktree_delta`, tree-diff + status) cannot
    /// see — a branch config change that RE-LANGUAGES, newly TARGETS, or DROPS a byte-identical
    /// file. Mirrors discovery's `(language, kind)` staleness for the overlay (#659 review). Two
    /// directions, both EXCLUDING `covered` (paths the content delta already reconciles):
    ///  - walk the branch checkout's target files (the branch config over `source_root`): a file
    ///    the base index LACKS (newly-targetable) or whose stored base identity DIFFERS
    ///    (re-languaged) → readable, so the overlay row carries the branch identity; a file whose
    ///    base identity already matches shows through the base row and needs no overlay;
    ///  - base rows the branch config NO LONGER targets → tombstone, so the stale base row is
    ///    shadowed instead of showing through.
    ///
    /// Read-only unless there is REAL divergence; on a later pass the row already exists in the
    /// overlay scope, so the identity-aware skip in `index_explicit_paths_from_root` makes it
    /// write-free. The caller GATES this on the branch target fingerprint differing from the base's
    /// ([`IndexDatabase::overlay_targets_may_drift`]), so a no-divergent-config worktree never runs
    /// the walk (#577 event-scoping).
    fn overlay_target_config_reconcile(
        &self,
        base_sha: &str,
        config: &Config,
        source_root: &Path,
        covered: &BTreeSet<PathBuf>,
    ) -> anyhow::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        // Base-scope identity: path → (language, kind).
        let base_identity: HashMap<PathBuf, (String, String)> = {
            let conn = self.storage.connection();
            let mut stmt = conn.prepare(
                "SELECT path, language, kind FROM main.files
                 WHERE repo_id = ?1 AND commit_sha = ?2 AND worktree_id = '' AND generation = ?3
                   AND kind != 'deleted'",
            )?;
            stmt.query_map(params![self.active_repo_id, base_sha, self.active_generation], |row| {
                Ok((
                    PathBuf::from(row.get::<_, String>(0)?),
                    (row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        let mut readable = Vec::new();
        let mut visited = BTreeSet::new();
        // Direction 1 — every file the BRANCH config targets in the checkout. `collect_index_files`
        // walks the branch config's targets over `source_root` (the linked checkout), honoring its
        // `.gitignore`, exactly like the base walker.
        let walk_config = Config { root: source_root.to_path_buf(), ..config.clone() };
        for file in collect_index_files(&walk_config)? {
            let rel = file.relative_path;
            if covered.contains(&rel) {
                continue;
            }
            visited.insert(rel.clone());
            let branch_identity =
                (file.language.as_str().to_string(), file.kind.as_str().to_string());
            if base_identity.get(&rel) == Some(&branch_identity) {
                continue; // the base row already carries the branch identity → it shows through
            }
            readable.push(rel); // newly-targetable OR re-languaged → shadow with the branch parse
        }
        // Direction 2 — base rows the branch config NO LONGER targets (the walk never reached them,
        // and the branch config doesn't claim them) → shadow the stale base row. A base row still
        // targeted but absent in the checkout is a content deletion the delta covers.
        let mut tombstones = Vec::new();
        for rel in base_identity.keys() {
            if covered.contains(rel) || visited.contains(rel) {
                continue;
            }
            if target_for_path(config, rel).is_none() {
                tombstones.push(rel.clone());
            }
        }
        Ok((readable, tombstones))
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
            // Direct `main.files` probe → explicit `repo_id` predicate (A3), same class as the
            // heal probes in `incremental.rs`.
            let mut stmt = conn.prepare(
                "SELECT path FROM main.files
                 WHERE repo_id = ?1 AND worktree_id = ?2 AND worktree_id != ''",
            )?;
            let rows = stmt.query_map(params![self.active_repo_id, worktree_id], |row| {
                row.get::<_, String>(0)
            })?;
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
