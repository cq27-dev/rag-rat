//! Multi-repo scoping (memory-sync phase A3): two registered repos share ONE database, and the
//! `repo_id` dimension must keep them isolated through the scope view (reads) and `garbage_collect`
//! (sweeps) — even when they share a commit sha (the fork case) or an identical `(path, commit)`
//! dead-context row. These pin the A3 disposition's "direct-scoped core tables" contract.
//!
//! Building two repos in one DB is deliberately below the phase-A open pipeline (which is single-
//! repo until A7's multi-repo registration): repo A is indexed for real, then repo B's registry row
//! and rows are seeded directly, and the connection is re-scoped by setting `active_repo_id` (a
//! `pub` field) + `set_context` — the exact machinery A7 will drive per-repo.

use super::*;

/// A shared DB holding repo A (really indexed) plus repo B (seeded rows at repo A's SAME commit).
struct TwoRepoFixture {
    db: IndexDatabase,
    root_a: PathBuf,
    repo_a_id: String,
    /// The commit both repos' rows sit at — proves the isolation is by `repo_id`, not commit sha.
    shared_commit: String,
}

const REPO_B: &str = "test-repo-b";
const DEAD_COMMIT: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

fn seed_repo_b_file(conn: &rusqlite::Connection, path: &str, commit: &str) {
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id)
         VALUES (?1, 'rust', 'source', 'bsha', 0, 0, ?2, '', ?3)",
        rusqlite::params![path, commit, REPO_B],
    )
    .unwrap();
}

/// Index repo A into a shared DB for real (a git checkout, so the scope view has a real commit),
/// then seed repo B directly: its registry row + a live file at repo A's commit + an identical
/// `(path, commit)` DEAD-context row in EACH repo (for the gc parity assertion). The connection
/// stays scoped to repo A (as `rebuild` left it).
fn two_repo_fixture() -> TwoRepoFixture {
    let root_a = unique_temp_root();
    let _ = fs::remove_dir_all(&root_a);
    fs::create_dir_all(root_a.join("src")).unwrap();
    fs::write(root_a.join("src/a_only.rs"), "pub fn a_only() {}\n").unwrap();
    run_git(&root_a, &["init", "-q", "-b", "main"]);
    run_git(&root_a, &["config", "user.email", "t@e"]);
    run_git(&root_a, &["config", "user.name", "t"]);
    run_git(&root_a, &["add", "."]);
    run_git(&root_a, &["commit", "-q", "-m", "init"]);

    let shared_db = unique_temp_root().join("shared.sqlite");
    let mut config_a = source_config(root_a.clone(), Language::Rust);
    config_a.database = shared_db;
    let db = IndexDatabase::rebuild(&config_a).unwrap();
    let repo_a_id = db.active_repo_id.clone();
    let shared_commit = db.active_commit_sha.clone();
    assert!(!shared_commit.is_empty(), "repo A is a real git checkout");
    assert_ne!(repo_a_id, REPO_B, "the two repos have distinct ids");

    {
        let conn = db.storage.connection();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, 'B', 0)",
            [REPO_B],
        )
        .unwrap();
        // Repo B's live file at repo A's SAME commit (the fork case).
        seed_repo_b_file(conn, "src/b_only.rs", &shared_commit);
        // An identical (path, commit) DEAD-context row in each repo — gc of repo A must prune only
        // its own.
        seed_repo_b_file(conn, "src/dead.rs", DEAD_COMMIT);
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id)
             VALUES ('src/dead.rs', 'rust', 'source', 'asha', 0, 0, ?1, '', ?2)",
            rusqlite::params![DEAD_COMMIT, repo_a_id],
        )
        .unwrap();
    }

    TwoRepoFixture { db, root_a, repo_a_id, shared_commit }
}

/// Paths visible through the per-connection scope VIEW (`temp.files`) at the active scope.
fn paths_in_view(db: &IndexDatabase) -> Vec<String> {
    let mut stmt = db.storage.connection().prepare("SELECT path FROM files ORDER BY path").unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
}

fn repo_file_count(db: &IndexDatabase, repo_id: &str) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE repo_id = ?1", [repo_id], |r| r.get(0))
        .unwrap()
}

/// The scope view filters on `repo_id` FIRST: repo A's connection never sees repo B's file even
/// though they sit at the SAME commit, and re-scoping the same connection to repo B flips the view.
#[test]
fn scope_view_isolates_repos_sharing_a_commit() {
    let mut fx = two_repo_fixture();

    // Scoped to repo A (as rebuild left it): sees repo A's file, NOT repo B's — despite the shared
    // commit sha.
    let view_a = paths_in_view(&fx.db);
    assert!(view_a.contains(&"src/a_only.rs".to_string()), "repo A sees its own file: {view_a:?}");
    assert!(
        !view_a.contains(&"src/b_only.rs".to_string()),
        "repo A's view must NOT leak repo B's file at the shared commit: {view_a:?}"
    );

    // Re-scope the SAME connection to repo B (the A7 per-repo drive, done here by hand).
    fx.db.active_repo_id = REPO_B.to_string();
    fx.db.set_context(&fx.shared_commit, "").unwrap();
    let view_b = paths_in_view(&fx.db);
    assert!(view_b.contains(&"src/b_only.rs".to_string()), "repo B sees its own file: {view_b:?}");
    assert!(
        !view_b.contains(&"src/a_only.rs".to_string()),
        "repo B's view must NOT leak repo A's file: {view_b:?}"
    );

    let _ = fs::remove_dir_all(fx.root_a);
}

/// `garbage_collect` is repo-sliced: sweeping repo A prunes ONLY repo A's dead-context rows and
/// leaves every one of repo B's rows intact — including a row with the IDENTICAL `(path, commit)`
/// as the repo A row it prunes (row-count parity across the sweep).
#[test]
fn gc_of_one_repo_leaves_the_other_intact() {
    let fx = two_repo_fixture();

    let repo_b_before = repo_file_count(&fx.db, REPO_B);
    assert_eq!(repo_b_before, 2, "repo B seeded with a live + a dead file");
    // Repo A carries its indexed files plus the seeded dead-context row.
    let repo_a_dead_before: i64 = fx
        .db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE repo_id = ?1 AND commit_sha = ?2",
            rusqlite::params![fx.repo_a_id, DEAD_COMMIT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(repo_a_dead_before, 1, "repo A has its dead-context row before gc");

    let report = fx.db.garbage_collect().unwrap();
    assert!(!report.skipped, "a live context was determined, so gc ran");

    // Repo A's dead-context row is gone (its commit is not live).
    let repo_a_dead_after: i64 = fx
        .db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE repo_id = ?1 AND commit_sha = ?2",
            rusqlite::params![fx.repo_a_id, DEAD_COMMIT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(repo_a_dead_after, 0, "gc pruned repo A's dead-context row");

    // Repo B is untouched — EVERY row survives, including the one at the identical (path, commit)
    // as the repo A row that was just pruned. This is the per-repo gc slice.
    let repo_b_after = repo_file_count(&fx.db, REPO_B);
    assert_eq!(repo_b_after, repo_b_before, "gc of repo A deletes nothing of repo B");
    let repo_b_dead: i64 = fx
        .db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE repo_id = ?1 AND path = 'src/dead.rs'",
            [REPO_B],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        repo_b_dead, 1,
        "repo B's identical (path, commit) dead row is NOT pruned by A's gc"
    );

    // The REPORT is repo-scoped too (2nd adversary wave): `files_remaining`/`chunks_remaining`
    // count the ACTIVE repo's slice, never the whole consolidated store — a whole-table count
    // would include repo B's 2 files (and the poison sibling's), and a sibling's index pass
    // committing between the before/after reads would skew the derived pruned counts.
    let repo_a_files: i64 = fx
        .db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE repo_id = ?1", [&fx.repo_a_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        report.files_remaining,
        u64::try_from(repo_a_files).unwrap(),
        "gc reports repo A's file slice, not the whole-store union",
    );
    let repo_a_chunks: i64 = fx
        .db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.chunks c JOIN main.files f ON f.id = c.file_id WHERE \
             f.repo_id = ?1",
            [&fx.repo_a_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        report.chunks_remaining,
        u64::try_from(repo_a_chunks).unwrap(),
        "gc reports repo A's chunk slice, not the whole-store union",
    );

    let _ = fs::remove_dir_all(fx.root_a);
}

/// The all-zeros id: the lexicographically SMALLEST 40-hex string, so a real commit-hash repo id
/// always sorts after it — used to expose a `sole_repo_id` fallback picking the wrong repo.
const SMALLER_SIBLING: &str = "0000000000000000000000000000000000000000";

