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

/// Record a migration id this binary does not know — the exact `schema_version` state a store
/// carries once a NEWER binary has migrated it, and therefore the state a pre-upgrade binary sees
/// after V097 lands.
///
/// Written as an unknown id rather than by tampering with V097's own row because the fence is the
/// LADDER's, not this migration's: `known_migration` is a closed set, so any id outside it makes
/// `status` answer `Newer`. V097 arms that fence purely by being on the ladder, which
/// `schema::migration_arming` already pins mechanically.
fn stamp_a_migration_from_a_newer_binary(db: &IndexDatabase) {
    db.storage
        .connection()
        .execute(
            "INSERT INTO schema_version(id, applied_at_ms, checksum, description)
             VALUES ('097_applied_by_a_newer_binary', 0, 'sha256:future', 'future migration')",
            [],
        )
        .unwrap();
}

/// What a pre-upgrade binary is looking at once a newer one has converted the store: a main
/// checkout plus a linked worktree whose overlay rows are keyed by a spelling this binary no
/// longer derives, and a `schema_version` roster carrying an id it does not know.
///
/// Returned so each route into the store can be driven against the SAME state. The scratch roots
/// come back with it because dropping them deletes the checkouts.
struct AStoreANewerBinaryMigrated {
    config: Config,
    db: IndexDatabase,
    stale_id: String,
    stale_rows: i64,
    _main: ScratchRoot,
    _linked: ScratchRoot,
}

fn a_store_a_newer_binary_migrated() -> AStoreANewerBinaryMigrated {
    let (main, config) = canonical_root_repo();
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let (linked_scratch, first) = canonical_worktree_destination();
    run_git(&main, &["worktree", "add", "-q", "-b", "first", first.to_str().unwrap()]);
    fs::write(first.join("crate/src/first.rs"), "pub fn first_fn() {}\n").unwrap();
    run_git(&first, &["add", "."]);
    run_git(&first, &["commit", "-q", "-m", "first"]);
    let first_report = db.index_worktree_overlay(&config, &first, &mut |_| {}).unwrap();

    // Only the CHECKOUT keys go stale — not `repo_roots.root` / `source_root`, which the
    // empty-index guard reads: a stale root there would abort the pass for an unrelated reason and
    // the mutation this test exists to catch would hide behind it.
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
    let stale_id = format!("{STALE_SPELLING_MARKER}{}", first_report.worktree_id);
    let stale_rows = rows_keyed_to(&db, &stale_id);
    assert!(stale_rows > 0, "the linked worktree's overlay is keyed to the stale spelling");

    // A newer binary migrated the store while this one stayed resident, holding its connection.
    stamp_a_migration_from_a_newer_binary(&db);
    let status = rag_rat_db::schema::status(db.storage.connection()).unwrap();
    assert_eq!(status.state, rag_rat_db::schema::SchemaState::Newer);

    AStoreANewerBinaryMigrated {
        config,
        db,
        stale_id,
        stale_rows,
        _main: main,
        _linked: linked_scratch,
    }
}

/// The coexistence question a store-converting migration has to answer: what stops a binary that
/// predates the conversion from garbage-collecting the rows it just rekeyed?
///
/// The answer is the schema version, and this pins that it reaches a RESIDENT process and not only
/// a fresh command. A maintenance pass re-opens the database through `open_and_migrate` at its
/// start — inside the per-repo write flock, and long before its gc stage — so the pass after an
/// upgrade fails at the version gate with nothing read, written, or collected. Without that, an
/// older build would derive its live worktree set from the OLD spelling, find every rekeyed id
/// outside it, and prune registered linked worktrees as dead checkouts.
///
/// Driven with the checkout spellings left STALE on disk, so the assertion is not merely "the pass
/// returned an error": if the gate ever stopped refusing, gc would run with a live set that cannot
/// match those rows and would delete them, and the surviving-rows assertion fails too.
#[test]
fn a_pre_upgrade_binary_cannot_collect_a_store_a_newer_one_migrated() {
    let store = a_store_a_newer_binary_migrated();

    // The resident process's NEXT pass — the gc-cadence one, the only kind that prunes.
    let err = crate::watch::maintenance_pass(&store.config, true)
        .expect_err("a pass must refuse a store migrated by a newer binary");
    assert!(
        err.to_string().contains("newer rag-rat"),
        "the pass must fail at the schema version gate, not incidentally: {err}",
    );

    assert_eq!(
        rows_keyed_to(&store.db, &store.stale_id),
        store.stale_rows,
        "the refused pass collected nothing — an older binary cannot prune rows keyed by a \
         spelling it no longer derives",
    );
}

