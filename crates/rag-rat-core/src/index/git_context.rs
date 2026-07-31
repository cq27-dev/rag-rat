//! Git context for the active checkout: changed paths, worktree contexts, pathspec/path mapping,
//! and raw git invocation.

use rag_rat_base::paths::path_string;
use rag_rat_base::repo_discover::discover_repo;

use super::*;

#[derive(Debug, Default)]
pub(crate) struct GitChangedPaths {
    pub(crate) changed: BTreeSet<PathBuf>,
    pub(crate) deleted: BTreeSet<PathBuf>,
}

pub(crate) fn git_changed_paths(root: &Path) -> anyhow::Result<GitChangedPaths> {
    let repo = discover_repo(root)?;
    let worktree_root = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("git repository has no worktree"))?
        .to_path_buf();
    let mut paths = GitChangedPaths::default();

    for item in repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::Files)
        .tree_index_track_renames(tree_index::TrackRenames::Disabled)
        .into_iter(config_root_status_pathspec(&worktree_root, root))?
    {
        let item = item?;
        let Some(path) = repo_relative_path_to_config_path(&worktree_root, root, item.location())
        else {
            continue;
        };
        if root.join(&path).exists() {
            if !paths.deleted.contains(&path) {
                paths.changed.insert(path);
            }
        } else {
            paths.changed.remove(&path);
            paths.deleted.insert(path);
        }
    }

    Ok(paths)
}

/// The status pathspec for a SUBDIR config root, or `None` when the config root IS the worktree
/// root.
///
/// LITERAL MAGIC, NOT A GLOB. A repo-relative path rendered by `path_string` is DATA, and a bare
/// pathspec is a PATTERN: gix parses one in shell-glob mode, where `\` escapes the following byte
/// and `*`, `?`, `[` are wildcards, and a leading `:` opens a magic signature. So the subdir root
/// `tools\api` (a legal Unix directory NAME, which the rendering now preserves instead of
/// destroying) produced `tools\api/**`, which matches nothing under that directory — its
/// modifications and deletions never reached `git_changed_paths`, and the default Changed pass
/// left every file in it stale. `:(literal)<dir>` takes the rest of the spec as bytes, so no
/// escaping is needed and no name can be misread as a pattern; git pathspec semantics already make
/// a directory match everything under it, which is what the old `/**` suffix was for.
///
/// #474: a pathspec disables gix's ignored-DIRECTORY pruning wherever the spec could still match —
/// the former whole-root wildcard `"*"` could match anywhere, so the dirwalk descended into every
/// gitignored subtree (probing `.gitignore`/`.gitattributes` per directory), ~47k opens per
/// maintenance pass under a vendored benchmark corpus in `target/` where `git status` prunes the
/// same tree with zero. The whole-root case therefore passes NO pathspec (the choice
/// `is_worktree_dirty` documents), pinned by
/// `git_changed_paths_does_not_descend_into_ignored_directories`.
///
/// A SUBDIR root KEEPS its `<subdir>/**` spec (#474 review): gix still prunes everything the spec
/// cannot match — including sibling ignored trees, pinned by
/// `git_changed_paths_prunes_ignored_directories_outside_a_subdir_root` — and the bounded walk
/// keeps an I/O failure in an out-of-scope sibling path from aborting a healthy subdir's changed
/// indexing. The unpruned descent inside the subdir's OWN ignored dirs is the pre-existing,
/// far smaller cost.
fn config_root_status_pathspec(worktree_root: &Path, config_root: &Path) -> Option<BString> {
    let relative = config_root.strip_prefix(worktree_root).unwrap_or_else(|_| Path::new(""));
    let relative = path_string(relative);
    if relative.is_empty() || relative == "." {
        None
    } else {
        Some(BString::from(literal_pathspec(&relative)))
    }
}

/// A path as a gix pathspec that matches it EXACTLY as bytes — `:(literal)` disables every
/// wildcard, escape and magic-signature reading, so a name containing `\`, `*`, `?`, `[` or a
/// leading `:` names the file it spells rather than a pattern that happens to look like it.
///
/// This is the only place a rendered path becomes a gix pathspec; the read-side conversions
/// (`entry_by_path`, `blame_file`) take a literal path already and are not pattern-parsed.
fn literal_pathspec(path: &str) -> String {
    format!(":(literal){path}")
}