/// Count `edges_data` rows whose source file belongs to `repo_id` (edges are attributed to a repo
/// through `source_file_id → files.repo_id`).
fn repo_edge_count(db: &IndexDatabase, repo_id: &str) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM edges_data
              WHERE source_file_id IN (SELECT id FROM main.files WHERE repo_id = ?1)",
            [repo_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// Seed ONE `edges_data` row whose source file is repo B's live file, so a repo-scoped graph wipe
/// can be shown to spare it.
fn seed_repo_b_edge(conn: &rusqlite::Connection) {
    let file_id: i64 = conn
        .query_row(
            "SELECT id FROM main.files WHERE repo_id = ?1 AND path = 'src/b_only.rs'",
            [REPO_B],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute_batch(
        "INSERT OR IGNORE INTO name_strings(value)
             VALUES ('b_fn'), ('b_callee'), ('calls'), ('Exact');",
    )
    .unwrap();
    let id_of = |v: &str| -> i64 {
        conn.query_row("SELECT id FROM name_strings WHERE value = ?1", [v], |r| r.get(0)).unwrap()
    };
    conn.execute(
        "INSERT INTO edges_data(source_file_id, from_name_id, to_name_id, edge_kind_id, \
         confidence_id, resolution_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            file_id,
            id_of("b_fn"),
            id_of("b_callee"),
            id_of("calls"),
            id_of("Exact"),
            id_of("Exact"),
        ],
    )
    .unwrap();
}

/// #413 finding #3: `ensure_graph_index_current` wipes then repopulates edges. `graph_index_version`
/// is per-repo, so a stale version for repo A must wipe ONLY repo A's edges — a global
/// `DELETE FROM edges_data` would leave repo B's graph empty until its own forced rebuild.
#[test]
fn graph_refresh_of_one_repo_leaves_the_other_repos_edges() {
    let fx = two_repo_fixture();
    seed_repo_b_edge(fx.db.storage.connection());
    let repo_b_before = repo_edge_count(&fx.db, REPO_B);
    assert_eq!(repo_b_before, 1, "repo B has its seeded edge before the refresh");

    // Mark repo A's graph index stale, then refresh it (the connection is scoped to repo A).
    fx.db.set_repo_meta("graph_index_version", "0").unwrap();
    fx.db.ensure_graph_index_current().unwrap();

    assert_eq!(
        repo_edge_count(&fx.db, REPO_B),
        repo_b_before,
        "repo A's graph refresh must not wipe repo B's edges"
    );
    let _ = fs::remove_dir_all(fx.root_a);
}

/// #413 finding #4: the config-bearing incremental path (`index --changed`/`--discover`) must
/// register/adopt the config's repo BEFORE stamping rows, exactly like `open_config`/`rebuild` —
/// not ride the config-blind sole-repo fallback. With a lexicographically-SMALLER sibling repo
/// present, `sole_repo_id` would pick the sibling; adoption must correct the active scope to the
/// config's repo so the pass stamps rows under it, not the sibling.
#[test]
fn incremental_adopts_the_config_repo_over_a_smaller_sibling() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "init"]);

    let config = source_config(root.clone(), Language::Rust);
    let repo_id = IndexDatabase::rebuild(&config).unwrap().active_repo_id;
    assert!(SMALLER_SIBLING < repo_id.as_str(), "the sibling id sorts before the real repo id");

    // A sibling repo whose id sorts BEFORE repo A's, so the sole-repo fallback would pick it.
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, 'decoy', 0)",
            [SMALLER_SIBLING],
        )
        .unwrap();
    }

    // Edit a file so the incremental pass actually re-stamps a row, then run the config discover.
    fs::write(root.join("src/lib.rs"), "pub fn f() { let _ = 1; }\n").unwrap();
    let db = IndexDatabase::index_discover(&config).unwrap();
    assert_eq!(db.active_repo_id, repo_id, "incremental adopts the config's repo, not the sibling");
    let under_sibling: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE repo_id = ?1", [SMALLER_SIBLING], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(under_sibling, 0, "no rows stamped under the sibling repo");
    let under_config: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE repo_id = ?1", [&repo_id], |r| r.get(0))
        .unwrap();
    assert!(under_config > 0, "rows are stamped under the config's repo");
    let _ = fs::remove_dir_all(root);
}

/// #413 round-4 finding #1: the read-only MCP open (`try_open_config_read_only`) must scope to the
/// CONFIG's repo, not the config-blind sole repo. With a lexicographically-SMALLER sibling repo
/// present, `sole_repo_id` would pick the sibling and serve ITS (empty) scope; the read path now
/// resolves the config's identity instead, so it serves the config repo's rows.
#[test]
fn read_only_open_binds_the_config_repo_over_a_smaller_sibling() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn ro_sibling_anchor() {}\n").unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "init"]);

    let config = source_config(root.clone(), Language::Rust);
    let repo_id = IndexDatabase::rebuild(&config).unwrap().active_repo_id;
    assert!(SMALLER_SIBLING < repo_id.as_str(), "the sibling id sorts before the real repo id");

    // A sibling repos row whose id sorts BEFORE the config repo's, so the config-blind sole-repo
    // fallback would pick it.
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, 'decoy', 0)",
            [SMALLER_SIBLING],
        )
        .unwrap();
        assert_eq!(
            schema::sole_repo_id(&conn).unwrap(),
            SMALLER_SIBLING,
            "the config-blind sole pick is the WRONG (sibling) repo — the bug this fix closes",
        );
    }

    let ro = IndexDatabase::try_open_config_read_only(&config)
        .unwrap()
        .expect("a current index is served read-only");
    assert_eq!(
        ro.active_repo_id, repo_id,
        "the read-only open binds the CONFIG's repo (by identity), not the smaller sibling",
    );
    // And it actually serves the config repo's rows (not the sibling's empty scope).
    assert!(
        !ro.symbols("ro_sibling_anchor", Some(Language::Rust), 10).unwrap().is_empty(),
        "the read-only connection answers queries scoped to the config repo",
    );
    let _ = fs::remove_dir_all(root);
}

/// All `repo_meta` rows as a sorted `(repo_id, key, value)` list — the snapshot the full-ladder
/// replay must leave byte-identical (V039/V040 relocate meta, so a no-op replay must move nothing).
fn dump_repo_meta(conn: &rusqlite::Connection) -> Vec<(String, String, Option<String>)> {
    let mut stmt =
        conn.prepare("SELECT repo_id, key, value FROM repo_meta ORDER BY repo_id, key").unwrap();
    let mut rows: Vec<(String, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.sort();
    rows
}

/// #413 round-4 finding #3: a full rebuild replays the WHOLE migration ladder (`create_or_migrate` →
/// `schema::apply`). V039's per-repo-meta relocation resolves a migration-local `sole_repo_id` that
/// HARD-ERRORS on a >1-repo registry — so `rag-rat index --full` for any repo in a CONSOLIDATED DB
/// would fail before adoption. The idempotence gate makes the already-migrated relocation a true
/// no-op (it never resolves the sole repo), so the replay succeeds and moves nothing. This also
/// proves V038 (NOT-EXISTS seed guard) and V040 (files.repo_id sentinel) are replay-safe on the
/// same two-repo DB — the whole ladder runs without error.
#[test]
fn full_ladder_replay_on_a_two_repo_db_is_idempotent() {
    let fx = two_repo_fixture();
    let db_path = fx.db.database_path().to_path_buf();
    // Replay on a FRESH connection — exactly like `create_or_migrate` (which opens its own
    // `IndexConnection` before `schema::apply`). The fixture's own connection carries the
    // `temp.files` scope VIEW, which would shadow `main.files` for a migration's `CREATE INDEX ...
    // ON files`; production never applies the ladder through a scoped connection.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

    // Two real repos own the DB — the consolidated shape that trips the migration-local
    // sole_repo_id.
    assert!(schema::multiple_real_repos(&conn).unwrap(), "the fixture DB holds >1 real repo");
    let repo_meta_before = dump_repo_meta(&conn);
    let index_meta_before: i64 =
        conn.query_row("SELECT COUNT(*) FROM index_meta", [], |r| r.get(0)).unwrap();
    let files_before: i64 =
        conn.query_row("SELECT COUNT(*) FROM main.files", [], |r| r.get(0)).unwrap();

    // Replay the FULL ladder, exactly what `create_or_migrate` / `index --full` does. Before the
    // gate this HARD-ERRORS at V039 on the 2-repo registry.
    schema::apply(&conn).expect("full-ladder replay on a consolidated 2-repo DB must not error");

    // A no-op: no meta relocated, no rows resurrected in the source tables, no files moved.
    assert_eq!(dump_repo_meta(&conn), repo_meta_before, "the replay moved no repo_meta rows");
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM index_meta", [], |r| r.get::<_, i64>(0)).unwrap(),
        index_meta_before,
        "the replay resurrected no keys in index_meta",
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM main.files", [], |r| r.get::<_, i64>(0)).unwrap(),
        files_before,
        "the replay moved/duplicated no files rows",
    );
    // Both repos still registered.
    assert!(schema::multiple_real_repos(&conn).unwrap(), "both repos still registered post-replay");
    let repo_b_files: i64 = conn
        .query_row("SELECT COUNT(*) FROM main.files WHERE repo_id = ?1", [REPO_B], |r| r.get(0))
        .unwrap();
    assert_eq!(repo_b_files, 2, "repo B's rows are intact after the replay");
    let _ = fs::remove_dir_all(fx.root_a);
}

/// #413 finding #5 (now V042): `oracle_runs.repo_id` scopes the prune to the ACTIVE repo, so GC for
/// repo A leaves repo B's run rows intact whenever B's context is absent from A's live sets — the
/// per-repo predicate that superseded the old `multiple_real_repos` seam guard.
#[test]
fn gc_skips_the_global_oracle_prune_on_a_multi_repo_db() {
    let fx = two_repo_fixture();
    // A dead-context oracle run belonging to the SIBLING repo B (its commit/worktree are not in
    // repo A's live sets, and its `repo_id` is REPO_B). Repo A's gc runs a repo-scoped oracle prune
    // (V042 `oracle_runs.repo_id` predicate, superseding the old `multiple_real_repos` guard), so
    // it must spare repo B's run.
    fx.db
        .storage
        .connection()
        .execute(
            "INSERT INTO oracle_runs(tool, tool_version, commit_sha, worktree_id, started_at, \
             status, stats_json, repo_id)
             VALUES ('rust-analyzer', 'v1', 'dead-commit', 'dead-wt', 0, 'Completed', '{}', ?1)",
            [REPO_B],
        )
        .unwrap();

    let report = fx.db.garbage_collect().unwrap();
    assert!(!report.skipped, "a live context was determined, so gc ran");
    let surviving: i64 = fx
        .db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM oracle_runs WHERE commit_sha = 'dead-commit'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(surviving, 1, "repo A's repo-scoped oracle prune spares repo B's run");
    let _ = fs::remove_dir_all(fx.root_a);
}

