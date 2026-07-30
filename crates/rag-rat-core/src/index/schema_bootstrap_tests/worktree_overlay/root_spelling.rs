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

/// The marker a poisoned fixture prefixes onto every persisted spelling, standing in for the
/// `\\?\` a pre-fix Windows index wrote. A stand-in because the real prefix is only ever produced
/// (and only ever rewritten) on Windows, so a fixture built with it would leave the Linux gate
/// asserting that nothing happened — which a rekey covering no tables at all would satisfy too.
const STALE_SPELLING_MARKER: &str = "<<stale>>";

/// Rewrite every persisted checkout spelling into the stale form, the way an index written before
/// the canonicalization change carries it.
///
/// Every target is named LITERALLY here — no list, constant, or helper is shared with the
/// migration. A fixture poisoned through the code under test can only ever be as complete as that
/// code, so a table or key the rekey forgets would be one the fixture never poisoned either, and
/// the miss would be invisible. Enumerated independently, dropping a target from the migration's
/// own lists turns into a failing assertion below.
fn poison_persisted_spellings(db: &IndexDatabase) {
    let conn = db.storage.connection();
    for table in ["files", "packages", "oracle_runs", "external_symbols"] {
        conn.execute(
            &format!(
                "UPDATE main.{table} SET worktree_id = ?1 || worktree_id WHERE worktree_id != ''"
            ),
            [STALE_SPELLING_MARKER],
        )
        .unwrap();
    }
    conn.execute("UPDATE main.repo_roots SET root = ?1 || root", [STALE_SPELLING_MARKER]).unwrap();
    for key in ["source_root", "git_history_indexed_root"] {
        conn.execute(
            "UPDATE main.repo_meta SET value = ?1 || value WHERE key = ?2",
            rusqlite::params![STALE_SPELLING_MARKER, key],
        )
        .unwrap();
    }
    // Only the SUFFIX goes stale: the prefix is a constant, the worktree path after it is what an
    // older binary spelled differently.
    let prefix = rag_rat_db::meta::WORKTREE_OVERLAY_BASIS_META_PREFIX;
    conn.execute(
        "UPDATE main.repo_meta SET key = ?1 || substr(key, ?2) WHERE substr(key, 1, ?3) = ?4",
        rusqlite::params![
            format!("{prefix}{STALE_SPELLING_MARKER}"),
            prefix.len() as i64 + 1,
            prefix.len() as i64,
            prefix,
        ],
    )
    .unwrap();
}

/// Raw count of rows keyed to `worktree_id`, bypassing the scope view — what GC prunes when a
/// checkout falls out of the live set.
fn rows_keyed_to(db: &IndexDatabase, worktree_id: &str) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE worktree_id = ?1", [worktree_id], |row| {
            row.get(0)
        })
        .unwrap()
}

/// The working-tree root recorded in `repo_roots` — the "this checkout was indexed here" signal
/// `repo_indexed_at_this_root` compares TEXTUALLY against `config.root` behind the empty-index
/// guard.
fn recorded_repo_root(db: &IndexDatabase) -> Option<String> {
    use rusqlite::OptionalExtension as _;
    db.storage
        .connection()
        .query_row("SELECT root FROM main.repo_roots", [], |row| row.get::<_, String>(0))
        .optional()
        .unwrap()
}

/// Whether the git-history reload gate still accepts the indexed commit / file-change rows for
/// `root`. It compares the recorded `git_history_indexed_root` cursor TEXTUALLY against a freshly
/// canonicalized root, so a stale spelling answers false — and false means the next pass deletes
/// and re-reads the entire history off a fresh revwalk and wipes the repo's blame cache with it.
fn history_is_current(db: &IndexDatabase, root: &Path) -> bool {
    crate::index::git_history::is_history_current(db.storage.connection(), root)
}