pub(crate) fn repo_relative_path_to_config_path(
    worktree_root: &Path,
    config_root: &Path,
    repo_relative_path: &gix::bstr::BStr,
) -> Option<PathBuf> {
    let path = PathBuf::from(repo_relative_path.to_str_lossy().as_ref());
    worktree_root.join(path).strip_prefix(config_root).ok().map(Path::to_path_buf)
}

/// HEAD commit sha for `root` via gix, or empty if unborn / not a repo (matching the old
/// `rev-parse HEAD` failure behavior).
pub(crate) fn head_sha(root: &Path) -> String {
    discover_repo(root).ok().map(|repo| repo_head_sha(&repo)).unwrap_or_default()
}

/// [`head_sha`] for an ALREADY-OPENED repository — byte-identical formatting (empty when unborn),
/// for callers holding a handle that must not pay a re-discover. Comparisons against a recorded
/// refresh basis (whose shas came from `head_sha`) rely on the two never diverging.
pub(crate) fn repo_head_sha(repo: &gix::Repository) -> String {
    repo.head_id().ok().map(|id| id.to_hex().to_string()).unwrap_or_default()
}

/// Whether the worktree has any uncommitted change (tracked modifications + untracked files), the
/// gix equivalent of a non-empty `git status --porcelain`. Lazy: stops at the first change.
pub(crate) fn is_worktree_dirty(root: &Path) -> bool {
    let Ok(repo) = discover_repo(root) else {
        return false;
    };
    let Ok(platform) = repo.status(gix::progress::Discard) else {
        return false;
    };
    // No pathspec → the whole worktree; lazy, so `.next()` stops at the first change.
    let Ok(mut changes) =
        platform.untracked_files(UntrackedFiles::Files).into_iter(None::<gix::bstr::BString>)
    else {
        return false;
    };
    changes.next().is_some()
}

/// Whether `relative` (a worktree-root-relative path) is modified in the worktree — the per-path,
/// filter-aware analog of [`is_worktree_dirty`]. gix status respects `.gitattributes` / autocrlf /
/// LFS normalization, so a clean-but-normalized file is NOT flagged (unlike a raw byte compare).
/// Conservative: a status error reports "not dirty" so callers (blame) don't lose results on a
/// transient status hiccup.
pub(crate) fn path_is_dirty(repo: &gix::Repository, relative: &Path) -> bool {
    let Ok(platform) = repo.status(gix::progress::Discard) else {
        return false;
    };
    // `:(literal)` over the `/`-separated rendering, for the reason
    // `config_root_status_pathspec` documents: the OS-native bytes of a file NAME are not a
    // pattern, and a name carrying `\`, `*`, `?`, `[` or a leading `:` would otherwise be parsed
    // as one and report a file with uncommitted edits as clean.
    let pathspec = BString::from(literal_pathspec(&path_string(relative)));
    let Ok(mut changes) = platform.untracked_files(UntrackedFiles::Files).into_iter([pathspec])
    else {
        return false;
    };
    changes.next().is_some()
}

/// The active-checkout `(commit_sha, worktree_id)` keys for `root`, as `open_config` derives them.
/// `pub` so out-of-crate callers that open an index by path (benches mirroring the production
/// `open_config` path, integration tests) can install the same active-checkout scope `search` uses.
pub fn resolve_git_context(root: &Path) -> (String, String) {
    let commit_sha = head_sha(root);
    (commit_sha, worktree_id_of(root))
}

/// Format a worktree checkout path into the `worktree_id` string scope rows are keyed by — shared
/// by `resolve_git_context`, `resolve_worktree_scope`, and (for liveness) `live_worktree_contexts`,
/// so the overlay WRITE key, the query scope, and the GC live set can't drift.
///
/// CANONICALIZES the path so the SAME physical worktree yields the SAME id no matter how it's
/// spelled — a symlinked or relative reference, or a trailing slash. Without this, indexing via one
/// spelling (CLI `--worktree`, the watcher's `proxy.base()`) and querying via another (the MCP
/// `worktree` param / cwd fallback) produced DIFFERENT ids: the query silently missed the overlay,
/// and GC — whose live set is canonical — pruned the live overlay as dead on every maintenance pass
/// (#219 review). Falls back to the literal path when it can't be resolved (a removed
/// worktree mid-GC), trailing slash trimmed either way.
pub(crate) fn worktree_id_of(path: &Path) -> String {
    let resolved = rag_rat_base::paths::canonicalize_or_simplified(path);
    resolved.to_string_lossy().trim_end_matches('/').to_string()
}