/// The complement: the ACTIVE repo's OWN dead-context run IS pruned. The dead run is stamped the
/// active (adopted) repo id — exactly as `record_oracle_run` stamps it in production — so the V042
/// repo-scoped prune (`oracle_runs.repo_id = active`) reaches it.
#[test]
fn gc_prunes_a_dead_oracle_run_on_a_single_repo_db() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let active = db.active_repo_id.clone();
    db.storage
        .connection()
        .execute(
            "INSERT INTO oracle_runs(tool, tool_version, commit_sha, worktree_id, started_at, \
             status, stats_json, repo_id)
             VALUES ('rust-analyzer', 'v1', 'dead-commit', 'dead-wt', 0, 'Completed', '{}', ?1)",
            [&active],
        )
        .unwrap();

    db.garbage_collect().unwrap();
    let surviving: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM oracle_runs WHERE commit_sha = 'dead-commit'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(surviving, 0, "single-repo gc prunes the dead-context oracle run as before");
    let _ = fs::remove_dir_all(root);
}

/// `LogicalSymbolKey::stable_id` folds the OWNING repo into the content hash: two repos with
/// byte-identical content (the same language/path/name/qualified-name/kind/signature key) derive
/// DISTINCT logical-symbol ids, so `insert_logical_group` for the second repo inserts a second row
/// instead of colliding on the `logical_symbols.id` PK (the pre-fix failure: `id` is the scalar
/// `sym_<hex>` wire handle and the FK/PK target of members/monikers/memory bindings, so it must
/// stay globally unique — the repo dimension lives INSIDE the derivation, not in a composite PK).
#[test]
fn identical_content_in_two_repos_yields_distinct_logical_symbols() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::index::schema::apply(&conn).unwrap();

    let key = crate::index::graph_index::LogicalSymbolKey {
        language: "rust".to_string(),
        path: "src/lib.rs".to_string(),
        name: "parse".to_string(),
        qualified_name: "lib::parse".to_string(),
        kind: "function".to_string(),
        signature: Some("fn parse()".to_string()),
    };

    IndexDatabase::insert_logical_group(&conn, "repo-a", &key, &[])
        .expect("repo A inserts its group");
    IndexDatabase::insert_logical_group(&conn, "repo-b", &key, &[])
        .expect("the IDENTICAL key under repo B must not collide on the id PK");

    let rows: Vec<(i64, String)> = {
        let mut stmt =
            conn.prepare("SELECT id, repo_id FROM logical_symbols ORDER BY repo_id").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(rows.len(), 2, "one logical-symbol row per repo");
    assert_eq!(rows[0].1, "repo-a");
    assert_eq!(rows[1].1, "repo-b");
    assert_ne!(rows[0].0, rows[1].0, "identical content derives repo-distinct ids");
    // Per-repo resolution: each id is the deterministic repo-folded derivation, so a cached
    // `sym_<hex>` handle keeps resolving to ITS repo's row across reindexes.
    assert_eq!(rows[0].0, key.stable_id("repo-a"));
    assert_eq!(rows[1].0, key.stable_id("repo-b"));
}

/// #413 round-5: `repo_brief`'s `graph_edges` total is scoped to the ACTIVE repo. `edges` has no
/// `repo_id`, so a global `COUNT(*)` over the `edges` view would report the union across a
/// consolidated DB while every other brief count is repo-scoped. It must count via
/// `source_file_id → main.files.repo_id` instead.
#[test]
fn repo_brief_edges_are_scoped_to_the_active_repo() {
    let fx = two_repo_fixture();
    seed_repo_b_edge(fx.db.storage.connection());
    let conn = fx.db.storage.connection();

    let repo_a_edges = repo_edge_count(&fx.db, &fx.repo_a_id);
    let global_edges: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges_data", [], |r| r.get(0)).unwrap();
    assert!(
        global_edges > repo_a_edges,
        "the seeded repo B edge makes the global total exceed repo A's ({global_edges} > \
         {repo_a_edges})",
    );

    // The connection is scoped to repo A (as rebuild left it), so the brief counts repo A's edges.
    let summary = crate::query::repo_brief::summary_counts(conn).unwrap();
    assert_eq!(
        summary.graph_edges,
        u64::try_from(repo_a_edges).unwrap(),
        "repo_brief graph_edges counts only the active repo's edges, not repo B's",
    );
    assert!(
        summary.graph_edges < u64::try_from(global_edges).unwrap(),
        "and is strictly below the global union (repo B's edge is excluded)",
    );
    let _ = fs::remove_dir_all(fx.root_a);
}

/// #413 round-5: an INSTALLED-but-EMPTY scope context (`""`, the sibling-safe "match nothing" scope
/// raw read callers install when the config repo can't be proven) is AUTHORITATIVE for
/// `active_repo_id` — it must NOT fall through to `sole_repo_id`, which would let a direct-scoped
/// reader (git history, parser failures, `repo_meta`) serve a sibling repo while the file view is
/// empty.
#[test]
fn active_repo_id_honors_an_installed_empty_scope() {
    let fx = two_repo_fixture();
    let conn = fx.db.storage.connection();
    // A repo_meta row under repo A that a config-blind sole/active pick WOULD surface.
    crate::index::meta::set_repo_meta(conn, &fx.repo_a_id, "git_commit", "a-head").unwrap();

    // Install an EMPTY scope: overwrite the connection's repo_id context with "" (what
    // `install_worktree_scope_view(conn, "", ..)` writes when the repo is unprovable).
    conn.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', '')",
        [],
    )
    .unwrap();

    assert_eq!(
        schema::active_repo_id(conn).unwrap(),
        "",
        "an installed empty context is authoritative — never the sole/first repo",
    );
    // Therefore a direct-scoped reader reads NOTHING, not repo A's (or a sibling's) row.
    let scoped = schema::active_repo_id(conn).unwrap();
    assert_eq!(
        crate::index::meta::repo_meta(conn, &scoped, "git_commit").unwrap(),
        None,
        "installed-empty scope yields empty direct-scoped reads, never a sibling's rows",
    );
    let _ = fs::remove_dir_all(fx.root_a);
}

/// #413 round-5: `query::orientation` threads the config's `[index] repo_id` override into its
/// raw-connection scope resolution. A fork that PINS an id while sharing a root commit with its
/// upstream would otherwise mis-scope: with `override = None` the identity route derives the shared
/// root-commit id and binds the (registered) UPSTREAM sibling. With the override, the pinned fork
/// id resolves first, so orientation installs the fork's own scope.
#[test]
fn orientation_pins_the_fork_repo_over_a_shared_root_sibling() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn fork_anchor() {}\n").unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "init"]);

    // Index the fork under a PINNED id (records root → fork-pin).
    let mut config = source_config(root.clone(), Language::Rust);
    config.repo_id_override = Some("fork-pin".to_string());
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(db.active_repo_id, "fork-pin", "the fork indexes under its pinned id");

    // The portable id the shared root commit derives — the UPSTREAM sibling's id. Seed it directly
    // (register_repo refuses a second real repo before A7).
    let upstream_id = crate::repo_identity::resolve_repo_identity(&root, None).unwrap().repo_id;
    assert_ne!(upstream_id, "fork-pin", "the shared-root id differs from the fork's pin");
    let conn = db.storage.connection();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, 'upstream', 0)",
        [&upstream_id],
    )
    .unwrap();

    // WITH the override: orientation installs the fork's own scope.
    crate::query::orientation::orientation(conn, &root, &root, Some("fork-pin")).unwrap();
    assert_eq!(
        schema::active_repo_id(conn).unwrap(),
        "fork-pin",
        "orientation threads the pin and installs the fork's scope",
    );

    // WITHOUT the override: the identity route derives the shared-root UPSTREAM id, but this root
    // is RECORDED under the fork's pin — the read-only owner mirror (Codex batch 4) now DECLINES
    // instead of serving the sibling's scope, installing the deliberate empty "match nothing"
    // scope. That matches the write path, which refuses this exact shape with the
    // mismatched-root-owner remedy. (Pre-mirror, this phase pinned the sibling BIND as the
    // documented mis-scope the pin fixes; the mirror upgrades it from "wrong scope" to "no
    // scope + surfaced refusal on the write path".)
    crate::query::orientation::orientation(conn, &root, &root, None).unwrap();
    assert_eq!(
        schema::active_repo_id(conn).unwrap(),
        "",
        "no pin at a fork-owned root → the owner mirror declines rather than bind the sibling",
    );
    let _ = fs::remove_dir_all(root);
}

// --- Cross-repo FTS / papertrail leak matrix (spec §9, memory-sync phase A4) ---
//
// Repo A is really indexed (its chunk_fts / commit_fts / git_file_changes are populated by the
// rebuild); repo B's search rows are SEEDED directly under REPO_B with UNIQUELY-named content. The
// contract: every search / git-history / papertrail query run on repo A's scoped connection returns
// ONLY repo A's rows and NEVER surfaces repo B's — even though both live in the one database.

/// Repo B's unique tokens (seeded under REPO_B); a query for any of these from repo A's connection
/// must come back empty.
const B_LEXICAL_TOKEN: &str = "zebrafishunique";
const B_COMMIT_TOKEN: &str = "narwhalcommitunique";
const B_ISSUE_TOKEN: &str = "unicornissueunique";
/// Repo A's unique GitHub token (seeded under the real repo id) — the positive control on the same
/// papertrail surface.
const A_ISSUE_TOKEN: &str = "phoenixissueunique";

