use rag_rat_base::paths::path_string;

use super::*;

/// Walk the commit history reachable from `head` with gix (gitoxide), producing the commit records
/// and their per-file changes in a SINGLE streaming pass — no `git log` subprocess and no
/// full-history stdout buffer, so memory stays bounded on deep-history repos (#212). Best-effort: a
/// repo gix can't open, or a commit it can't read, yields fewer rows rather than failing the whole
/// index, but marks the result incomplete so it cannot become an append base.
pub(super) fn read_history(root: &Path, worktree_root: &Path, head: &str) -> ReadGitHistory {
    let mut commits = Vec::new();
    let mut changes = Vec::new();
    let complete = read_history_inner(root, worktree_root, head, None, &mut commits, &mut changes)
        .unwrap_or(false);
    ReadGitHistory { commits, changes, complete }
}

pub(super) fn read_history_excluding(
    root: &Path,
    worktree_root: &Path,
    head: &str,
    hidden_head: &str,
) -> anyhow::Result<ReadGitHistory> {
    let mut commits = Vec::new();
    let mut changes = Vec::new();
    let complete = read_history_inner(
        root,
        worktree_root,
        head,
        Some(hidden_head),
        &mut commits,
        &mut changes,
    )?;
    Ok(ReadGitHistory { commits, changes, complete })
}

fn read_history_inner(
    root: &Path,
    worktree_root: &Path,
    head: &str,
    hidden_head: Option<&str>,
    commits: &mut Vec<CommitRecord>,
    changes: &mut Vec<FileChange>,
) -> anyhow::Result<bool> {
    let mut repo = rag_rat_base::repo_discover::discover_repo(root)?;
    // The by-commit-time walk + per-commit tree diffs look up each commit/tree more than once; an
    // object cache avoids repeated zlib inflation (gitoxide's own recommendation for these passes).
    repo.object_cache_size_if_unset(16 * 1024 * 1024);
    let head_id = gix::ObjectId::from_hex(head.as_bytes())?;
    // A blob-diff resource cache for per-file line counts (`change.diff`), reused across commits
    // and cleared each commit to bound its growth.
    let mut diff_cache = repo.diff_resource_cache_for_tree_diff()?;
    // When `root` is a SUBDIRECTORY of the worktree, scope history to commits/paths under that
    // subtree — what the old `git log <head> -- .` (run from `root`) covered. `None` at the
    // worktree root keeps everything (#213 review).
    let scope = scope_prefix(root, worktree_root);
    let walk = repo
        .rev_walk([head_id])
        // Newest-first, matching `git log`'s default reverse-chronological order.
        .sorting(Sorting::ByCommitTime(gix::traverse::commit::simple::CommitTimeOrder::NewestFirst));
    let walk = match hidden_head {
        Some(hidden_head) => {
            let hidden_id = gix::ObjectId::from_hex(hidden_head.as_bytes())?;
            walk.with_hidden([hidden_id])
        },
        None => walk,
    }
    .all()?;
    for info in walk {
        // A walk error (e.g. traversing past a shallow clone's boundary into absent objects) ends
        // the walk with what we have, rather than discarding the whole history (#213 review).
        let Ok(info) = info else { return Ok(false) };
        let Ok(commit) = repo.find_commit(info.id) else { return Ok(false) };
        let author = commit.author()?;
        let committer = commit.committer()?;
        let body = commit.message_raw_sloppy().to_string();
        let record = CommitRecord {
            hash: info.id.to_hex().to_string(),
            author_name: author.name.to_string(),
            author_email: author.email.to_string(),
            authored_at_s: author.seconds(),
            committed_at_s: committer.seconds(),
            subject: commit.message()?.summary().to_string(),
            // The full raw message (git `%B`), so commit FTS covers the body, not just the subject.
            body: body.trim().to_string(),
        };
        let hash = record.hash.clone();
        let parent_ids: Vec<_> = commit.parent_ids().collect();
        let new_tree = commit.tree()?;
        // First parent's tree + whether its object is PRESENT. A missing object (shallow-clone
        // boundary) → empty tree, treated as a ROOT: it records the full diff vs the empty tree
        // (matching `git log --numstat` on a shallow clone), so even a shallow MERGE tip records
        // its files instead of looking like a zero-change merge (#213 review).
        let (parent_tree, parent_present) = match parent_ids.first() {
            Some(parent) =>
                match repo.find_commit(parent.detach()).ok().and_then(|p| p.tree().ok()) {
                    Some(tree) => (tree, true),
                    None => (repo.empty_tree(), false),
                },
            None => (repo.empty_tree(), false),
        };
        // A real MERGE (>1 parent, first parent present) records NO per-file changes — `git log
        // --numstat` has no `--diff-merges` — but its first-parent diff still decides whether the
        // commit is IN SCOPE (a merge that didn't touch `root` is omitted, like `git log -- .` /
        // `-- <subtree>`). A shallow boundary (parent absent) is a root, so it DOES record its
        // diff.
        let is_merge_with_parent = parent_ids.len() > 1 && parent_present;
        diff_cache.clear_resource_cache();
        let mut commit_changes = Vec::new();
        parent_tree
            .changes()?
            // Full paths, AND rename detection ON (gix default is off; `git log --numstat` has it ON
            // unless `--no-renames`): a `git mv` becomes a single Rewrite at the destination rather
            // than a spurious delete+add that corrupts per-path churn (#213 review).
            .options(|opts| {
                opts.track_path().track_rewrites(Some(gix::diff::Rewrites::default()));
            })
            .for_each_to_obtain_tree(&new_tree, |change| {
                push_file_change(&change, &hash, scope.as_deref(), &mut diff_cache, &mut commit_changes);
                Ok::<_, std::convert::Infallible>(Action::Continue(()))
            })?;
        // No in-scope changes vs the first parent → not part of this index's history: `git log --
        // .` / `-- <subtree>` history simplification lists NEITHER empty commits (even at
        // the repo root) NOR merges/commits that didn't touch the scope (#213 review). (A
        // root/shallow-boundary commit diffs against the empty tree, so it has changes and
        // is kept; a mode-only change also counts.)
        if commit_changes.is_empty() {
            continue;
        }
        commits.push(record);
        // A real merge's first-parent diff was used ONLY for the scope decision above — do NOT
        // record it (numstat has no merge diff). Non-merges and shallow-boundary roots
        // record their changes.
        if !is_merge_with_parent {
            changes.append(&mut commit_changes);
        }
    }
    Ok(true)
}

