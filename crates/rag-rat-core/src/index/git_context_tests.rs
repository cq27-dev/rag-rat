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

/// The wildcard half of the same class, and the direction a per-path spec does NOT survive by
/// accident. `*` is legal in a Unix file name and a wildcard in gix's shell-glob mode, so the
/// spec `a*b.rs` also matches `axb.rs`: an untouched file is reported dirty because a SIBLING
/// has uncommitted edits, and blame then refuses the committed attribution it should have used.
/// The literal byte compare that rescues an exact spelling cannot help here — the extra match
/// is a different path, not the same one spelled oddly.
///
/// Escaping the spec as a GLOB (`globset::escape`, which brackets `? * [ ] { }`) would cover
/// only this case: it leaves `\` and a leading `:` untouched, and neither is glob syntax —
/// they are an escape and a magic signature, parsed before globbing. `:(literal)` is what
/// covers all three, which is why the spec is built that way rather than escaped.
///
/// Unix-only: Windows forbids `*` in a name, so the fixture cannot exist there.
#[cfg(unix)]
#[test]
fn a_wildcard_named_file_is_not_dirty_because_a_sibling_is() {
    let dir = temp_dir("pathspec-wildcard-clean");
    git(&dir, &["init", "-q"]);
    // `a*b.rs` is the file under test; `axb.rs` is what its name matches read as a pattern.
    for name in ["a*b.rs", "axb.rs"] {
        std::fs::write(dir.join(name), "pub fn a() {}\n").unwrap();
    }
    git(&dir, &["add", "-A", "."]);
    git(&dir, &["commit", "-q", "-m", "seed"]);

    // Only the sibling is edited.
    std::fs::write(dir.join("axb.rs"), "pub fn a2() {}\n").unwrap();

    let repo = discover_repo(&dir).unwrap();
    assert!(path_is_dirty(&repo, Path::new("axb.rs")), "the sibling really is dirty");
    assert!(
        !path_is_dirty(&repo, Path::new("a*b.rs")),
        "a wildcard in the NAME must not let a sibling's edits report this file as dirty",
    );
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

    let CheckoutKey { commit_sha: base_sha, worktree_id: base_id } = resolve_git_context(&main);
    let CheckoutKey { commit_sha: sha, worktree_id: wt } =
        resolve_worktree_scope(&main, Some(&linked));
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
    assert_eq!(resolve_worktree_scope(&main, Some(&main.join("nope"))), resolve_git_context(&main));
}