/// Seed a GitHub ref + issue + FTS row for `repo_id`, all carrying `token`.
fn seed_github_issue(
    conn: &rusqlite::Connection,
    repo_id: &str,
    owner: &str,
    repo: &str,
    number: i64,
    token: &str,
    source_path: &str,
) {
    conn.execute(
        "INSERT INTO github_refs(owner, repo, number, ref_kind, source_kind, source_path, \
         source_commit, source_text, discovered_at_ms, repo_id)
         VALUES (?1, ?2, ?3, 'closing', 'file', ?4, NULL, ?5, 0, ?6)",
        rusqlite::params![owner, repo, number, source_path, format!("{token} ref"), repo_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, \
         is_pull_request, synced_at_ms, repo_id)
         VALUES (?1, ?2, ?3, 'http://x', 'open', ?4, ?5, 0, 0, ?6)",
        rusqlite::params![
            owner,
            repo,
            number,
            format!("{token} title"),
            format!("{token} body"),
            repo_id
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
         VALUES (?1, ?2, ?3, 'issue', ?4, 'http://x', ?5, ?6, 'other', ?7)",
        rusqlite::params![
            owner,
            repo,
            number,
            number.to_string(),
            format!("{token} title"),
            format!("{token} body"),
            repo_id
        ],
    )
    .unwrap();
}

/// Seed repo B's search-surface rows (a chunk in chunk_fts, a commit in commit_fts, a
/// git_file_changes row, a github ref/issue/fts) under REPO_B, plus repo A's own github issue under
/// the real id (the positive control). Requires a `two_repo_fixture` (repo A indexed, repo B's file
/// rows already seeded).
fn seed_search_leak_data(fx: &TwoRepoFixture) {
    let conn = fx.db.storage.connection();

    // Repo B: a chunk whose text carries a unique token, wired into chunk_text + the contentless
    // chunk_fts, hung off repo B's live b_only.rs file row.
    let file_id: i64 = conn
        .query_row(
            "SELECT id FROM main.files WHERE repo_id = ?1 AND path = 'src/b_only.rs'",
            [REPO_B],
            |r| r.get(0),
        )
        .unwrap();
    let text = format!("fn b_fn() {{ /* {B_LEXICAL_TOKEN} */ }}");
    let chunk_id: i64 = conn
        .query_row(
            "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte, \
             start_line, end_line, text_hash)
             VALUES (?1, 'symbol', 'b_fn', 0, 10, 1, 5, 'bhash') RETURNING id",
            [file_id],
            |r| r.get(0),
        )
        .unwrap();
    crate::index::chunk_text_store::seed_chunk_text(conn, chunk_id, &text).unwrap();
    conn.execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", rusqlite::params![
        chunk_id, text
    ])
    .unwrap();

    // Repo B: a commit + file change under REPO_B, then re-point the external-content commit_fts
    // over EVERY repo's commits so repo B's commit IS in the FTS (the leak surface the join
    // must filter).
    conn.execute(
        "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, committed_at_s, \
         subject, body, changed_file_count, repo_id)
         VALUES ('bbbbb1111', 'b', 'b@e', 10, 10, ?1, '', 1, ?2)",
        rusqlite::params![format!("{B_COMMIT_TOKEN} subject"), REPO_B],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind, \
         repo_id)
         VALUES ('bbbbb1111', 'src/b_only.rs', 1, 0, 'modified', ?1)",
        [REPO_B],
    )
    .unwrap();
    crate::index::schema::rebuild_commit_fts(conn).unwrap();

    // Repo B's papertrail (must never leak) + repo A's own papertrail (the positive control).
    seed_github_issue(conn, REPO_B, "octob", "rb", 77, B_ISSUE_TOKEN, "src/b_only.rs");
    seed_github_issue(conn, &fx.repo_a_id, "octoa", "ra", 11, A_ISSUE_TOKEN, "src/a_only.rs");
}

/// Lexical + hybrid candidate selection is repo-scoped: repo B's uniquely-tokened chunk is
/// unreachable from repo A's connection (both the bm25 and the vector candidate pass JOIN the
/// repo-scoped `files` view, so a sibling repo's chunk is dropped before ranking), while repo A's
/// own chunk is still found.
#[test]
fn lexical_and_hybrid_search_never_surface_the_other_repo() {
    let fx = two_repo_fixture();
    seed_search_leak_data(&fx);
    let conn = fx.db.storage.connection();

    // Raw lexical (bm25 candidates through the scope view): repo B's token does not leak.
    let leaked =
        crate::search::lexical::search_lexical_only(conn, B_LEXICAL_TOKEN, 10, false).unwrap();
    assert!(leaked.is_empty(), "repo B chunk leaked into repo A lexical search: {leaked:?}");
    // Full hybrid entry (bm25 + vector fuse; no model ⇒ lexical-only, still through the view).
    let leaked_hybrid = fx.db.search(B_LEXICAL_TOKEN, 10, false).unwrap();
    assert!(leaked_hybrid.is_empty(), "repo B chunk leaked into repo A hybrid search");
    // Positive control: repo A's own indexed symbol IS reachable.
    let own = crate::search::lexical::search_lexical_only(conn, "a_only", 10, false).unwrap();
    assert!(
        own.iter().any(|hit| hit.path == "src/a_only.rs"),
        "repo A must still see its own chunk: {own:?}"
    );

    let _ = fs::remove_dir_all(fx.root_a);
}

/// Git-history queries are repo-scoped: repo B's commit (commit_fts MATCH) and its path history are
/// unreachable from repo A, and `status` counts only repo A's commits — repo A still sees its own.
#[test]
fn git_history_queries_never_surface_the_other_repo() {
    let fx = two_repo_fixture();
    seed_search_leak_data(&fx);
    let conn = fx.db.storage.connection();

    // commit_search joins commit_fts → git_commits and filters git_commits.repo_id.
    let leaked = crate::index::git_history::commit_search(conn, B_COMMIT_TOKEN, 10).unwrap();
    assert!(leaked.is_empty(), "repo B commit leaked into repo A commit_search: {leaked:?}");
    let own = crate::index::git_history::commit_search(conn, "init", 10).unwrap();
    assert!(!own.is_empty(), "repo A must still see its own commit");

    // history_for_path filters git_file_changes.repo_id.
    let leaked_hist =
        crate::index::git_history::history_for_path(conn, "src/b_only.rs", 10).unwrap();
    assert!(leaked_hist.is_empty(), "repo B path history leaked: {leaked_hist:?}");
    let own_hist = crate::index::git_history::history_for_path(conn, "src/a_only.rs", 10).unwrap();
    assert!(!own_hist.is_empty(), "repo A must still see its own path history");

    // status counts are per-repo: repo A has exactly its one indexed commit, not repo B's.
    let status = crate::index::git_history::status(conn, &fx.root_a).unwrap();
    assert_eq!(status.commit_count, 1, "status counts only repo A's git_commits, not repo B's");

    let _ = fs::remove_dir_all(fx.root_a);
}

/// Papertrail queries are repo-scoped: repo B's issue (github_fts MATCH) and its path-anchored ref
/// are unreachable from repo A, while repo A's own issue on the SAME surface is found.
#[test]
fn papertrail_queries_never_surface_the_other_repo() {
    let fx = two_repo_fixture();
    seed_search_leak_data(&fx);
    let conn = fx.db.storage.connection();

    // github_issue_search → search_fts, which filters github_fts.repo_id.
    let leaked = fx.db.github_issue_search(B_ISSUE_TOKEN, 10).unwrap();
    assert!(leaked.is_empty(), "repo B issue leaked into repo A github search: {leaked:?}");
    let own = fx.db.github_issue_search(A_ISSUE_TOKEN, 10).unwrap();
    assert!(!own.is_empty(), "repo A must still see its own issue");

    // refs_for_path filters github_refs.repo_id.
    let leaked_refs = crate::index::github::refs_for_path(conn, "src/b_only.rs", 10).unwrap();
    assert!(leaked_refs.is_empty(), "repo B github ref leaked: {leaked_refs:?}");
    let own_refs = crate::index::github::refs_for_path(conn, "src/a_only.rs", 10).unwrap();
    assert!(!own_refs.is_empty(), "repo A must still see its own github ref");

    let _ = fs::remove_dir_all(fx.root_a);
}

// ================================================================================================
// Phase A5 periphery scoping (clones / oracle / reconcile / memories).
//
// The A5 periphery-scoping migration is V042 (in the ladder), so a plain `schema::apply` scopes the
// periphery — these tests reach the scoped schema through the ladder, then seed two repos and
// switch the active repo through the scope-context row (the per-repo drive A7 will own). Every
// periphery query gates its `repo_id` predicate on `schema::periphery_repo_scope`, so a raw
// connection with no ladder applied runs unscoped (the pre-A5 behavior). The migration-shape
// assertions (fresh-tip, deferred-absence / rebuild in isolation, forward path) live in
// `repo_registry.rs`; these tests pin the cross-repo SCOPING BEHAVIOR of the query sweeps.
// ================================================================================================

use crate::query::memory::{
    RepoMemoryBindTarget, RepoMemoryCreate, RepoMemoryCreateResult, create_memory, memory_search,
};

const A5_REPO_A: &str = "a5-repo-a";
const A5_REPO_B: &str = "a5-repo-b";

/// A raw connection at the periphery-scoped schema (`schema::apply` runs the ladder through V042),
/// holding two real repos beside the `__unassigned__` placeholder V038 seeds.
fn a5_scoped_two_repo_conn() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    schema::apply(&conn).unwrap();
    for repo in [A5_REPO_A, A5_REPO_B] {
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
            [repo],
        )
        .unwrap();
    }
    conn
}

/// Point the connection's periphery scope at `repo_id` — the value `schema::active_repo_id` reads
/// (what A7 drives per repo). Mirrors `install_scope_view`'s `temp.connection_context` write.
fn a5_set_active_repo(conn: &rusqlite::Connection, repo_id: &str) {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
        [repo_id],
    )
    .unwrap();
}

/// Create a memory under the connection's active repo, bound to a commit (no `files` row needed).
fn a5_create_memory(
    conn: &rusqlite::Connection,
    title: &str,
    body: &str,
    commit: &str,
) -> RepoMemoryCreateResult {
    create_memory(conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: title.to_string(),
        body: body.to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget { commit_hash: Some(commit.to_string()), ..Default::default() },
    })
    .unwrap()
}

fn a5_repo_memory_count(conn: &rusqlite::Connection, repo_id: &str) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM repo_memories WHERE repo_id = ?1", [repo_id], |r| r.get(0))
        .unwrap()
}

