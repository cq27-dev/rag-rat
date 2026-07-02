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

/// #413 finding #5: `oracle_runs` has NO `repo_id` yet (V042 seam), so its GLOBAL prune must be
/// SKIPPED on a consolidated multi-repo DB — else GC for repo A wipes repo B's run rows whenever
/// B's context is absent from A's live sets.
#[test]
fn gc_skips_the_global_oracle_prune_on_a_multi_repo_db() {
    let fx = two_repo_fixture();
    // A dead-context oracle run: its commit/worktree are not in repo A's live sets.
    fx.db
        .storage
        .connection()
        .execute(
            "INSERT INTO oracle_runs(tool, tool_version, commit_sha, worktree_id, started_at, \
             status, stats_json)
             VALUES ('rust-analyzer', 'v1', 'dead-commit', 'dead-wt', 0, 'Completed', '{}')",
            [],
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
    assert_eq!(surviving, 1, "the multi-repo guard spares sibling oracle runs");
    let _ = fs::remove_dir_all(fx.root_a);
}

/// The complement: a single-repo DB has no sibling to protect, so the global oracle prune runs and
/// drops the dead-context run — unchanged behavior.
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
    db.storage
        .connection()
        .execute(
            "INSERT INTO oracle_runs(tool, tool_version, commit_sha, worktree_id, started_at, \
             status, stats_json)
             VALUES ('rust-analyzer', 'v1', 'dead-commit', 'dead-wt', 0, 'Completed', '{}')",
            [],
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

    // WITHOUT the override: the identity route resolves the shared-root sibling (the mis-scope the
    // pin fixes).
    crate::query::orientation::orientation(conn, &root, &root, None).unwrap();
    assert_eq!(
        schema::active_repo_id(conn).unwrap(),
        upstream_id,
        "no pin → orientation binds the shared-root upstream sibling",
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