/// The same fence over the OTHER route into the store: `rag-rat index --full`.
///
/// A full rebuild does not open through `open_and_migrate` — it reaches `schema::apply` through
/// `create_or_migrate`, which asks only whether the state is `Compatible` and migrates whenever it
/// is not. `Newer` fell into that "not" and the rebuild proceeded: this binary's ladder replayed
/// over a store a newer one had already converted, and the rebuild then re-registered
/// `repo_roots.root` / `source_root` and re-staged the base scope in the spelling THIS binary
/// derives, stranding the converted overlay rows for the next collector to prune. Refused inside
/// `apply_schema_under_lock`, the single function that calls `schema::apply`, so the refusal holds
/// on every route rather than on the ones an enumeration remembered.
///
/// Asserted on the error MESSAGE and on the surviving rows, for the same reason as the pass test —
/// "it returned an error" would also be satisfied by a rebuild that failed after writing.
#[test]
fn a_pre_upgrade_full_index_cannot_replay_its_ladder_over_a_store_a_newer_one_migrated() {
    let store = a_store_a_newer_binary_migrated();

    // `expect_err` would need `IndexDatabase: Debug`, which it deliberately is not.
    let Err(err) = IndexDatabase::rebuild(&store.config) else {
        panic!("a full rebuild must refuse a store migrated by a newer binary")
    };
    assert!(
        err.to_string().contains("newer rag-rat"),
        "the rebuild must fail at the schema version gate, not incidentally: {err}",
    );

    assert_eq!(
        rows_keyed_to(&store.db, &store.stale_id),
        store.stale_rows,
        "the refused rebuild rewrote nothing — the converted overlay rows are still keyed as the \
         newer binary left them",
    );
}

/// `repo_meta` / `index_meta` are the open-ended half of the persisted surface: a table growing a
/// `worktree_id` is caught by
/// `migration_097_covers_every_worktree_id_column_in_the_schema`, but a new meta key is just a
/// string, and a path-valued one goes stale on the same upgrade with nothing to notice it.
///
/// So classify by VALUE rather than by memory: every meta value in a real, exercised index that is
/// an absolute path must be one the rekey rewrites, or an explicitly reviewed exception. A new key
/// that starts holding a checkout path fails here, by name, instead of quietly missing the sweep.
#[test]
fn every_absolute_path_in_the_meta_bag_is_rekeyed_or_reviewed() {
    /// Reviewed and deliberately NOT rekeyed. Each is an absolute path, and each is left alone for
    /// a reason that is about what the value MEANS, not about it being awkward to rewrite.
    const REVIEWED_NOT_REKEYED: &[&str] = &[
        // Provenance of the binary that last migrated this store (#585), read by humans
        // diagnosing a fleet stranding. Never compared against a canonicalized path — and
        // rewriting it would falsify the record of what actually ran.
        "last_migration_binary_exe",
        // A composite freshness key that FOLDS the history cursor, root spelling included.
        // Rekeying the cursor makes it stale, which is the change-coupling table's ordinary
        // invalidation path (a bounded window recompute the next history apply would trigger
        // anyway); rewriting a stamp from a migration would couple the ladder to its format.
        "git_coupling_stamp",
    ];

    let (main, config) = canonical_root_repo();
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    let (_first_scratch, first) = canonical_worktree_destination();
    run_git(&main, &["worktree", "add", "-q", "-b", "first", first.to_str().unwrap()]);
    fs::write(first.join("crate/src/first.rs"), "pub fn first_fn() {}\n").unwrap();
    run_git(&first, &["add", "."]);
    run_git(&first, &["commit", "-q", "-m", "first"]);
    let first_report = db.index_worktree_overlay(&config, &first, &mut |_| {}).unwrap();
    db.record_worktree_overlay_basis(&first_report.worktree_id, "base-sha", "linked-sha", 42)
        .unwrap();

    let basis_prefix = rag_rat_db::meta::WORKTREE_OVERLAY_BASIS_META_PREFIX;
    let conn = db.storage.connection();
    for table in ["repo_meta", "index_meta"] {
        let mut stmt = conn.prepare(&format!("SELECT key, value FROM main.{table}")).unwrap();
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (key, value) in rows {
            // The overlay basis carries its checkout in the KEY; the value is a sha triple.
            if let Some(worktree_id) = key.strip_prefix(basis_prefix) {
                assert!(
                    Path::new(worktree_id).is_absolute(),
                    "the overlay-basis key suffix is a checkout path, so V097 rekeys the KEY: \
                     {key}",
                );
                continue;
            }
            if !Path::new(&value).is_absolute() {
                continue;
            }
            let covered =
                rag_rat_db::schema::migrations::V097_PATH_VALUED_META_KEYS.contains(&key.as_str());
            assert!(
                covered || REVIEWED_NOT_REKEYED.contains(&key.as_str()),
                "`{table}[{key}]` holds an absolute path ({value:?}) that V097 neither rekeys nor \
                 records as reviewed. A persisted path is compared TEXTUALLY against a \
                 freshly-canonicalized one, so on the next Windows upgrade this value stops \
                 matching. Add it to V097_PATH_VALUED_META_KEYS, or to REVIEWED_NOT_REKEYED with \
                 the reason it must keep the spelling it was written with.",
            );
        }
    }
}