/// An identically-titled memory created under each repo is a DISTINCT live memory, and
/// `memory_search` from one repo's scope never surfaces the other's — the cross-repo memory-search
/// leak guard (the FTS MATCH spans both repos, so the post-MATCH `repo_id` filter is what
/// isolates).
#[test]
fn identical_titled_memories_live_in_both_repos_and_search_isolates() {
    let conn = a5_scoped_two_repo_conn();

    a5_set_active_repo(&conn, A5_REPO_A);
    let a =
        a5_create_memory(&conn, "shared reentrancy invariant", "repo A body about widgets", "ca");
    assert!(!a.duplicate);

    a5_set_active_repo(&conn, A5_REPO_B);
    let b =
        a5_create_memory(&conn, "shared reentrancy invariant", "repo B body about widgets", "cb");
    assert!(!b.duplicate, "the same title under a DIFFERENT repo is not a duplicate");

    assert_eq!(a5_repo_memory_count(&conn, A5_REPO_A), 1, "repo A holds exactly its memory");
    assert_eq!(a5_repo_memory_count(&conn, A5_REPO_B), 1, "repo B holds exactly its memory");
    assert_ne!(a.memory.memory_id, b.memory.memory_id, "the two are distinct memories");

    a5_set_active_repo(&conn, A5_REPO_A);
    let hits_a = memory_search(&conn, "reentrancy", 10).unwrap();
    assert_eq!(hits_a.len(), 1, "repo A search returns exactly its own memory: {hits_a:?}");
    assert_eq!(hits_a[0].memory_id, a.memory.memory_id);

    a5_set_active_repo(&conn, A5_REPO_B);
    let hits_b = memory_search(&conn, "reentrancy", 10).unwrap();
    assert_eq!(hits_b.len(), 1, "repo B search returns exactly its own memory: {hits_b:?}");
    assert_eq!(hits_b[0].memory_id, b.memory.memory_id);
}

/// Dedupe NEVER crosses repos: identical title+body+binding is a duplicate WITHIN a repo, but the
/// same content under a sibling repo is a fresh memory.
#[test]
fn memory_dedupe_does_not_cross_repos() {
    let conn = a5_scoped_two_repo_conn();

    a5_set_active_repo(&conn, A5_REPO_A);
    let first = a5_create_memory(&conn, "dedupe title", "dedupe body", "sha-shared");
    assert!(!first.duplicate);
    let again = a5_create_memory(&conn, "dedupe title", "dedupe body", "sha-shared");
    assert!(again.duplicate, "same content in the SAME repo dedupes");
    assert_eq!(again.memory.memory_id, first.memory.memory_id);
    assert_eq!(a5_repo_memory_count(&conn, A5_REPO_A), 1);

    a5_set_active_repo(&conn, A5_REPO_B);
    let cross = a5_create_memory(&conn, "dedupe title", "dedupe body", "sha-shared");
    assert!(!cross.duplicate, "identical content in a DIFFERENT repo is never a duplicate");
    assert_ne!(cross.memory.memory_id, first.memory.memory_id);
    assert_eq!(a5_repo_memory_count(&conn, A5_REPO_B), 1);
}

fn a5_seed_oracle_run(conn: &rusqlite::Connection, repo_id: &str, commit: &str, worktree: &str) {
    conn.execute(
        "INSERT INTO oracle_runs(
             repo_id, tool, tool_version, commit_sha, worktree_id, started_at, status, stats_json)
         VALUES (?1, 'scip', 'v1', ?2, ?3, 0, 'ok', '{}')",
        rusqlite::params![repo_id, commit, worktree],
    )
    .unwrap();
}

fn a5_oracle_run_count(conn: &rusqlite::Connection, repo_id: &str) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM oracle_runs WHERE repo_id = ?1", [repo_id], |r| r.get(0))
        .unwrap()
}

/// `prune_oracle_runs_outside_scope` is per-repo: gc of repo A prunes only repo A's dead runs and
/// leaves every one of repo B's — even though repo B's `(commit, worktree)` is equally "not live"
/// from repo A's live set (the pre-A5 unscoped sweep would have deleted it too).
#[test]
fn oracle_run_prune_is_per_repo() {
    let conn = a5_scoped_two_repo_conn();
    a5_seed_oracle_run(&conn, A5_REPO_A, "a-dead", "a-wt");
    a5_seed_oracle_run(&conn, A5_REPO_B, "b-commit", "b-wt");

    a5_set_active_repo(&conn, A5_REPO_A);
    // Repo A's live set names neither run's (commit, worktree), so repo A's run is dead. The prune
    // is scoped to repo A, so repo B's run (equally not-live here) is untouched.
    let live_commits = vec!["a-live".to_string()];
    let live_worktrees = vec!["a-live-wt".to_string()];
    let deleted = crate::index::oracle::prune_oracle_runs_outside_scope(
        &conn,
        &live_commits,
        &live_worktrees,
    )
    .unwrap();
    assert_eq!(deleted, 1, "only repo A's dead run is pruned");
    assert_eq!(a5_oracle_run_count(&conn, A5_REPO_A), 0, "repo A's dead run is gone");
    assert_eq!(a5_oracle_run_count(&conn, A5_REPO_B), 1, "repo B's run survives repo A's prune");
}

/// A clone-graph precompute on repo A completes a fresh generation and GCs its OWN superseded
/// generations, but leaves a sibling repo's generation row intact — the per-repo generation sweep
/// (`complete_generation`'s scoped DELETE). The pre-A5 unscoped `DELETE WHERE generation != live`
/// would have wiped repo B's row.
#[test]
fn clone_precompute_leaves_sibling_repo_generation_untouched() {
    let mut fx = two_repo_fixture();
    {
        let conn = fx.db.storage.connection();
        // V042 already ran at OPEN time (before `install_scope_view`), so the periphery is scoped
        // here; `two_repo_fixture` left the connection on repo A's scope.
        // Clean slate: drop whatever generation the rebuild may have left and clear repo A's live
        // pointer so the precompute below is not skipped as already-current.
        conn.execute_batch("DELETE FROM clone_graph_generations;").unwrap();
        conn.execute(
            "DELETE FROM repo_meta WHERE repo_id = ?1 AND key = 'clone_graph_live_generation'",
            [fx.repo_a_id.as_str()],
        )
        .unwrap();
        // Seed a COMPLETE generation owned by repo B (a high number, so repo A's MAX+1 is
        // distinct).
        conn.execute(
            "INSERT INTO clone_graph_generations(
                 generation, status, theta_floor, normalizer_kind, normalizer_version,
                 source_revision, cursor_symbol_id, edges_written, postings_written, started_at_ms,
                 finished_at_ms, repo_id)
             VALUES (5000, 'Complete', 0.7, 'baseline', ?1, 'revB', 0, 0, 1, 0, 0, ?2)",
            rusqlite::params![crate::index::clones::NORM_VERSION, REPO_B],
        )
        .unwrap();
    }

    // Re-install the scope view (dropped for the migration) so the precompute reads repo A's scoped
    // symbols, and drive a REAL precompute on repo A (the active scope). It allocates gen 5001
    // (global MAX+1), completes it, and runs `complete_generation`'s scoped DELETE.
    fx.db.set_context(&fx.shared_commit, "").unwrap();
    fx.db.precompute_clone_graph(None).unwrap();

    let conn = fx.db.storage.connection();
    let repo_b_gen: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM clone_graph_generations WHERE repo_id = ?1 AND generation = 5000",
            [REPO_B],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(repo_b_gen, 1, "repo A's precompute must NOT delete repo B's generation");
    let repo_a_complete: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM clone_graph_generations WHERE repo_id = ?1 AND status = \
             'Complete'",
            [fx.repo_a_id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(repo_a_complete >= 1, "repo A published its own fresh Complete generation");

    let _ = fs::remove_dir_all(fx.root_a);
}

/// A5 finding 2: `refresh_clone_token_df` reads `symbol_fingerprints` scoped to the active repo
/// (joining `symbols` → `files.repo_id`, since the table is content-addressed with no `repo_id`),
/// so a consolidated DB's SIBLING repo's fingerprints never pool their token frequencies into THIS
/// repo's df — which would inflate document frequencies and reorder SourcererCC candidate
/// selection.
#[test]
fn clone_token_df_recompute_excludes_a_sibling_repos_fingerprints() {
    // A sentinel token far outside any real `FNV-1a(token)` the fixture produces.
    const SENTINEL_TOKEN: i64 = 9_900_000_333;
    let fx = two_repo_fixture();
    {
        let conn = fx.db.storage.connection();
        // Hang a symbol + a baseline fingerprint (carrying the sentinel token) off repo B's live
        // file. An UNSCOPED df recompute would pool the sentinel into repo A's df.
        let file_id: i64 = conn
            .query_row(
                "SELECT id FROM main.files WHERE repo_id = ?1 AND path = 'src/b_only.rs'",
                [REPO_B],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
             end_byte, start_line, end_line, is_test)
             VALUES (?1, 'rust', 'b_fn', NULL, 'function', 0, 0, 0, 0, 0)",
            [file_id],
        )
        .unwrap();
        let symbol_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO symbol_fingerprints(symbol_id, normalizer_kind, normalizer_version, \
             oracle_run_id, struct_hash, token_len, token_bag, created_at_ms)
             VALUES (?1, 'baseline', 1, NULL, 'bstruct', 1, ?2, 0)",
            rusqlite::params![
                symbol_id,
                crate::index::clones::bag_blob::encode_token_bag(&[(SENTINEL_TOKEN, 1)])
            ],
        )
        .unwrap();
    }

    // Recompute df scoped to repo A (as `two_repo_fixture` left the connection). The sibling's
    // fingerprint must NOT pool in.
    fx.db.refresh_clone_token_df().unwrap();
    let conn = fx.db.storage.connection();
    let leaked: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM clone_token_df WHERE token_hash = ?1 AND repo_id = ?2",
            rusqlite::params![SENTINEL_TOKEN, fx.repo_a_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        leaked, 0,
        "a sibling repo's fingerprint token leaked into the active repo's clone_token_df",
    );

    let _ = fs::remove_dir_all(fx.root_a);
}

