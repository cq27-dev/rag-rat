//! The overlay's scoping contract for `config.root` (#1027).
//!
//! `Config::load` normalizes its root through `canonicalize()`, and the overlay derives
//! `config.root`'s subdir by stripping it against the repo's canonicalized workdir. A root spelled
//! any other way — a symlinked `$PWD`, macOS's `/var` for `/private/var`, a Windows 8.3 alias —
//! used to strip to nothing: the refresh then scoped itself at the repo root, matched no target
//! path, wrote nothing, invalidated no client, and still returned `Ok(())`.
//!
//! These tests pin the two halves of the fix: the overlay normalizes the root it is handed, and
//! refuses (loudly) to run when no subdir can be derived at all.

use super::*;

/// The refresh's observable freshness token — what a connected editor polls. A refresh that
/// scopes itself at the wrong directory leaves this untouched while still reporting success, which
/// is exactly the silent failure this file exists to catch.
fn lens_revision(db: &IndexDatabase) -> String {
    db.lens_version().unwrap().revision
}

/// A `crate/`-in-repo fixture reached through a SYMLINKED root: `alias` points at the real scratch
/// dir, and the `Config` deliberately keeps the alias spelling. Returns the guards (kept alive for
/// the test), the config, and the linked worktree path.
///
/// `config.root` is set back to the alias spelling ON PURPOSE, after the fixture canonicalized it:
/// a `Config` assembled in-process (a fixture, an embedding caller, a hand-edited root) carries
/// whatever spelling it was given, and the overlay must scope correctly regardless.
#[cfg(unix)]
fn symlinked_root_fixture() -> (ScratchRoot, ScratchRoot, ScratchRoot, Config, PathBuf) {
    let real = unique_temp_root();
    let _ = fs::remove_dir_all(&real);
    fs::create_dir_all(real.join("crate/src")).unwrap();
    fs::write(real.join("crate/src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&real);
    run_git(&real, &["add", "."]);
    run_git(&real, &["commit", "-q", "-m", "base"]);

    let alias = unique_temp_root();
    let _ = fs::remove_dir_all(&alias);
    std::os::unix::fs::symlink(real.as_path(), alias.as_path()).unwrap();

    let mut config = source_config(alias.join("crate"), Language::Rust);
    config.root = alias.join("crate");

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&real, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    let linked_path = linked.to_path_buf();
    (real, alias, linked, config, linked_path)
}

/// A linked-worktree refresh scoped through a NON-CANONICAL `config.root` must index the branch's
/// files and move the Lens revision. Before the fix the subdir stripped to nothing, every
/// candidate kept its `crate/` prefix, no target matched, and the pass reported `Ok(())` having
/// written nothing — a connected editor was never invalidated.
#[cfg(unix)]
#[test]
fn a_symlinked_config_root_still_scopes_the_overlay_to_the_config_subdir() {
    let (_real, _alias, _linked, config, linked_path) = symlinked_root_fixture();
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    assert!(
        path_in_scope(&db, "src/a.rs"),
        "the base index is scoped to `crate/`, so its paths are config-root-relative",
    );

    fs::write(linked_path.join("crate/src/b.rs"), "pub fn branch_fn() {}\n").unwrap();
    run_git(&linked_path, &["add", "."]);
    run_git(&linked_path, &["commit", "-q", "-m", "branch"]);

    let before = lens_revision(&db);
    let report = db.index_worktree_overlay(&config, &linked_path, &mut |_| {}).unwrap();

    assert_eq!(report.indexed, 1, "the branch-only file is indexed into the overlay");
    assert!(path_in_scope(&db, "src/b.rs"), "the overlay row is keyed relative to `config.root`");
    assert_eq!(names_in_scope(&db, "src/b.rs"), vec!["branch_fn".to_string()]);
    assert_ne!(before, lens_revision(&db), "a refresh that indexed rows must invalidate clients");
}

/// Sibling isolation under the same non-canonical root: a SECOND linked worktree's refresh must
/// not disturb the first one's overlay rows, and the base scope must stay on its own content.
#[cfg(unix)]
#[test]
fn a_symlinked_config_root_keeps_sibling_worktree_overlays_isolated() {
    let (real, _alias, _linked, config, first_path) = symlinked_root_fixture();
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    fs::write(first_path.join("crate/src/first.rs"), "pub fn first_fn() {}\n").unwrap();
    run_git(&first_path, &["add", "."]);
    run_git(&first_path, &["commit", "-q", "-m", "first"]);
    let first = db.index_worktree_overlay(&config, &first_path, &mut |_| {}).unwrap();
    assert_eq!(first.indexed, 1);

    let second = unique_temp_root();
    let _ = fs::remove_dir_all(&second);
    run_git(&real, &["worktree", "add", "-q", "-b", "other", second.to_str().unwrap()]);
    fs::write(second.join("crate/src/second.rs"), "pub fn second_fn() {}\n").unwrap();
    run_git(&second, &["add", "."]);
    run_git(&second, &["commit", "-q", "-m", "second"]);
    let report = db.index_worktree_overlay(&config, &second, &mut |_| {}).unwrap();
    assert_eq!(report.indexed, 1, "the sibling's branch-only file is indexed into its own overlay");

    // Active scope = the SECOND worktree: it sees its own file, never the sibling's.
    assert!(path_in_scope(&db, "src/second.rs"));
    assert!(!path_in_scope(&db, "src/first.rs"), "a sibling worktree's overlay is not visible");

    // The FIRST worktree's overlay rows survived the sibling's refresh. Read them by their own
    // `(worktree_id, commit_sha)` key rather than through the connection's installed scope — the
    // connection is still scoped to the SECOND worktree, and re-scoping it would only re-test what
    // `path_in_scope` above already covers.
    let scoped = db.storage.connection();
    let rows: Vec<String> = scoped
        .prepare("SELECT path FROM main.files WHERE worktree_id = ?1 AND commit_sha = ''")
        .unwrap()
        .query_map([&first.worktree_id], |row| row.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(rows, vec!["src/first.rs".to_string()], "the first overlay is intact and isolated");
}

/// A root that resolves OUTSIDE its repository's working tree cannot be scoped at all — the
/// overlay must say so instead of silently scoping at the repo root and reporting success. The
/// pre-fix fallback made this indistinguishable from an unchanged worktree.
#[cfg(unix)]
#[test]
fn a_config_root_resolving_outside_the_repo_fails_loudly_instead_of_mis_scoping() {
    let repo = unique_temp_root();
    let _ = fs::remove_dir_all(&repo);
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&repo);
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "base"]);

    let config = source_config(repo.to_path_buf(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&repo, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/b.rs"), "pub fn branch_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    // An in-repo directory that is really a link to somewhere outside it: lexically inside the
    // working tree (so the repo still discovers), but it resolves elsewhere, so no subdir of the
    // working tree describes it.
    let outside = unique_temp_root();
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(outside.as_path(), repo.join("escape").as_path()).unwrap();
    let mut escaping = config.clone();
    escaping.root = repo.join("escape");

    let err = db
        .index_worktree_overlay(&escaping, &linked, &mut |_| {})
        .expect_err("an unscopable root must not report a successful, empty refresh");
    let message = format!("{err:#}");
    assert!(
        message.contains("cannot scope a worktree overlay"),
        "the failure must name the scoping problem, got: {message}",
    );
}

/// The fixture `Config` must carry a CANONICAL root, exactly as `Config::load` does. Without this
/// the whole suite exercises a configuration production can never produce, and root-spelling bugs
/// stay invisible on the platform the per-PR matrix runs on.
#[cfg(unix)]
#[test]
fn the_fixture_config_canonicalizes_its_root_like_config_load() {
    let real = unique_temp_root();
    let _ = fs::remove_dir_all(&real);
    fs::create_dir_all(real.join("crate/src")).unwrap();

    let alias = unique_temp_root();
    let _ = fs::remove_dir_all(&alias);
    std::os::unix::fs::symlink(real.as_path(), alias.as_path()).unwrap();

    let config = source_config(alias.join("crate"), Language::Rust);
    assert_eq!(
        config.root,
        alias.join("crate").canonicalize().unwrap(),
        "a fixture Config must normalize its root the way `Config::load` does",
    );
    assert!(
        config.database.starts_with(&config.root),
        "the fixture database path follows the canonical root",
    );
}
