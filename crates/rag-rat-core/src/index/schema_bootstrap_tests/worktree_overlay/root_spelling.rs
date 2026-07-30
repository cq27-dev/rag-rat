//! The overlay's scoping contract for the SPELLING a canonical `config.root` carries (#1048).
//!
//! `config_root` (beside this file) covers the case where the root is spelled through a symlink,
//! which is a Unix-only fixture. This module covers the other half: the root is exactly what
//! `Config::load` produces, and the question is whether that spelling is one the rest of the system
//! consumes. On Windows `std::fs::canonicalize` answers `\\?\C:\…`, and:
//!   * `git worktree add \\?\C:\…\linked` fails outright — `fatal: could not create leading
//!     directories of '//?/C:/…': Invalid argument`;
//!   * gix reports `workdir()` in the ordinary `C:\…` form, so the overlay's subdir derivation
//!     (`config.root` stripped against the workdir) matches nothing.
//!
//! Nothing here is `cfg`-gated: on Unix the assertions are cheap and hold trivially, and it is the
//! Windows leg that has to run them. That is deliberate — a Unix-only probe would have caught none
//! of this.

use super::*;

/// Whether `path` carries the Windows extended-length (`\\?\`) prefix. Textual on purpose: this is
/// asking about the SPELLING, and the answer must be the same one `git` and gix compute from the
/// bytes they are handed.
fn is_verbatim(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().starts_with(r"\\?\")
}

/// A committed `crate/`-in-repo main checkout, plus the `Config` a load from disk would produce for
/// its `crate/` subroot.
fn canonical_root_repo() -> (ScratchRoot, Config) {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("crate/src")).unwrap();
    fs::write(main.join("crate/src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);

    let config = source_config(main.join("crate"), Language::Rust);
    (main, config)
}

/// A linked-worktree destination in the same canonical spelling `config.root` carries — what a
/// caller building a sibling checkout from a loaded config actually hands `git`.
fn canonical_worktree_destination() -> (ScratchRoot, PathBuf) {
    let scratch = unique_temp_root();
    let _ = fs::remove_dir_all(&scratch);
    let destination = test_scratch::canonical_config_root(scratch.to_path_buf());
    (scratch, destination)
}

/// The two consumers of `config.root` must both accept the spelling it is normalized to: `git`
/// as a `worktree add` destination, and gix as a path its `workdir()` is a prefix of.
///
/// Before the fix this failed on Windows at the `git worktree add` — the canonical root is a
/// `\\?\` verbatim path there, and git cannot create leading directories for one.
#[test]
fn a_canonical_config_root_is_a_spelling_git_and_gix_both_accept() {
    let (main, config) = canonical_root_repo();
    assert!(
        !is_verbatim(&config.root),
        "a canonical config root must not carry the Windows verbatim prefix: {:?}",
        config.root,
    );

    // gix: the repository discovered FROM the canonical root reports a workdir the root is under,
    // so the overlay's `strip_prefix` derives the `crate/` subdir instead of collapsing to nothing.
    let repo = rag_rat_base::repo_discover::discover_repo(&config.root).unwrap();
    let workdir = rag_rat_base::paths::canonicalize_or_simplified(repo.workdir().unwrap());
    assert_eq!(
        config.root.strip_prefix(&workdir).ok(),
        Some(Path::new("crate")),
        "config.root {:?} must strip against the repository workdir {:?}",
        config.root,
        workdir,
    );

    // git: a destination spelled the way the canonical root is must be usable verbatim as a
    // `worktree add` argument.
    let (_scratch, destination) = canonical_worktree_destination();
    assert!(
        !is_verbatim(&destination),
        "the worktree destination is not verbatim: {destination:?}"
    );
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", destination.to_str().unwrap()]);
    assert!(destination.join("crate/src/a.rs").is_file(), "the linked checkout was created");
}

/// The end-to-end scoping this protects: with a root in the canonical spelling, a linked
/// worktree's refresh indexes the branch-only file under a `config.root`-relative path, and a
/// SECOND linked worktree sharing the same database neither sees nor disturbs the first one's rows.
///
/// Main checkout, two linked siblings, one database — the overlay's whole contract, driven through
/// the root spelling production produces rather than a hand-built one.
#[test]
fn a_canonical_config_root_scopes_the_overlay_and_isolates_siblings() {
    let (main, config) = canonical_root_repo();
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    assert!(
        path_in_scope(&db, "src/a.rs"),
        "the base index is scoped to `crate/`, so its paths are config-root-relative",
    );

    let (_first_scratch, first) = canonical_worktree_destination();
    run_git(&main, &["worktree", "add", "-q", "-b", "first", first.to_str().unwrap()]);
    fs::write(first.join("crate/src/first.rs"), "pub fn first_fn() {}\n").unwrap();
    run_git(&first, &["add", "."]);
    run_git(&first, &["commit", "-q", "-m", "first"]);
    let first_report = db.index_worktree_overlay(&config, &first, &mut |_| {}).unwrap();
    assert_eq!(first_report.indexed, 1, "the branch-only file is indexed into the overlay");
    assert!(
        path_in_scope(&db, "src/first.rs"),
        "the overlay row is keyed relative to `config.root`, so the subdir derivation matched",
    );
    assert_eq!(names_in_scope(&db, "src/first.rs"), vec!["first_fn".to_string()]);

    let (_second_scratch, second) = canonical_worktree_destination();
    run_git(&main, &["worktree", "add", "-q", "-b", "second", second.to_str().unwrap()]);
    fs::write(second.join("crate/src/second.rs"), "pub fn second_fn() {}\n").unwrap();
    run_git(&second, &["add", "."]);
    run_git(&second, &["commit", "-q", "-m", "second"]);
    let second_report = db.index_worktree_overlay(&config, &second, &mut |_| {}).unwrap();
    assert_eq!(second_report.indexed, 1, "the sibling's branch-only file gets its own overlay");

    // Active scope = the SECOND worktree: it sees its own file, never the sibling's.
    assert!(path_in_scope(&db, "src/second.rs"));
    assert!(!path_in_scope(&db, "src/first.rs"), "a sibling worktree's overlay is not visible");

    // The FIRST worktree's rows survived, read by their own `(worktree_id, commit_sha)` key rather
    // than through the connection's installed scope (still the second worktree's).
    let scoped = db.storage.connection();
    let rows: Vec<String> = scoped
        .prepare("SELECT path FROM main.files WHERE worktree_id = ?1 AND commit_sha = ''")
        .unwrap()
        .query_map([&first_report.worktree_id], |row| row.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(rows, vec!["src/first.rs".to_string()], "the first overlay is intact and isolated");
}