/// A5 finding: `memory_by_id` is the guard for `memory_get` / `update_memory` / `mark_obsolete` /
/// `rebind_memory`. Scoped to the active repo, so a caller holding a SIBLING repo's memory id can
/// neither READ nor MUTATE it — the by-id lookup returns `None` (surfaced as "not found") and the
/// sibling memory is left untouched.
#[test]
fn memory_by_id_read_and_mutations_refuse_a_sibling_repos_memory() {
    use crate::query::memory::{
        RepoMemoryUpdate, mark_obsolete, memory_by_id, rebind_memory, update_memory,
    };
    let conn = a5_scoped_two_repo_conn();

    a5_set_active_repo(&conn, A5_REPO_A);
    let a = a5_create_memory(&conn, "repo A only", "body a", "ca");

    // Switch to repo B and try to reach repo A's memory by its id.
    a5_set_active_repo(&conn, A5_REPO_B);
    assert!(
        memory_by_id(&conn, &a.memory.memory_id).unwrap().is_none(),
        "memory_get must not read a sibling repo's memory by id",
    );
    assert!(
        update_memory(&conn, RepoMemoryUpdate {
            memory_id: a.memory.memory_id.clone(),
            kind: None,
            title: Some("hijacked".to_string()),
            body: None,
            confidence: None,
            status: None,
            tags: None,
        })
        .is_err(),
        "update_memory must refuse a sibling repo's memory id",
    );
    assert!(
        mark_obsolete(&conn, &a.memory.memory_id).is_err(),
        "mark_obsolete must refuse a sibling repo's memory id",
    );
    assert!(
        rebind_memory(&conn, &a.memory.memory_id, RepoMemoryBindTarget {
            commit_hash: Some("cx".to_string()),
            ..Default::default()
        })
        .is_err(),
        "rebind_memory must refuse a sibling repo's memory id",
    );

    // Repo A's memory is untouched: back under repo A it is still active with its original title.
    a5_set_active_repo(&conn, A5_REPO_A);
    let still = memory_by_id(&conn, &a.memory.memory_id).unwrap().expect("repo A still owns it");
    assert_eq!(still.status, "active");
    assert_eq!(still.title, "repo A only", "a sibling-scoped update must not have landed");
}

/// A5 finding: `rebind_memory` deletes + re-inserts bindings; the re-inserted rows must inherit the
/// PARENT memory's `repo_id` (not strand at the `__unassigned__` placeholder), or they drop out of
/// the binding-scoped sweeps while the parent memory stays in its repo.
#[test]
fn rebind_keeps_bindings_on_the_parent_memorys_repo() {
    use crate::query::memory::rebind_memory;
    let conn = a5_scoped_two_repo_conn();
    a5_set_active_repo(&conn, A5_REPO_A);
    let a = a5_create_memory(&conn, "rebind me", "body", "c-old");

    rebind_memory(&conn, &a.memory.memory_id, RepoMemoryBindTarget {
        commit_hash: Some("c-new".to_string()),
        ..Default::default()
    })
    .unwrap();

    let stranded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM repo_memory_bindings WHERE memory_id = ?1 AND repo_id != ?2",
            rusqlite::params![a.memory.memory_id, A5_REPO_A],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stranded, 0, "every rebound binding must carry the parent memory's repo_id");
    let owned: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM repo_memory_bindings WHERE memory_id = ?1 AND repo_id = ?2",
            rusqlite::params![a.memory.memory_id, A5_REPO_A],
            |r| r.get(0),
        )
        .unwrap();
    assert!(owned >= 1, "the rebound commit binding is stamped under repo A");
}

/// A5 finding: `resolve_moniker` scans `logical_symbol_monikers` and joins `logical_symbols`. BOTH
/// sides are scoped, so a SIBLING repo carrying the SAME SCIP moniker string cannot capture this
/// repo's re-resolution — turning a unique match ambiguous, or relocating onto the sibling symbol.
#[test]
fn resolve_moniker_ignores_a_sibling_repos_moniker_row() {
    use crate::query::memory::{MonikerResolution, resolve_moniker};
    let conn = a5_scoped_two_repo_conn();

    // The same moniker string "M" under the same tool in BOTH repos, each with a live logical
    // symbol (`logical_symbol_id` is content-derived + repo-folded, so the two ids differ).
    for (lsid, repo) in [(111_i64, A5_REPO_A), (222_i64, A5_REPO_B)] {
        conn.execute(
            "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, \
             kind, variant_count, group_reason, repo_id)
             VALUES (?1, 'rust', 'src/x.rs', 'x', NULL, 'function', 1, 'g', ?2)",
            rusqlite::params![lsid, repo],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO logical_symbol_monikers(logical_symbol_id, tool, tool_version, moniker, \
             computed_at, repo_id)
             VALUES (?1, 'scip-rust', 'v1', 'M', 0, ?2)",
            rusqlite::params![lsid, repo],
        )
        .unwrap();
    }

    a5_set_active_repo(&conn, A5_REPO_A);
    match resolve_moniker(&conn, "M", "scip-rust").unwrap() {
        MonikerResolution::Unique { logical_symbol_id, .. } => assert_eq!(
            logical_symbol_id, 111,
            "moniker resolution must bind repo A's symbol, not the sibling's",
        ),
        other => panic!(
            "expected a unique repo-A resolution, got {other:?} (a sibling leak reads as \
             ambiguous)"
        ),
    }
}

/// A5 finding: `dream_run`'s lifecycle (lookup / supersede / resolve / list) is scoped, so a run in
/// one repo never resolves or emits a SIBLING repo's worklist rows on a consolidated DB.
#[test]
fn dream_run_leaves_a_sibling_repos_findings_untouched() {
    use crate::dream::{DreamOptions, dream_run};
    let conn = a5_scoped_two_repo_conn();

    // An OPEN finding owned by each repo.
    for (id, subject, repo) in
        [("a-open", "a::subject", A5_REPO_A), ("b-open", "b::subject", A5_REPO_B)]
    {
        conn.execute(
            "INSERT INTO dream_findings(id, kind, subject, claim_hash, evidence, base_rank, \
             status, first_seen_at_ms, last_seen_at_ms, repo_id)
             VALUES (?1, 'coverage_gap', ?2, 'ch', 'ev', 1.0, 'open', 0, 0, ?3)",
            rusqlite::params![id, subject, repo],
        )
        .unwrap();
    }

    // Run dream on repo A. Its (empty) index reports nothing this run, so the resolve pass resolves
    // repo A's own unreported finding — but repo B's finding must be left OPEN and unemitted.
    a5_set_active_repo(&conn, A5_REPO_A);
    let report =
        dream_run(&conn, DreamOptions { limit: 50, now_ms: 1_000, verify: false }).unwrap();

    let a_status: String = conn
        .query_row("SELECT status FROM dream_findings WHERE id = 'a-open'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(a_status, "resolved", "repo A's own unreported finding is resolved");
    let b_status: String = conn
        .query_row("SELECT status FROM dream_findings WHERE id = 'b-open'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(b_status, "open", "repo A's dream run must NOT resolve repo B's finding");
    assert!(
        !report.findings.iter().any(|f| f.subject == "b::subject"),
        "the emitted worklist must not include a sibling repo's finding",
    );
}

/// A5 finding: the dream CANDIDATE BUILDERS are repo-scoped, not just the lifecycle. A sibling
/// repo's path-bound memory at a path this repo also has must NOT suppress this repo's
/// coverage-gap finding (a false negative via the unscoped `repo_memory_bindings` exclusion
/// subquery), and a sibling repo's memory referencing a gone path must NOT surface as this repo's
/// stale_reference finding.
#[test]
fn dream_candidate_builders_ignore_a_sibling_repos_memories() {
    use crate::dream::{DreamOptions, dream_run};
    let conn = a5_scoped_two_repo_conn();

    // Repo A: a load-bearing symbol (one caller edge) in `src/shared.rs`, with NO memory of its
    // own — the coverage-gap candidate.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id)
         VALUES ('src/shared.rs', 'rust', 'source', 'asha', 0, 0, '', '', ?1)",
        [A5_REPO_A],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    let mut symbol_ids = Vec::new();
    for name in ["shared_fn", "caller_fn"] {
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
             end_byte, start_line, end_line, is_test)
             VALUES (?1, 'rust', ?2, NULL, 'function', 0, 0, 0, 0, 0)",
            rusqlite::params![file_id, name],
        )
        .unwrap();
        symbol_ids.push(conn.last_insert_rowid());
    }
    conn.execute_batch(
        "INSERT OR IGNORE INTO name_strings(value)
             VALUES ('caller_fn'), ('shared_fn'), ('calls_name'), ('Exact');",
    )
    .unwrap();
    let name_id = |value: &str| -> i64 {
        conn.query_row("SELECT id FROM name_strings WHERE value = ?1", [value], |r| r.get(0))
            .unwrap()
    };
    conn.execute(
        "INSERT INTO edges_data(source_file_id, from_symbol_id, to_symbol_id, from_name_id, \
         to_name_id, edge_kind_id, confidence_id, resolution_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        rusqlite::params![
            file_id,
            symbol_ids[1],
            symbol_ids[0],
            name_id("caller_fn"),
            name_id("shared_fn"),
            name_id("calls_name"),
            name_id("Exact"),
        ],
    )
    .unwrap();

    // Repo B: a memory whose PATH BINDING collides with repo A's file (the suppression vector) and
    // whose body references a path that resolves nowhere (the stale_reference vector).
    conn.execute(
        "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
         updated_at_ms, source, memory_version, repo_id)
         VALUES ('bmem', 'Invariant', 'b title', 'refs crates/ghost/src/vanished.rs', 'high', \
         'active', 0, 0, 'agent', 'v1', ?1)",
        [A5_REPO_B],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id)
         VALUES ('bmem', 'path', 'b-bind', 'src/shared.rs', 'current', 0, ?1)",
        [A5_REPO_B],
    )
    .unwrap();

    a5_set_active_repo(&conn, A5_REPO_A);
    let report =
        dream_run(&conn, DreamOptions { limit: 10, now_ms: 1_000, verify: false }).unwrap();

    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == "coverage_gap" && f.subject == "src/shared.rs::shared_fn"),
        "a sibling repo's same-path memory binding must NOT suppress the active repo's \
         coverage-gap finding: {:?}",
        report.findings,
    );
    assert!(
        !report.findings.iter().any(|f| f.kind == "stale_reference" && f.subject == "bmem"),
        "a sibling repo's memory must NOT surface as the active repo's stale_reference: {:?}",
        report.findings,
    );
}