/// Upgrading an index whose persisted spellings predate the canonicalization change must REKEY
/// them, not orphan them.
///
/// This is the data-loss case (#1048). A non-empty `worktree_id` is a canonicalized checkout path
/// — it keys every linked worktree's overlay and every dirty row (committed base rows are shared
/// across checkouts under `worktree_id = ''` and are not implicated). Once production answers a
/// different spelling than the one on disk, those rows fall out of the active scope AND out of
/// `garbage_collect`'s live set, which is built from the new spelling: GC sees each stored id as a
/// checkout that no longer exists and deletes its rows. Registered, live worktrees, pruned as
/// dead, on the first maintenance pass after the upgrade. The recorded roots go with them: the
/// empty-index guard's "indexed here" signal, and the git-history reload cursor whose staleness
/// costs a full revwalk and the repo's blame cache on the first pass after the upgrade.
///
/// Driven through a real main checkout plus TWO linked worktrees on ONE database, so the assertion
/// covers the active-checkout scope and sibling preservation, not just a single-checkout shape.
#[test]
fn an_upgrade_rekeys_stale_checkout_spellings_instead_of_collecting_them() {
    let (main, config) = canonical_root_repo();
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let (_first_scratch, first) = canonical_worktree_destination();
    run_git(&main, &["worktree", "add", "-q", "-b", "first", first.to_str().unwrap()]);
    fs::write(first.join("crate/src/first.rs"), "pub fn first_fn() {}\n").unwrap();
    run_git(&first, &["add", "."]);
    run_git(&first, &["commit", "-q", "-m", "first"]);
    let first_report = db.index_worktree_overlay(&config, &first, &mut |_| {}).unwrap();

    let (_second_scratch, second) = canonical_worktree_destination();
    run_git(&main, &["worktree", "add", "-q", "-b", "second", second.to_str().unwrap()]);
    fs::write(second.join("crate/src/second.rs"), "pub fn second_fn() {}\n").unwrap();
    run_git(&second, &["add", "."]);
    run_git(&second, &["commit", "-q", "-m", "second"]);
    let second_report = db.index_worktree_overlay(&config, &second, &mut |_| {}).unwrap();

    let first_rows = rows_keyed_to(&db, &first_report.worktree_id);
    let second_rows = rows_keyed_to(&db, &second_report.worktree_id);
    assert!(first_rows > 0 && second_rows > 0, "both overlays are keyed by their checkout path");
    let recorded_root = recorded_repo_root(&db);
    assert_eq!(
        recorded_root.as_deref(),
        Some(config.root.to_string_lossy().as_ref()),
        "the indexed-here signal records the canonical root spelling",
    );
    assert!(
        history_is_current(&db, &config.root),
        "the rebuild leaves the git-history reload gate satisfied, so there is a skipped reload \
         for a stale spelling to un-skip",
    );

    // The per-worktree refresh basis, recorded the way a completed overlay refresh leaves it. Its
    // worktree identity lives in the KEY, so it is the one value a column-only rekey would miss.
    db.record_worktree_overlay_basis(&first_report.worktree_id, "base-sha", "linked-sha", 42)
        .unwrap();

    // The store as an older binary left it.
    poison_persisted_spellings(&db);
    assert_eq!(
        rows_keyed_to(&db, &first_report.worktree_id),
        0,
        "the harm this guards: with a stale spelling on disk the overlay rows are unreachable \
         under the id production now derives",
    );
    assert!(
        !history_is_current(&db, &config.root),
        "the second harm: the git-history reload gate no longer recognizes its own cursor, so the \
         next pass re-reads the whole history and wipes the blame cache",
    );

    // The upgrade.
    rag_rat_db::schema::migrations::rekey_persisted_path_spellings(
        db.storage.connection(),
        |stored| stored.strip_prefix(STALE_SPELLING_MARKER).map(str::to_string),
    )
    .unwrap();

    assert_eq!(
        rows_keyed_to(&db, &first_report.worktree_id),
        first_rows,
        "every row of the first overlay is rekeyed, not a subset",
    );
    assert_eq!(rows_keyed_to(&db, &second_report.worktree_id), second_rows);
    assert_eq!(
        recorded_repo_root(&db),
        recorded_root,
        "the indexed-here signal behind the empty-index guard is rekeyed too — left stale, an \
         established checkout reads as a first-time empty repo",
    );
    assert!(
        history_is_current(&db, &config.root),
        "the git-history reload cursor is rekeyed alongside them, so the upgrade does not force a \
         full revwalk and a cold blame cache",
    );

    // The load-bearing half: GC's live set is built from the CURRENT spelling, so a row the rekey
    // missed is a row GC deletes as a dead checkout. Running it here is what turns "the rekey
    // covered these tables" into "no live worktree loses data on upgrade".
    set_base_scope(&mut db, &config.root);
    db.garbage_collect().unwrap();
    assert_eq!(
        rows_keyed_to(&db, &first_report.worktree_id),
        first_rows,
        "a registered linked worktree's overlay is not collected as a dead checkout",
    );
    assert_eq!(
        rows_keyed_to(&db, &second_report.worktree_id),
        second_rows,
        "the sibling worktree's overlay is preserved alongside it, not just the last one indexed",
    );

    // The refresh basis is keyed BY worktree id, so it has to move with the rows it describes —
    // otherwise the overlay re-derives from scratch and the GC that prunes basis rows outside the
    // live set drops the quiet-window anchor.
    assert!(
        db.worktree_overlay_basis(&first_report.worktree_id).unwrap().is_some(),
        "the overlay refresh basis is rekeyed alongside its overlay rows",
    );
}