/// Resolve the active scope `(commit_sha, worktree_id)` for a query, honoring an optional caller
/// `worktree` (a linked-worktree checkout the caller is working in). The scope's BASE commit is
/// always `root`'s indexed HEAD (the shared base index) — a linked worktree is served as an OVERLAY
/// on that base, never as a separate index, and config/db stay anchored to `root` (#219). When
/// `worktree` names a valid linked sibling of `root`'s repo, the `worktree_id` selects that
/// worktree's overlay; otherwise (absent / main / foreign / unreadable) it falls back to `root`'s
/// own scope — never the wrong repo.
pub(crate) fn resolve_worktree_scope(root: &Path, worktree: Option<&Path>) -> (String, String) {
    let (base_sha, base_id) = resolve_git_context(root);
    match worktree.and_then(|candidate| validated_sibling_worktree(root, candidate)) {
        Some(linked) => (base_sha, worktree_id_of(&linked)),
        None => (base_sha, base_id),
    }
}

/// If `candidate` is a LINKED worktree (`git_dir != common_dir`) sharing `root`'s repository (same
/// common dir), return its checkout dir; otherwise `None`. Server-side validation of an UNTRUSTED
/// caller-supplied path (e.g. a cwd that on macOS can be `/`): a main-worktree, foreign-repo, or
/// unreadable path returns `None` so the caller falls back to the base scope rather than serving
/// the wrong repo (#219). The comparison canonicalizes git/common dirs; the returned checkout path
/// is gix's raw `workdir()` so its `worktree_id` matches the GC live set from
/// `live_worktree_contexts`.
fn validated_sibling_worktree(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let repo = discover_repo(candidate).ok()?;
    let git_dir = rag_rat_base::paths::canonicalize(repo.git_dir()).ok()?;
    let common_dir = rag_rat_base::paths::canonicalize(repo.common_dir()).ok()?;
    // The main worktree's per-worktree git dir IS the common dir; a linked worktree's differs. Main
    // → None → base scope (which already serves the main checkout).
    if git_dir == common_dir {
        return None;
    }
    // Same repository as `root` iff they share a common dir — rejects an unrelated repo's worktree.
    let root_common =
        rag_rat_base::paths::canonicalize(discover_repo(root).ok()?.common_dir()).ok()?;
    if common_dir != root_common {
        return None;
    }
    repo.workdir().map(Path::to_path_buf)
}

/// The live (commit_sha, worktree_id) keys across every worktree that shares this repo (the gix
/// equivalent of `git worktree list --porcelain`). Each worktree contributes its HEAD commit (for
/// clean rows) and its path (for dirty/overlay rows). Returns empty vecs outside a git worktree.
pub(crate) fn live_worktree_contexts(root: &Path) -> (Vec<String>, Vec<String>) {
    let mut commits = Vec::new();
    let mut worktrees = Vec::new();
    let Ok(repo) = discover_repo(root) else {
        return (commits, worktrees);
    };
    let add_repo =
        |worktrees: &mut Vec<String>, commits: &mut Vec<String>, repo: &gix::Repository| {
            if let Some(workdir) = repo.workdir() {
                worktrees.push(worktree_id_of(workdir));
            }
            if let Ok(id) = repo.head_id() {
                commits.push(id.to_hex().to_string());
            }
        };
    // The current worktree (may itself be the main one or a linked one).
    add_repo(&mut worktrees, &mut commits, &repo);
    // The MAIN worktree, ALWAYS — `worktrees()` enumerates only LINKED worktrees and
    // `repo.workdir()` is whichever checkout we were launched from, so when that's a linked
    // worktree the main one would otherwise be missing. A GC reading this set must keep the
    // main worktree's path/HEAD live or it could prune the main checkout's indexed rows (#213
    // review). The common dir IS the main repo's git dir.
    if let Ok(main) = gix::open(repo.common_dir()) {
        add_repo(&mut worktrees, &mut commits, &main);
    }
    // Linked worktrees: path from the proxy (robust even if the checkout is gone), HEAD from
    // opening the worktree's git dir. Use the inaccessible-tolerant open so a
    // temporarily-missing checkout STILL contributes its registered HEAD — otherwise a clean
    // row keyed by that commit could be GC-pruned even though the worktree is still registered
    // and may return (#213 review).
    if let Ok(proxies) = repo.worktrees() {
        for proxy in proxies {
            if let Ok(base) = proxy.base() {
                worktrees.push(worktree_id_of(&base));
            }
            if let Ok(linked) = proxy.into_repo_with_possibly_inaccessible_worktree()
                && let Ok(id) = linked.head_id()
            {
                commits.push(id.to_hex().to_string());
            }
        }
    }
    // The current/main/linked sets overlap (e.g. launched from the main worktree); dedup so each
    // live worktree path + HEAD appears once.
    worktrees.sort();
    worktrees.dedup();
    commits.sort();
    commits.dedup();
    (commits, worktrees)
}