/// Dream v2 pass 0 (poison-sibling discipline): the verification QUEUE — a new consuming surface —
/// must never surface a sibling repo's memories, and its `memory_reality` (V046) churn-skip read
/// must be repo-scoped (per the V042 gating pattern), so a sibling's verified row cannot suppress
/// this repo's memory.
#[test]
fn verification_queue_never_surfaces_a_sibling_repos_memories() {
    use crate::dream::verification_queue;
    let conn = a5_scoped_two_repo_conn();

    // An active memory in EACH repo, both anchor-broken (a gone binding) so both are
    // enqueue-eligible.
    for (id, repo) in [("a_mem", A5_REPO_A), ("b_mem", A5_REPO_B)] {
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id) VALUES \
             (?1,'Invariant','t','prose',?2,'active','agent',1,1,'agent','v1',?3)",
            rusqlite::params![id, "high", repo],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, anchor_status, \
             created_at_ms, repo_id) VALUES (?1,'symbol','foo','gone',0,?2)",
            rusqlite::params![id, repo],
        )
        .unwrap();
    }
    // The sibling repo's memory is ALREADY verified (a matching memory_reality row) — an unscoped
    // reality read would wrongly let repo A's run see it; a scoped read must ignore it.
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, body_hash, checked_at_ms) VALUES \
         ('b_mem', ?1, 'bh', 0)",
        [A5_REPO_B],
    )
    .unwrap();

    a5_set_active_repo(&conn, A5_REPO_A);
    let queue = verification_queue(&conn, 1_000, 50).unwrap();
    let ids: Vec<&str> = queue.iter().map(|e| e.memory_id.as_str()).collect();
    assert_eq!(ids, vec!["a_mem"], "the queue holds ONLY the active repo's memory: {ids:?}");
}

/// Dream v2 pass 0 (poison-sibling discipline): the EVIDENCE PACK — a new consuming surface —
/// resolves identifiers and excerpts through the repo-scoped `files` view, so a sibling repo's
/// same-path file / same-name symbol never leaks into the active repo's pack, and a
/// sibling-exclusive symbol resolves to the authoritative NOT FOUND.
#[test]
fn evidence_pack_never_surfaces_a_sibling_repos_symbols_or_files() {
    use crate::dream::evidence_pack;
    let conn = a5_scoped_two_repo_conn();
    let commit = "cafecafecafecafecafecafecafecafecafecafe";

    // Seed a file + one symbol + one chunk under `repo`, at the shared `commit`. Returns nothing;
    // the shared path/commit prove isolation is by repo_id, not path or commit.
    let seed = |repo: &str, symbol: &str, chunk_text: &str| {
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id) VALUES \
             ('src/shared.rs','rust','source',?1,0,0,?2,'',?3)",
            rusqlite::params![format!("sha-{repo}"), commit, repo],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust',?2,'function',0,0)",
            rusqlite::params![file_id, symbol],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
             text_hash) VALUES (?1,'code',0,0,1,1,'th')",
            [file_id],
        )
        .unwrap();
        let chunk_id = conn.last_insert_rowid();
        crate::index::chunk_text_store::seed_chunk_text(&conn, chunk_id, chunk_text).unwrap();
    };
    // Repo A: shared symbol `target_symbol`, chunk carries a repo-A marker.
    seed(A5_REPO_A, "target_symbol", "fn target_symbol() { REPO_A_MARKER }");
    // Repo B (sibling), SAME path + commit: `sibling_only_symbol` exists ONLY here + a repo-B
    // marker.
    seed(A5_REPO_B, "sibling_only_symbol", "fn sibling_only_symbol() { REPO_B_MARKER }");

    // Repo A's memory names both symbols and is bound to the shared path.
    conn.execute(
        "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
         created_at_ms, updated_at_ms, source, memory_version, repo_id) VALUES \
         ('a_mem','Invariant','t','refs `target_symbol` and \
         `sibling_only_symbol`','high','active','agent',1,1,'agent','v1',?1)",
        [A5_REPO_A],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('a_mem','path','src/shared.rs','src/shared.rs','current',0,?1)",
        [A5_REPO_A],
    )
    .unwrap();
    // A sibling memory that the active repo's pack must never surface.
    conn.execute(
        "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
         created_at_ms, updated_at_ms, source, memory_version, repo_id) VALUES \
         ('b_mem','Invariant','t','sibling note','high','active','agent',1,1,'agent','v1',?1)",
        [A5_REPO_B],
    )
    .unwrap();

    // Scope to repo A and install its `files` view at the shared commit (the isolation mechanism).
    a5_set_active_repo(&conn, A5_REPO_A);
    crate::index::lifecycle::install_scope_view(&conn, commit, "").unwrap();

    let pack = evidence_pack(&conn, "a_mem").unwrap();
    let resolution = |ident: &str| {
        pack.identifiers.iter().find(|i| i.identifier == ident).map(|i| i.resolution.as_str())
    };
    assert_eq!(
        resolution("target_symbol"),
        Some("symbol src/shared.rs::target_symbol"),
        "the active repo's own symbol resolves"
    );
    assert_eq!(
        resolution("sibling_only_symbol"),
        Some("NOT FOUND anywhere in the source tree"),
        "a symbol that exists ONLY in the sibling repo must NOT resolve — the resolution is \
         repo-scoped through the files view",
    );
    let excerpt_text: String = pack.excerpts.iter().map(|e| e.text.as_str()).collect();
    assert!(excerpt_text.contains("REPO_A_MARKER"), "the excerpt is the active repo's file text");
    assert!(
        !excerpt_text.contains("REPO_B_MARKER"),
        "the same-path sibling file must NOT leak into the active repo's excerpt: {excerpt_text}",
    );

    // The sibling's own memory is invisible to a pack built under the active repo's scope.
    let sibling_pack = evidence_pack(&conn, "b_mem").unwrap();
    assert!(
        sibling_pack.identifiers.is_empty() && sibling_pack.excerpts.is_empty(),
        "evidence_pack must return an empty pack for a memory outside the active repo scope",
    );
}

/// Dream v2 pass 2 (poison-sibling discipline): the summary + verdict READ-JOIN that the
/// `surface = "summary"` view uses (`current_summary_and_verdict`) must be repo-scoped. Both repos
/// carry a `memory_summaries` + `memory_reality` row under the SAME (memory_id, body_hash) — a
/// read that forgot its `repo_id` predicate would surface the wrong repo's summary/verdict. The
/// scoped read must return each repo's OWN row.
#[test]
fn summary_and_verdict_read_join_never_surfaces_a_sibling_repos_row() {
    use crate::query::memory::current_summary_and_verdict;
    let conn = a5_scoped_two_repo_conn();
    let body = "a body shared by both repos' notes";
    let body_hash = crate::index::hex_sha256(body.as_bytes());

    // Same (memory_id, body_hash) under BOTH repos — distinct summary text + verdict per repo.
    for (repo, summary, verdict) in [
        (A5_REPO_A, "repo A compacted summary", "diverged"),
        (A5_REPO_B, "repo B compacted summary", "current"),
    ] {
        conn.execute(
            "INSERT INTO memory_summaries(memory_id, repo_id, body_hash, summary, prompt_version, \
             generated_at_ms) VALUES ('shared_mem', ?1, ?2, ?3, ?4, 0)",
            rusqlite::params![repo, body_hash, summary, crate::dream::COMPACT_PROMPT_VERSION],
        )
        .unwrap();
        // Stamp the current evidence hash (empty value — `shared_mem` has no bindings/identifiers)
        // and the current verdict prompt version so the hydrator's stale gates show the marker; the
        // point of THIS test is repo scoping.
        let inputs =
            crate::dream::checked_inputs_hash(&conn, "shared_mem", &Some(repo.to_string()))
                .unwrap();
        conn.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, body_hash, verdict, \
             checked_inputs_hash, prompt_version, checked_at_ms) VALUES ('shared_mem', ?1, ?2, \
             ?3, ?4, ?5, 0)",
            rusqlite::params![
                repo,
                body_hash,
                verdict,
                inputs,
                crate::dream::VERDICT_PROMPT_VERSION
            ],
        )
        .unwrap();
    }

    a5_set_active_repo(&conn, A5_REPO_A);
    let (summary_a, verdict_a) = current_summary_and_verdict(&conn, "shared_mem", body).unwrap();
    assert_eq!(
        summary_a.as_deref(),
        Some("repo A compacted summary"),
        "repo A reads its OWN summary"
    );
    assert_eq!(verdict_a.as_deref(), Some("[verdict: diverged]"), "repo A reads its OWN verdict");

    a5_set_active_repo(&conn, A5_REPO_B);
    let (summary_b, verdict_b) = current_summary_and_verdict(&conn, "shared_mem", body).unwrap();
    assert_eq!(
        summary_b.as_deref(),
        Some("repo B compacted summary"),
        "repo B reads its OWN summary"
    );
    assert!(
        verdict_b.as_deref().is_some_and(|v| v.starts_with("[verdict: current")),
        "repo B reads its OWN verdict, not repo A's diverged: {verdict_b:?}"
    );
}

/// A5 finding: `memory_id` folds the owning repo into its hash suffix, so two repos creating
/// IDENTICAL content in the SAME millisecond derive distinct ids (the repo-scoped dedupe correctly
/// passes both — a repo-blind id would explode on the global PK). Pre-A5 (`None` scope) keeps the
/// original derivation. Memory ids stay globally unique and coordination-free either way — folding
/// the repo INTO the hash strengthens that (phase B replication relies on it).
#[test]
fn memory_ids_fold_the_repo_so_same_millisecond_identical_content_cannot_collide() {
    use crate::query::memory::memory_id;
    let input_hash = "0123456789abcdef0123456789abcdef";
    let a = memory_id(1_000, input_hash, &Some(A5_REPO_A.to_string()));
    let b = memory_id(1_000, input_hash, &Some(A5_REPO_B.to_string()));
    assert_ne!(a, b, "same content + same millisecond under two repos derive DISTINCT ids");
    assert!(a.starts_with("mem_3e8_") && b.starts_with("mem_3e8_"), "id shape unchanged: {a} {b}");
    assert_eq!(
        memory_id(1_000, input_hash, &None),
        format!("mem_3e8_{}", &input_hash[..12]),
        "the pre-A5 (unscoped) derivation is byte-identical to the original",
    );
}