/// Record one tree-diff change as a `FileChange` — any leaf path change (regular blob, symlink,
/// submodule gitlink, OR a rename/copy), skipping only directory (tree) entries. Symlinks and
/// gitlinks are included because `git log --numstat` records them as changed paths, and renames are
/// recorded once at the destination (rename detection is on) (#213 review).
fn push_file_change(
    change: &Change<'_, '_, '_>,
    hash: &str,
    scope: Option<&str>,
    diff_cache: &mut gix::diff::blob::Platform,
    out: &mut Vec<FileChange>,
) {
    // Keep any leaf path change; drop only TREE entries (directory nodes the diff may emit
    // alongside their leaves). A rename/copy is recorded once at the DESTINATION (the old
    // numstat parser normalized `old => new` to the destination), with counts from the
    // rewrite's content diff (`None` for a pure 100%-similar rename) (#213 review).
    // Each arm yields (change_kind, ROOT-RELATIVE path, counts) or returns. `scope_relative` maps a
    // worktree-root-relative location to the index-root-relative path (or `None` = outside `root`).
    let (change_kind, path, additions, deletions) = match change {
        Change::Addition { location, entry_mode, .. } if !entry_mode.is_tree() => {
            let Some(path) = scope_relative(scope, &location.to_string()) else { return };
            let (additions, deletions) = blob_line_counts(change, diff_cache);
            ("added", path, additions, deletions)
        },
        Change::Deletion { location, entry_mode, .. } if !entry_mode.is_tree() => {
            let Some(path) = scope_relative(scope, &location.to_string()) else { return };
            let (additions, deletions) = blob_line_counts(change, diff_cache);
            ("deleted", path, additions, deletions)
        },
        Change::Modification { location, entry_mode, .. } if !entry_mode.is_tree() => {
            let Some(path) = scope_relative(scope, &location.to_string()) else { return };
            let (additions, deletions) = blob_line_counts(change, diff_cache);
            ("modified", path, additions, deletions)
        },
        Change::Rewrite { location, source_location, entry_mode, diff, copy, .. }
            if !entry_mode.is_tree() =>
        {
            // A rename/copy can cross the index-root boundary in a subtree index, so filter EACH
            // side by scope: both inside → one entry at the destination (a rename
            // within the subtree); source-only inside → the file LEFT the subtree,
            // recorded as a deletion; dest-only inside → it ENTERED the subtree,
            // recorded as an addition. Matches what `git log --numstat -- <subtree>`
            // reported for a boundary-crossing move (#213 review).
            let source = scope_relative(scope, &source_location.to_string());
            let destination = scope_relative(scope, &location.to_string());
            let (additions, deletions) = diff.as_ref().map_or((None, None), |stats| {
                (Some(i64::from(stats.insertions)), Some(i64::from(stats.removals)))
            });
            match (source, destination) {
                (Some(_), Some(path)) =>
                    (if *copy { "copied" } else { "renamed" }, path, additions, deletions),
                // Crossing the boundary: counts are unknown (the rewrite diff is the content delta,
                // not the full add/delete) — the path + kind are what path history needs.
                (None, Some(path)) => ("added", path, None, None),
                (Some(path), None) => ("deleted", path, None, None),
                (None, None) => return,
            }
        },
        // A tree entry, or any future variant: not a leaf path change.
        _ => return,
    };
    out.push(FileChange {
        commit_hash: hash.to_string(),
        path,
        additions,
        deletions,
        change_kind: change_kind.to_string(),
    });
}