#[cfg(test)]
mod worktree_scope_tests {

    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        rag_rat_base::test_git::run(dir, args);
    }

    /// An owned scratch dir (removed on drop) plus its canonical path, which is what the git
    /// resolution under test reports. Worktree-add destinations are also guards: the fresh empty
    /// dir satisfies `git worktree add`, and cleanup never depends on reaching the end of the test.
    fn temp_dir(tag: &str) -> rag_rat_base::test_scratch::ScratchDir {
        rag_rat_base::test_scratch::ScratchDir::new(&format!("ragrat-wtscope-{tag}"))
    }

    fn init_repo(tag: &str) -> (rag_rat_base::test_scratch::ScratchDir, PathBuf) {
        let dir = temp_dir(tag);
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        let canonical = rag_rat_base::paths::canonicalize(&dir).unwrap();
        (dir, canonical)
    }

    #[test]
    fn none_is_the_base_scope() {
        let (_scratch, main) = init_repo("none");
        assert_eq!(resolve_worktree_scope(&main, None), resolve_git_context(&main));
    }

    /// #474: the status walk must PRUNE gitignored directories, not descend into them — on a repo
    /// with a large ignored build tree (a vendored benchmark corpus under `target/`), descending
    /// costs tens of thousands of directory opens on EVERY maintenance pass. `git status` prunes;
    /// the gix walk must too. Observability: an inotify watch on a directory fires `Access(Open)`
    /// when that directory itself is opened (readdir), so a pruned walk delivers no events inside
    /// the ignored subtree. Linux-only (inotify), like the watch-placement tests.
    #[cfg(target_os = "linux")]
    #[test]
    fn git_changed_paths_does_not_descend_into_ignored_directories() {
        use notify::Watcher as _;
        let (_scratch, dir) = init_repo("ignored-prune");
        std::fs::write(dir.join(".gitignore"), "vendor/\n").unwrap();
        git(&dir, &["add", ".gitignore"]);
        git(&dir, &["commit", "-q", "-m", "ignore vendor"]);
        std::fs::create_dir_all(dir.join("vendor/nested")).unwrap();
        std::fs::write(dir.join("vendor/nested/blob.txt"), "ignored").unwrap();
        // A real change outside the ignored tree, so the walk has work to report.
        std::fs::write(dir.join("b.txt"), "fresh").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .unwrap();
        watcher.watch(&dir.join("vendor/nested"), notify::RecursiveMode::NonRecursive).unwrap();
        // Drain any setup noise until quiet (the watch-test discipline), so only the status
        // walk's own accesses can land in the assertion window.
        while rx.recv_timeout(std::time::Duration::from_millis(200)).is_ok() {}

        let paths = git_changed_paths(&dir).unwrap();
        assert!(
            paths.changed.contains(&PathBuf::from("b.txt")),
            "the real change is still reported: {paths:?}"
        );

        let mut intrusions = Vec::new();
        while let Ok(event) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            if let Ok(event) = event {
                intrusions.extend(event.paths);
            }
        }
        assert!(
            intrusions.is_empty(),
            "the status walk descended into the gitignored subtree: {intrusions:?}"
        );
    }

    /// The subdir-root complement (#474 review): a `<subdir>/**` pathspec keeps the status walk
    /// BOUNDED — gix prunes everything the spec cannot match, so a sibling gitignored tree is
    /// never entered even though a pathspec is present. This is the load-bearing assumption
    /// behind keeping the spec for subdir roots (isolation from out-of-scope failures) while the
    /// whole-root case drops it entirely.
    #[cfg(target_os = "linux")]
    #[test]
    fn git_changed_paths_prunes_ignored_directories_outside_a_subdir_root() {
        use notify::Watcher as _;
        let (_scratch, dir) = init_repo("subdir-prune");
        std::fs::write(dir.join(".gitignore"), "vendor/\n").unwrap();
        std::fs::create_dir_all(dir.join("crates")).unwrap();
        std::fs::write(dir.join("crates/lib.rs"), "pub fn a() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "subdir root"]);
        std::fs::create_dir_all(dir.join("vendor/nested")).unwrap();
        std::fs::write(dir.join("vendor/nested/blob.txt"), "ignored").unwrap();
        std::fs::write(dir.join("crates/lib.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .unwrap();
        watcher.watch(&dir.join("vendor/nested"), notify::RecursiveMode::NonRecursive).unwrap();
        while rx.recv_timeout(std::time::Duration::from_millis(200)).is_ok() {}

        let paths = git_changed_paths(&dir.join("crates")).unwrap();
        assert!(
            paths.changed.contains(&PathBuf::from("lib.rs")),
            "the subdir change is reported config-root-relative: {paths:?}"
        );

        let mut intrusions = Vec::new();
        while let Ok(event) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            if let Ok(event) = event {
                intrusions.extend(event.paths);
            }
        }
        assert!(
            intrusions.is_empty(),
            "a subdir pathspec must not open sibling ignored trees: {intrusions:?}"
        );
    }

    /// A rendered path is DATA; a bare gix pathspec is a PATTERN. Under a config root named
    /// `tools\api` — a legal Unix directory name the rendering now preserves — the old
    /// `<subdir>/**` spec spelled a backslash that gix's shell-glob parse reads as an escape, so it
    /// matched nothing at all: every modification and deletion under that root was missing from
    /// `git_changed_paths`, and the default Changed pass left those files stale forever.
    ///
    /// Unix-only: Windows forbids `\` in a name, so the fixture cannot exist there.
    #[cfg(unix)]
    #[test]
    fn a_backslash_named_subdir_root_still_reports_its_changes() {
        let dir = temp_dir("backslash-subdir");
        git(&dir, &["init", "-q"]);
        // A directory whose NAME contains a backslash, plus a slash-nested sibling that a collapsed
        // reading of the same spec would match instead.
        std::fs::create_dir_all(dir.join("tools\\api")).unwrap();
        std::fs::create_dir_all(dir.join("tools/api")).unwrap();
        std::fs::write(dir.join("tools\\api").join("edited.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(dir.join("tools\\api").join("removed.rs"), "pub fn b() {}\n").unwrap();
        std::fs::write(dir.join("tools/api").join("nested.rs"), "pub fn c() {}\n").unwrap();
        git(&dir, &["add", "-A", "."]);
        git(&dir, &["commit", "-q", "-m", "seed"]);

        std::fs::write(dir.join("tools\\api").join("edited.rs"), "pub fn a2() {}\n").unwrap();
        std::fs::remove_file(dir.join("tools\\api").join("removed.rs")).unwrap();

        let paths = git_changed_paths(&dir.join("tools\\api")).unwrap();
        assert!(
            paths.changed.contains(&PathBuf::from("edited.rs")),
            "a modification under a backslash-named root must be reported: {paths:?}"
        );
        assert!(
            paths.deleted.contains(&PathBuf::from("removed.rs")),
            "and so must a deletion: {paths:?}"
        );
        // The spec names one directory, not a pattern a sibling can also satisfy.
        let nested = git_changed_paths(&dir.join("tools/api")).unwrap();
        assert!(
            nested.changed.is_empty() && nested.deleted.is_empty(),
            "the slash-nested sibling is untouched and must report nothing: {nested:?}"
        );
    }

    /// The same class on the per-path status probe blame gates on. A whole-path spec survives a
    /// backslash by accident — pathspec matching tries a literal byte compare before the glob, and
    /// an exact path wins there — but a LEADING `:` is read as a magic signature before any
    /// matching happens at all, so a top-level file named `:notes.rs` reported clean no matter how
    /// dirty it was, and blame then attributed its uncommitted lines to the last commit.
    ///
    /// Unix-only, like the sibling above: neither name is legal on Windows.
    #[cfg(unix)]
    #[test]
    fn a_file_whose_name_looks_like_pathspec_syntax_reads_as_dirty_when_it_is() {
        let dir = temp_dir("pathspec-syntax-dirty");
        git(&dir, &["init", "-q"]);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        // `:notes.rs` opens a magic signature; `src/foo\bar.rs` spells an escape in glob mode.
        let names = [":notes.rs", "src/foo\\bar.rs"];
        for name in names {
            std::fs::write(dir.join(name), "pub fn a() {}\n").unwrap();
        }
        git(&dir, &["add", "-A", "."]);
        git(&dir, &["commit", "-q", "-m", "seed"]);

        let repo = discover_repo(&dir).unwrap();
        for name in names {
            assert!(!path_is_dirty(&repo, Path::new(name)), "{name} is committed and unmodified");
        }
        for name in names {
            std::fs::write(dir.join(name), "pub fn a2() {}\n").unwrap();
            assert!(
                path_is_dirty(&repo, Path::new(name)),
                "{name} has uncommitted edits and must not be read as a pattern"
            );
        }
    }

    #[test]
    fn linked_worktree_selects_its_overlay_on_the_base_commit() {
        let (_scratch, main) = init_repo("linked-main");
        let linked = temp_dir("linked-wt");
        git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
        // A commit on the branch so the linked HEAD diverges from the base.
        std::fs::write(linked.join("b.txt"), "branch").unwrap();
        git(&linked, &["add", "."]);
        git(&linked, &["commit", "-q", "-m", "branch"]);

        let (base_sha, base_id) = resolve_git_context(&main);
        let (sha, wt) = resolve_worktree_scope(&main, Some(&linked));
        // Overlay-on-base: the base commit stays the rooted checkout's HEAD; only the worktree_id
        // changes, selecting the linked worktree's overlay.
        assert_eq!(sha, base_sha, "base commit must remain the rooted checkout's HEAD");
        assert_ne!(wt, base_id, "worktree_id must select the linked worktree");
        assert_eq!(
            rag_rat_base::paths::canonicalize(PathBuf::from(&wt)).unwrap(),
            rag_rat_base::paths::canonicalize(linked).unwrap()
        );
    }

    #[test]
    fn main_worktree_path_falls_back_to_base() {
        let (_scratch, main) = init_repo("main-fallback");
        // Passing the MAIN checkout (git_dir == common_dir) is not a linked worktree → base scope.
        assert_eq!(resolve_worktree_scope(&main, Some(&main)), resolve_git_context(&main));
    }

    #[test]
    fn foreign_repo_worktree_falls_back_to_base() {
        let (_scratch, main) = init_repo("foreign-main");
        let (_scratch2, other) = init_repo("foreign-other");
        let other_linked = temp_dir("foreign-linked");
        git(&other, &["worktree", "add", "-q", other_linked.to_str().unwrap()]);
        // A genuine linked worktree, but of a DIFFERENT repo (common dir differs) → base scope,
        // never the foreign repo.
        assert_eq!(resolve_worktree_scope(&main, Some(&other_linked)), resolve_git_context(&main));
    }

    #[test]
    fn unreadable_path_falls_back_to_base() {
        let (_scratch, main) = init_repo("unreadable");
        // The macOS bogus cwd `/` and a nonexistent path both resolve to base — no panic, no wrong
        // repo (untrusted-input guard).
        assert_eq!(resolve_worktree_scope(&main, Some(Path::new("/"))), resolve_git_context(&main));
        assert_eq!(
            resolve_worktree_scope(&main, Some(&main.join("nope"))),
            resolve_git_context(&main)
        );
    }
}