/// A5 finding: `reconcile_attempts` is a global append-only log; the `last_reconcile` status read
/// must scope by `repo_id`, or a sibling repo's NEWER attempt is reported as this repo's status.
#[test]
fn reconcile_status_ignores_a_sibling_repos_attempt() {
    let fx = two_repo_fixture();
    {
        let conn = fx.db.storage.connection();
        // Repo A's attempt (older) + repo B's attempt (NEWER — wins an unscoped ORDER BY).
        conn.execute(
            "INSERT INTO reconcile_attempts(started_at_ms, status, batch_size, repo_id)
             VALUES (100, 'Ok', 8, ?1)",
            [fx.repo_a_id.as_str()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO reconcile_attempts(started_at_ms, status, batch_size, repo_id)
             VALUES (999, 'Blocked', 8, ?1)",
            [REPO_B],
        )
        .unwrap();
    }
    let status =
        fx.db.llm_status().unwrap().last_reconcile.expect("repo A has a reconcile attempt");
    assert_eq!(status.status, "Ok", "the status must be repo A's, not repo B's newer attempt");
    assert_eq!(status.started_at_ms, 100);
    let _ = fs::remove_dir_all(fx.root_a);
}

/// A5 finding: the raw memory-summary readers (`repo_brief::memory_counts` /
/// `memory_counts_by_path`, `orientation`'s `active_non_dir_memory_*`, `tree::dir_memory_titles`)
/// bypass the scoped memory API. Scoped now, so a sibling repo's memory — even one whose path or
/// dir collides with ours — never inflates this repo's brief or shows in its orientation / tree.
#[test]
fn memory_summary_readers_exclude_a_sibling_repos_memory() {
    let fx = two_repo_fixture();
    {
        let conn = fx.db.storage.connection();
        // Repo B's ACTIVE memory: a path binding at repo A's REAL file path (collision), plus a
        // ROOT `dir` binding (so `tree`'s `root_memory_title` would leak it if unscoped).
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, repo_id)
             VALUES ('bmem', 'Invariant', 'REPO B SECRET', 'b body', 'high', 'active', 0, 0, \
             'agent', 'v1', ?1)",
            [REPO_B],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id)
             VALUES ('bmem', 'path', 'b-path', 'src/a_only.rs', 'current', 0, ?1)",
            [REPO_B],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id)
             VALUES ('bmem', 'dir', '', NULL, 'current', 0, ?1)",
            [REPO_B],
        )
        .unwrap();
    }
    let conn = fx.db.storage.connection();

    // (a) repo_brief: the summary + per-path memory counts exclude the sibling.
    let brief = fx
        .db
        .repo_brief(crate::query::repo_brief::RepoBriefOptions {
            mode: crate::query::repo_brief::RepoBriefMode::Spine,
            limit: 50,
            include_generated: true,
            include_memories: true,
        })
        .unwrap();
    assert_eq!(
        brief.summary.repo_memories.active, 0,
        "repo_brief summary counted a sibling repo's memory",
    );
    if let Some(candidate) = brief.candidates.iter().find(|c| c.path == "src/a_only.rs") {
        assert_eq!(
            candidate.metrics.memories.active, 0,
            "per-path memory count leaked the sibling at the shared path src/a_only.rs",
        );
    }

    // (b) tree: the root dir-memory title is not the sibling's.
    let tree =
        crate::query::tree::dir_tree(conn, &crate::query::tree::TreeOpts::default()).unwrap();
    assert_ne!(
        tree.root_memory_title.as_deref(),
        Some("REPO B SECRET"),
        "dir_memory_titles leaked a sibling repo's root dir memory",
    );

    // (c) orientation: the active non-dir memory titles do not include the sibling. (Runs last —
    // it re-installs the scope view on the connection.)
    let orient =
        crate::query::orientation::orientation(conn, &fx.root_a, &fx.root_a, None).unwrap();
    assert!(
        !orient.active_memory_titles.iter().any(|t| t == "REPO B SECRET"),
        "orientation leaked a sibling repo's active memory title",
    );

    let _ = fs::remove_dir_all(fx.root_a);
}

/// A7 (Codex batch 2 P2): the open-time model-manifest heal must never mutate a SIBLING repo's
/// `repo_meta`. Pre-fix, the incremental pass's low-level open ran `ensure_model_manifest` BEFORE
/// adoption, so its `active_repo_id` resolved the config-less sole pick — the lexicographically
/// FIRST repo — and a heal-owed pass (`remove_legacy_models`) DELETED that repo's active-model
/// meta while the command held only its own repo's lock. Post-fix the heal is deferred until after
/// adopt + set_context, so it reads/clears only the config's own repo.
#[test]
fn incremental_open_heal_leaves_a_sibling_repos_model_meta_alone() {
    let fx = two_repo_fixture();
    let conn = fx.db.storage.connection();

    // A sibling registered under an id that sorts FIRST (before any hex-derived id), carrying a
    // LEGACY active-model value — exactly the row the pre-adoption sole-pick heal would delete.
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('0-first-sibling', \
         's', 0)",
        [],
    )
    .unwrap();
    crate::index::meta::set_repo_meta(
        conn,
        "0-first-sibling",
        "active_embedding_model",
        "fastembed-all-minilm-l6-v2",
    )
    .unwrap();
    // The fixture's DB is a shared temp file, not root_a/.rag-rat — reuse its path for the config.
    let db_path = fx.db.database_path().to_path_buf();
    drop(fx.db);

    // A production incremental pass on repo A (the config's repo).
    let mut config = source_config(fx.root_a.clone(), Language::Rust);
    config.database = db_path;
    let db = IndexDatabase::index_changed(&config).unwrap();

    // The sibling's legacy meta row SURVIVES: the heal ran scoped to repo A, never the sole pick.
    let sibling_meta = crate::index::meta::repo_meta(
        db.storage.connection(),
        "0-first-sibling",
        "active_embedding_model",
    )
    .unwrap();
    assert_eq!(
        sibling_meta.as_deref(),
        Some("fastembed-all-minilm-l6-v2"),
        "the open-time manifest heal mutated a sibling repo's repo_meta (pre-adoption sole pick)",
    );

    let _ = fs::remove_dir_all(fx.root_a);
}

/// A7 (Codex batch 3, same class as the incremental finding — the CALLEE now enforces it): a
/// config-less `IndexDatabase::migrate` on a MULTI-REPO DB must not run the model-manifest heal —
/// its connection has no scope context, so `active_repo_id` would resolve the first-sorting repo
/// and a heal-owed pass would delete THAT sibling's `repo_meta` model keys. This is exactly the
/// connection `rag-rat consolidate` reaches migrate through BEFORE registering its repo. The
/// witness gate in `ensure_model_manifest` skips + defers (the next config-bearing open heals
/// scoped), regardless of which caller reaches it next.
#[test]
fn config_less_migrate_on_a_multi_repo_db_leaves_sibling_model_meta_alone() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let db_path = root.join("global.sqlite");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        // Two real repos make the DB consolidated; the first-sorting one carries a LEGACY
        // active-model value — the row a config-less heal would delete.
        conn.execute_batch(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES
                 ('0-first-sibling', 's', 0), ('zz-other', 'z', 0);
             DELETE FROM repos WHERE repo_id = '__unassigned__';",
        )
        .unwrap();
        crate::index::meta::set_repo_meta(
            &conn,
            "0-first-sibling",
            "active_embedding_model",
            "fastembed-all-minilm-l6-v2",
        )
        .unwrap();
    }

    IndexDatabase::migrate(&db_path).unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let sibling_meta =
        crate::index::meta::repo_meta(&conn, "0-first-sibling", "active_embedding_model").unwrap();
    assert_eq!(
        sibling_meta.as_deref(),
        Some("fastembed-all-minilm-l6-v2"),
        "a config-less migrate healed (deleted) a sibling repo's model meta",
    );
    let _ = fs::remove_dir_all(&root);
}

/// A7 (Codex batch 4 — the source_root variant of the pre-adoption-pick family): an
/// AdoptionPending open derives `source_root` from the config-less SOLE pick (a first-sorting
/// SIBLING on a consolidated DB); adoption must RESET it from the config before any deferred heal,
/// or `ensure_graph_index_current` re-reads changed files from the sibling's checkout while
/// stamping the target's rows. The rule: adoption resets ALL connection-carried repo-derived state
/// before any deferred heal.
#[test]
fn incremental_open_resets_source_root_from_the_config_before_heals() {
    let fx = two_repo_fixture();
    let conn = fx.db.storage.connection();
    // A first-sorting sibling whose recorded source_root points at a NONEXISTENT checkout — the
    // root the pre-adoption pick would carry into the deferred graph heal.
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('0-first-sibling', \
         's', 0)",
        [],
    )
    .unwrap();
    crate::index::meta::set_repo_meta(
        conn,
        "0-first-sibling",
        "source_root",
        "/nonexistent-sibling-checkout",
    )
    .unwrap();
    let db_path = fx.db.database_path().to_path_buf();
    drop(fx.db);

    let mut config = source_config(fx.root_a.clone(), Language::Rust);
    config.database = db_path;
    let db = IndexDatabase::index_changed(&config).unwrap();

    assert_eq!(
        db.storage.source_root(),
        Some(config.root.as_path()),
        "adoption must reset source_root from the config before the deferred heals — a stale \
         sibling root would refresh the target's graph from the wrong checkout",
    );

    let _ = fs::remove_dir_all(fx.root_a);
}