/// Map a worktree-root-relative `location` to the index-root-relative path, or `None` if it falls
/// outside the index `root`. At the worktree root (`scope` is `None`) it's the location unchanged;
/// under a subtree index the location must start with the subtree prefix (#213 review).
fn scope_relative(scope: Option<&str>, location: &str) -> Option<String> {
    match scope {
        None => Some(location.to_string()),
        Some(prefix) => location
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(str::to_string),
    }
}

/// Line counts for a blob/symlink change via gix's blob diff. `(None, None)` for a
/// binary/uncountable diff — AND for submodule gitlinks (`EntryKind::Commit`, a commit id with no
/// text content): gix's blob diff accepts only blobs/symlinks. The old `git log --numstat`
/// synthesized `1/0`,`1/1`,`0/1` for submodule pointer changes; we record the path with unknown
/// counts rather than fall back to a raw `git` invocation. Tracked in #218.
fn blob_line_counts(
    change: &Change<'_, '_, '_>,
    diff_cache: &mut gix::diff::blob::Platform,
) -> (Option<i64>, Option<i64>) {
    change
        .diff(diff_cache)
        .ok()
        .and_then(|mut platform| platform.line_counts().ok().flatten())
        .map_or((None, None), |stats| {
            (Some(i64::from(stats.insertions)), Some(i64::from(stats.removals)))
        })
}

/// The index `root`'s path WITHIN the worktree (e.g. `Some("tools/rag-rat")`), or `None` when
/// `root` IS the worktree root (or the two can't be related). Used to scope history to the subtree
/// the old `git log -- .` (run from `root`) covered. Canonicalizes both sides so a symlink / path-
/// representation mismatch doesn't spuriously scope (or fail to scope).
fn scope_prefix(root: &Path, worktree_root: &Path) -> Option<String> {
    let root = rag_rat_base::paths::canonicalize(root).ok()?;
    let worktree_root = rag_rat_base::paths::canonicalize(worktree_root).ok()?;
    let relative = root.strip_prefix(&worktree_root).ok()?;
    let prefix = path_string(relative);
    (!prefix.is_empty()).then_some(prefix)
}
