//! A6 generation-staged full rebuild: the V043 `files.generation` shape, reader consistency across
//! the generation flip, that a paused (staged, chunked) rebuild does not hold the write lock, and
//! that the poison sibling survives a full rebuild of the primary repo.
//!
//! The reader-consistency tests drive `rebuild_with_progress` on a background thread and pause it
//! at the after-wave-commit barrier ([`crate::index::rebuild::set_after_wave_commit`]). The
//! barrier registry is KEYED BY DATABASE PATH with an RAII unregister guard, so the tests are
//! safe under plain `cargo test` (one process, parallel libtest threads — the coverage job's
//! model) as well as nextest's process-per-test: concurrent tests use distinct temp databases and
//! therefore distinct keys.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use rusqlite::OptionalExtension;

use super::*;

/// A git fixture repo with the named source files under `src/`, plus its source `Config` (absolute
/// DB path, like production). A REAL git checkout so `adopt_repo_from_config` registers a portable
/// repo id.
fn generation_fixture(files: &[(&str, &str)]) -> (PathBuf, Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    for (name, body) in files {
        fs::write(root.join("src").join(name), body).unwrap();
    }
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    (root, config)
}

/// The repo id `root` maps to, resolved on a fresh read connection (registration already happened).
fn resolve_repo_id(db_path: &Path, root: &Path) -> String {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    crate::index::resolve_scope_repo_id(&conn, root, None).unwrap().unwrap_or_default()
}

/// The live `files.generation` pointer for `repo_id`, read from a fresh connection.
fn live_generation(db_path: &Path, repo_id: &str) -> i64 {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT value FROM repo_meta WHERE repo_id = ?1 AND key = 'live_files_generation'",
        [repo_id],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .unwrap()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0)
}

/// Non-deleted file rows of `repo_id` at a specific generation, read directly from `main.files`.
fn files_at_generation(db_path: &Path, repo_id: &str, generation: i64) -> i64 {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM main.files WHERE repo_id = ?1 AND generation = ?2 AND kind != \
         'deleted'",
        rusqlite::params![repo_id, generation],
        |r| r.get(0),
    )
    .unwrap()
}

/// The file count a READER sees through the production scope view — installs `temp.files` on a
/// fresh connection exactly as the raw-conn hook path does (resolving the live generation from
/// repo_meta), then counts through it. This is what a concurrent reader of the repo actually
/// observes.
fn reader_scoped_file_count(db_path: &Path, root: &Path) -> i64 {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let repo_id =
        crate::index::resolve_scope_repo_id(&conn, root, None).unwrap().unwrap_or_default();
    crate::index::install_worktree_scope_view(&conn, &repo_id, root, root).unwrap();
    conn.query_row("SELECT COUNT(*) FROM temp.files", [], |r| r.get(0)).unwrap()
}

/// Whether a path is visible to a READER through the scope view (live generation).
fn reader_sees_path(db_path: &Path, root: &Path, path: &str) -> bool {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let repo_id =
        crate::index::resolve_scope_repo_id(&conn, root, None).unwrap().unwrap_or_default();
    crate::index::install_worktree_scope_view(&conn, &repo_id, root, root).unwrap();
    conn.query_row("SELECT EXISTS(SELECT 1 FROM temp.files WHERE path = ?1)", [path], |r| r.get(0))
        .unwrap()
}

/// V043 adds `files.generation` and widens the UNIQUE to include it, so two rows identical except
/// in generation coexist while a true duplicate still violates the key. (The ABSOLUTE schema-tip
/// pin lives in the NEWEST migration's test — see `v044_widens_the_github_natural_keys_...` — so
/// this checks the tip SYMBOLICALLY and never needs a bump when a later migration lands.)
#[test]
fn migration_043_adds_generation_to_the_files_unique_key() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );
    assert!(
        conn_table_columns(&conn, "files").contains(&"generation".to_string()),
        "files gains a generation column"
    );

    // generation is part of the UNIQUE: two rows differing ONLY in generation coexist.
    let insert = |generation: i64| {
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             repo_id, generation) VALUES ('a.rs', 'rust', 'source', 'h', 0, 0, 'r', ?1)",
            [generation],
        )
    };
    insert(0).unwrap();
    insert(1).expect("a second generation of the same (repo_id, path, scope) coexists");
    assert!(
        insert(0).is_err(),
        "a true duplicate (same repo_id, path, commit_sha, worktree_id, generation) violates \
         UNIQUE"
    );
}

/// V043's `files` rebuild in ISOLATION against a post-V040 shape: it adds `generation` (absent
/// before — the deferred-absence assertion anchored to the migration DDL, not the full ladder),
/// preserves `id` + `repo_id` verbatim, backfills generation 0 (repo-neutral — no sole-repo
/// resolution), re-converges from a torn `files_new` scratch table, and is a no-op on replay.
#[test]
fn migration_043_adds_generation_and_reconverges_from_torn_state() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    // A post-V040 `files` shape (repo_id, NO generation) with one adopted row, plus a leftover
    // scratch table from a crashed prior V043 pass.
    conn.execute_batch(
        "CREATE TABLE files(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL, language TEXT NOT NULL, kind TEXT NOT NULL, sha256 TEXT NOT NULL,
            modified_at_ms INTEGER NOT NULL, generated INTEGER NOT NULL DEFAULT 0,
            indexed_at_ms INTEGER NOT NULL, indexed_revision TEXT NOT NULL DEFAULT '',
            commit_sha TEXT NOT NULL DEFAULT '', worktree_id TEXT NOT NULL DEFAULT '',
            has_test_code INTEGER NOT NULL DEFAULT 0, repo_id TEXT NOT NULL DEFAULT \
         '__unassigned__',
            UNIQUE(repo_id, path, commit_sha, worktree_id)
        );
        INSERT INTO files(id, path, language, kind, sha256, modified_at_ms, indexed_at_ms, repo_id)
            VALUES (42, 'keep.rs', 'rust', 'source', 'h', 0, 0, 'realrepo');
        CREATE TABLE files_new(leftover INTEGER);",
    )
    .unwrap();
    // Deferred-absence, anchored to the migration DDL in isolation (the Risk-memory pattern).
    assert!(
        !conn_table_columns(&conn, "files").contains(&"generation".to_string()),
        "generation is absent before V043 runs"
    );

    schema::apply_files_generation(&conn).unwrap();

    assert!(
        conn_table_columns(&conn, "files").contains(&"generation".to_string()),
        "V043 adds the generation column"
    );
    let (id, repo, generation): (i64, String, i64) = conn
        .query_row("SELECT id, repo_id, generation FROM files WHERE path = 'keep.rs'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(id, 42, "id preserved verbatim (FK target of chunks/symbols/edges_data)");
    assert_eq!(
        repo, "realrepo",
        "repo_id preserved verbatim (generation backfill is repo-neutral)"
    );
    assert_eq!(generation, 0, "existing rows carry generation 0");
    // Torn scratch re-converged (dropped + rebuilt), and a replay short-circuits on the sentinel.
    schema::apply_files_generation(&conn).expect("replay is a no-op");
    // The widened UNIQUE now admits a second generation of the same scope.
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, repo_id, \
         generation) VALUES ('keep.rs', 'rust', 'source', 'h', 0, 0, 'realrepo', 1)",
        [],
    )
    .expect("generation joined the UNIQUE key");
}

/// STEP 1 (reader consistency): a reader opened while a full rebuild is mid-flight sees the
/// COMPLETE old generation until the flip, then the complete new one; the dead generation is swept
/// by gc.
#[test]
fn a_reader_sees_the_complete_old_generation_until_the_rebuild_flips() {
    let (root, config) = generation_fixture(&[
        ("a.rs", "pub fn a() -> u32 { 1 }\n"),
        ("b.rs", "pub fn b() -> u32 { 2 }\n"),
        ("c.rs", "pub fn c() -> u32 { 3 }\n"),
    ]);
    // First rebuild establishes the live generation with three files.
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    let old_live = live_generation(&db_path, &repo_id);
    assert_eq!(reader_scoped_file_count(&db_path, &root), 3, "three files live before the rebuild");

    // Add a FOURTH file as an UNCOMMITTED (dirty) working-tree file, so the next generation differs
    // in count from the live one WITHOUT moving HEAD — the reader-consistency invariant is about
    // the GENERATION axis with commit/worktree held fixed (a HEAD move would put the old
    // generation's committed rows at a different commit scope than the reader resolves to,
    // conflating the axes).
    fs::write(root.join("src/d.rs"), "pub fn d() -> u32 { 4 }\n").unwrap();

    // Pause the rebuild at the after-wave-commit barrier: the fresh generation is staged
    // (committed) but the live pointer has NOT flipped yet. The barrier is KEYED by this test's
    // own database path (parallel same-process tests never collide) and the guard unregisters it
    // on drop, panic included (see `spawn_paused_rebuild`).
    let (_barrier, reached_rx, resume_tx, handle) = spawn_paused_rebuild(&config);

    // The rebuild is now paused with the staged generation committed but not live.
    reached_rx.recv().unwrap();
    assert_eq!(
        live_generation(&db_path, &repo_id),
        old_live,
        "the live generation has NOT advanced while the rebuild is mid-flight"
    );
    assert_eq!(
        reader_scoped_file_count(&db_path, &root),
        3,
        "a reader mid-rebuild sees the COMPLETE old generation (three files), not the staged one"
    );
    assert!(
        !reader_sees_path(&db_path, &root, "src/d.rs"),
        "the staged new file is invisible to readers until the flip"
    );

    // Let the rebuild finish (flip). The barrier guard unregisters on scope exit.
    resume_tx.send(()).unwrap();
    handle.join().unwrap();

    // After the flip a reader sees the NEW generation (four files, including d.rs).
    let new_live = live_generation(&db_path, &repo_id);
    assert!(new_live > old_live, "the flip advanced the live generation: {old_live} -> {new_live}");
    assert_eq!(reader_scoped_file_count(&db_path, &root), 4, "the reader now sees four files");
    assert!(
        reader_sees_path(&db_path, &root, "src/d.rs"),
        "the new file is visible after the flip"
    );

    // The dead old generation lingers until gc, then is swept.
    assert!(
        files_at_generation(&db_path, &repo_id, old_live) > 0,
        "the dead old generation lingers in storage before gc"
    );
    let db = IndexDatabase::open_config(&config).unwrap();
    db.garbage_collect().unwrap();
    assert_eq!(
        files_at_generation(&db_path, &repo_id, old_live),
        0,
        "gc sweeps the dead old generation"
    );

    drop(db);
    let _ = fs::remove_dir_all(root);
}

/// STEP 2 (bounded writer holds): while a full rebuild is PAUSED between committed waves — holding
/// no transaction, because the rebuild is chunked, not one mega-transaction — a write to ANOTHER
/// repo in the same shared DB completes well within the busy-timeout slice. A whole-rebuild
/// `BEGIN IMMEDIATE` would pin the WAL write lock and this would block past the 2 s bar.
#[test]
fn a_paused_rebuild_does_not_block_a_write_to_another_repo() {
    let (root, config) = generation_fixture(&[
        ("a.rs", "pub fn a() -> u32 { 1 }\n"),
        ("b.rs", "pub fn b() -> u32 { 2 }\n"),
    ]);
    // Register repo A by indexing it once, THEN seed a second repo B directly (phase-A
    // `register_repo` refuses a second real repo through the normal open path, exactly as the
    // multi_repo_scope fixtures seed a sibling).
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-b-gen', \
             'B', 0)",
            [],
        )
        .unwrap();
    }

    // Pause repo A's SECOND rebuild between committed waves. Database-keyed barrier + RAII guard
    // (see the step-1 test) — parallel same-process tests never collide on it.
    let (_barrier, reached_rx, resume_tx, handle) = spawn_paused_rebuild(&config);

    reached_rx.recv().unwrap();
    // The rebuild is paused with no open transaction. A write to repo B must land immediately.
    let write_conn = rusqlite::Connection::open(&db_path).unwrap();
    write_conn.busy_timeout(std::time::Duration::from_secs(2)).unwrap();
    let started = std::time::Instant::now();
    write_conn
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, repo_id)
             VALUES ('gen-b-mem', 'Invariant', 't', 'b', 'high', 'active', 0, 0, 'manual', 'v1', \
             'repo-b-gen')",
            [],
        )
        .expect("the write to repo B must not be blocked by repo A's paused rebuild");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "a write to another repo completed in {elapsed:?} while repo A's rebuild was mid-flight — \
         the staged rebuild must not hold the write lock across the whole file set"
    );

    resume_tx.send(()).unwrap();
    handle.join().unwrap();

    let _ = fs::remove_dir_all(root);
}

/// The poison-sibling harness survives a full rebuild of the PRIMARY repo: the generation-staged
/// rebuild + flip + carry-forward operate only on the primary repo's generations, so the sibling's
/// generation-0 rows (its own live generation) are never swept — and a repo-unscoped generation
/// sweep would have deleted them and tripped this assertion.
#[test]
fn the_poison_sibling_survives_a_full_rebuild_of_the_primary_repo() {
    let (root, config) = poison_test_config("gen_sibling");
    // The first rebuild seeds the poison sibling at its tail (default-on). A SECOND full rebuild of
    // the primary repo advances the primary's generation, carries its overlays forward, and leaves
    // its prior generation dead — none of which may disturb the sibling.
    let _first = IndexDatabase::rebuild(&config).unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());
    drop(db);
    let _ = fs::remove_dir_all(root);
}

/// This repo's `parser_failures` count for a path, from a fresh connection.
fn failure_count_for_path(db_path: &Path, repo_id: &str, path: &str) -> i64 {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM parser_failures WHERE repo_id = ?1 AND path = ?2",
        rusqlite::params![repo_id, path],
        |r| r.get(0),
    )
    .unwrap()
}

/// P2 #1: `parser_failures` is path-keyed, generation-less indexer state — the dead-GENERATION
/// sweep must NOT clear it (a path still failing in the LIVE generation shares its path with the
/// dead generation's row), while the (re)parse itself upserts on failure and clears on a clean
/// parse, and the rebuild tail drops records for paths removed from the tree.
#[test]
fn dead_generation_sweep_keeps_a_live_paths_parser_failure() {
    let (root, config) = generation_fixture(&[
        ("good.rs", "pub fn good() -> i32 { 1 }\n"),
        ("bad.rs", "pub fn broken( {\n"),
    ]);
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    assert_eq!(
        failure_count_for_path(&db_path, &repo_id, "src/bad.rs"),
        1,
        "the failing file's record is written by the first rebuild"
    );

    // Second rebuild: bad.rs still fails in the NEW generation; the old generation is left dead.
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Sweep the dead generation. The path exists (and still fails) in the LIVE generation, so the
    // path-keyed record must SURVIVE the generation-dead cascade.
    db.garbage_collect().unwrap();
    assert_eq!(
        failure_count_for_path(&db_path, &repo_id, "src/bad.rs"),
        1,
        "sweeping the dead generation must not clear a LIVE path's parser failure (P2 #1)"
    );
    drop(db);

    // Fix the file: the next rebuild's clean (re)parse clears the record — the REBUILD owns the
    // table, not the sweep (no gc needed).
    fs::write(root.join("src/bad.rs"), "pub fn repaired() -> i32 { 2 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "fix"]);
    IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        failure_count_for_path(&db_path, &repo_id, "src/bad.rs"),
        0,
        "a clean re-parse clears the path's failure record at rebuild time"
    );

    // Break it again, then DELETE the file: the rebuild-tail orphan sweep drops the record for a
    // path with a dead-generation row but none in the published generation.
    fs::write(root.join("src/bad.rs"), "pub fn broken_again( {\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "break"]);
    IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(failure_count_for_path(&db_path, &repo_id, "src/bad.rs"), 1);
    fs::remove_file(root.join("src/bad.rs")).unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "remove"]);
    IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        failure_count_for_path(&db_path, &repo_id, "src/bad.rs"),
        0,
        "a path removed from the tree loses its stale record in the rebuild tail"
    );

    let _ = fs::remove_dir_all(root);
}

/// P2 #2: the pointer flip is the TERMINAL write of a rebuild. A failure anywhere in the tail
/// (here: injected into the logical-symbol rebuild via a trigger) must roll the WHOLE terminal
/// transaction back — `live_files_generation` stays on the old generation and readers are
/// unaffected; a retry stages a fresh generation and flips. Also pins the STAGED-until-publish
/// parser-failure contract: the fixture's failing file is FIXED before the failing rebuild, and
/// its failure record survives the failed rebuild (the staged clear rolled back with the terminal
/// transaction) then clears on the successful retry.
#[test]
fn a_tail_failure_leaves_the_old_generation_live_and_a_retry_flips() {
    let (root, config) = generation_fixture(&[
        ("a.rs", "pub fn a() -> u32 { 1 }\n"),
        ("b.rs", "pub fn b() -> u32 { 2 }\n"),
        ("bad.rs", "pub fn broken( {\n"),
    ]);
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    let old_live = live_generation(&db_path, &repo_id);
    assert_eq!(reader_scoped_file_count(&db_path, &root), 3);
    assert_eq!(failure_count_for_path(&db_path, &repo_id, "src/bad.rs"), 1);

    // Fix the failing file (dirty working-tree edit — the rebuild reads disk): the NEXT successful
    // rebuild will clear the record, but the FAILED one below must not (the clear is staged and
    // rolls back with the terminal transaction — parser-failure state flips WITH the pointer).
    fs::write(root.join("src/bad.rs"), "pub fn repaired() -> u32 { 3 }\n").unwrap();

    // Inject a failure into the terminal transaction: every logical-symbol INSERT aborts. The
    // trigger lives in the MAIN schema, so the rebuild's own fresh connection hits it.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_logical_symbols BEFORE INSERT ON logical_symbols
             BEGIN SELECT RAISE(ABORT, 'injected logical-symbol failure'); END;",
        )
        .unwrap();
    }

    let err = IndexDatabase::rebuild(&config);
    assert!(err.is_err(), "the injected logical-symbol failure must fail the rebuild");
    assert_eq!(
        live_generation(&db_path, &repo_id),
        old_live,
        "a tail failure must NOT publish the staged generation (P2 #2): the flip is terminal"
    );
    assert_eq!(
        reader_scoped_file_count(&db_path, &root),
        3,
        "readers stay on the complete old generation after the failed rebuild"
    );
    assert_eq!(
        failure_count_for_path(&db_path, &repo_id, "src/bad.rs"),
        1,
        "the fixed file's staged CLEAR rolled back with the terminal txn — parser-failure state \
         is published atomically with the pointer, never mid-pass"
    );

    // Retry after removing the fault: the rebuild stages a FRESH generation (above the failed
    // attempt's committed-but-never-flipped waves) and flips.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("DROP TRIGGER fail_logical_symbols;").unwrap();
    }
    IndexDatabase::rebuild(&config).unwrap();
    assert!(
        live_generation(&db_path, &repo_id) > old_live,
        "the retry flips to a fresh generation"
    );
    assert_eq!(reader_scoped_file_count(&db_path, &root), 3);
    assert_eq!(
        failure_count_for_path(&db_path, &repo_id, "src/bad.rs"),
        0,
        "the successful retry publishes the staged clear"
    );

    let _ = fs::remove_dir_all(root);
}

/// P2 #3: dead-generation reclamation needs NO git context, so it runs even on the "no live
/// context" path that refuses CONTEXT pruning — a non-git / plain-directory index must not leak a
/// full generation per rebuild.
#[test]
fn non_git_indexes_sweep_dead_generations_despite_the_context_prune_refusal() {
    // A PLAIN DIRECTORY (no git init): the repo registers under the placeholder (identity Absent),
    // and `prune_to_live(&[], &[])` is exactly the refused-context path.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn a() -> u32 { 1 }\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn b() -> u32 { 2 }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let repo_id = db.active_repo_id.clone();
    let fts_count = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM chunk_fts", [], |r| r.get(0))
            .unwrap()
    };
    let report = db.prune_to_live(&[], &[]).unwrap();
    assert!(report.skipped, "context pruning is still refused with no live sets");
    let baseline_fts = fts_count(&db);
    drop(db);

    // A second full rebuild leaves the first generation dead; the context-refused prune must
    // still reclaim it (files, chunks, symbols, FTS), keeping growth flat.
    let db = IndexDatabase::rebuild(&config).unwrap();
    let live =
        crate::index::schema::live_files_generation(db.storage.connection(), &repo_id).unwrap();
    let dead_count = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM main.files WHERE repo_id = ?1 AND generation != ?2",
                rusqlite::params![repo_id, live],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert!(dead_count(&db) > 0, "the second rebuild leaves the first generation dead");

    let report = db.prune_to_live(&[], &[]).unwrap();
    assert!(report.skipped, "context pruning is refused — but the generation sweep ran");
    assert!(report.files_pruned > 0, "the dead generation was reclaimed");
    assert_eq!(dead_count(&db), 0, "no dead-generation rows remain for this repo (P2 #3)");
    assert_eq!(
        fts_count(&db),
        baseline_fts,
        "chunk_fts stays flat across rebuild + sweep — no unbounded growth"
    );
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());
    drop(db);
    let _ = fs::remove_dir_all(root);
}

/// P2 #4: a carried-forward overlay's edges are re-resolved against the freshly staged base inside
/// the terminal transaction — a base symbol referenced by an overlay edge gets the NEW
/// generation's re-minted `symbols.id`, and after gc no edge endpoint dangles.
#[test]
fn carried_overlay_edges_re_resolve_onto_the_new_base_generation() {
    let (root, config) = generation_fixture(&[("lib.rs", "pub fn base_fn() -> i32 { 1 }\n")]);
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);

    // A linked worktree whose overlay ADDS a caller of the base symbol.
    let linked =
        root.parent().unwrap().join(format!("{}-wt", root.file_name().unwrap().to_string_lossy()));
    run_git(&root, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/caller.rs"), "pub fn overlay_caller() -> i32 { base_fn() }\n")
        .unwrap();
    let mut db = IndexDatabase::open_config(&config).unwrap();
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    drop(db);

    // The overlay edge resolved onto the CURRENT base generation's symbol id.
    let overlay_edge_target = |db_path: &Path| -> Option<i64> {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT d.to_symbol_id FROM edges_data d
             JOIN main.files f ON f.id = d.source_file_id
             JOIN name_strings ek ON ek.id = d.edge_kind_id
             WHERE f.path = 'src/caller.rs' AND f.worktree_id != '' AND ek.value = 'calls_name'",
            [],
            |r| r.get::<_, Option<i64>>(0),
        )
        .unwrap()
    };
    let before = overlay_edge_target(&db_path).expect("overlay edge resolves onto the base symbol");

    // Full rebuild: the base re-emits with re-minted symbol ids; the overlay rows are carried
    // forward and their edges re-resolved in the terminal transaction.
    let db = IndexDatabase::rebuild(&config).unwrap();
    let live = live_generation(&db_path, &repo_id);
    let new_base_symbol: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT s.id FROM main.symbols s JOIN main.files f ON f.id = s.file_id
             WHERE s.name = 'base_fn' AND f.repo_id = ?1 AND f.generation = ?2",
            rusqlite::params![repo_id, live],
            |r| r.get(0),
        )
        .unwrap();
    let after = overlay_edge_target(&db_path)
        .expect("the carried overlay edge must stay resolved after the rebuild");
    assert_eq!(
        after, new_base_symbol,
        "the carried overlay edge re-resolves onto the NEW base generation's symbol id (P2 #4)"
    );
    assert_ne!(after, before, "the base symbol id was re-minted by the rebuild");

    // After the dead generation is swept, no non-NULL edge endpoint dangles.
    db.garbage_collect().unwrap();
    let dangling: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM edges_data
             WHERE to_symbol_id IS NOT NULL
               AND to_symbol_id NOT IN (SELECT id FROM main.symbols)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dangling, 0, "no dangling edge endpoints after the sweep");

    drop(db);
    let _ = fs::remove_dir_all(&linked);
    let _ = fs::remove_dir_all(root);
}

/// P2 (parser_failures.rs): a previously-indexed path that freshly fails PREPARATION (invalid
/// UTF-8 — a failure recorded with NO new file row) must survive the rebuild-tail orphan sweep:
/// the staged-upsert set shields exactly the paths visited-and-failed this pass.
#[test]
fn a_fresh_prepare_failure_for_a_previously_indexed_path_survives_the_tail_sweep() {
    let (root, config) = generation_fixture(&[
        ("good.rs", "pub fn good() -> i32 { 1 }\n"),
        ("vic.rs", "pub fn victim() -> i32 { 2 }\n"),
    ]);
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    assert_eq!(failure_count_for_path(&db_path, &repo_id, "src/vic.rs"), 0);

    // Turn the previously-indexed file UNREADABLE-as-text (invalid UTF-8): preparation fails, a
    // failure is recorded, and NO new file row is written — while the OLD generation still holds
    // vic.rs rows. A file-row-presence sweep alone would treat the path as removed and delete the
    // record recorded moments earlier.
    fs::write(root.join("src/vic.rs"), [0xFF, 0xFE, 0x00, 0x9F, 0x92]).unwrap();
    IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        failure_count_for_path(&db_path, &repo_id, "src/vic.rs"),
        1,
        "a fresh PREPARE failure for a previously-indexed path survives the tail orphan sweep"
    );

    let _ = fs::remove_dir_all(root);
}

/// P2 (rebuild.rs FTS race): during a FIRST index (no `chunk_text_dict` yet — chunk text only
/// temp-staged until `build_chunk_text_store`), a concurrent connection's FTS freshness heal must
/// NOT destroy the staged generation's inline `chunk_fts` rows: `ensure_fts_fresh` defers while
/// any chunk lacks its durable text row, and the published generation ends BM25-complete.
#[test]
fn a_concurrent_fts_refresh_during_first_index_does_not_lose_staged_rows() {
    let (root, config) = generation_fixture(&[
        ("a.rs", "pub fn first_index_alpha() -> u32 { 1 }\n"),
        ("b.rs", "pub fn first_index_beta() -> u32 { 2 }\n"),
    ]);
    let db_path = config.database.clone();

    // Pause the FIRST index between committed waves: chunk_fts rows exist for the staged chunks,
    // chunk_text does not (the text store is built in Phase 2).
    let (_barrier, reached_rx, resume_tx, handle) = spawn_paused_rebuild(&config);
    reached_rx.recv().unwrap();

    // A concurrent open sees a content revision the FTS has never synced (first index) — exactly
    // the state that used to trigger the destructive 'delete-all' + rebuild-from-chunk_text.
    let db2 = IndexDatabase::open(&db_path).unwrap();
    let fts_rows = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM chunk_fts", [], |r| r.get(0))
            .unwrap()
    };
    let staged_fts_before = fts_rows(&db2);
    assert!(staged_fts_before > 0, "the committed waves wrote inline chunk_fts rows");
    db2.ensure_fts_fresh().expect("the freshness heal must degrade gracefully, not error");
    assert_eq!(
        fts_rows(&db2),
        staged_fts_before,
        "the freshness heal must NOT destroy staged chunk_fts rows while chunk text is only \
         temp-staged (it defers until the text store is complete)"
    );
    drop(db2);

    resume_tx.send(()).unwrap();
    handle.join().unwrap();

    // The published generation is BM25-complete: every chunk of THIS repo has its FTS row (the
    // poison sibling's chunk deliberately has neither an fts nor a text row and is excluded).
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let repo_id = resolve_repo_id(&db_path, &root);
    let missing_fts: bool = conn
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM main.chunks
                 JOIN main.files ON main.files.id = main.chunks.file_id
                 WHERE main.files.repo_id = ?1
                   AND main.chunks.id NOT IN (SELECT rowid FROM chunk_fts)
             )",
            [&repo_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!missing_fts, "every published chunk has a BM25 row after the first index");

    let _ = fs::remove_dir_all(root);
}

/// P2 (incremental.rs interleaved writers) — the PROOF's regression test: a memory written
/// mid-rebuild through the LOCKLESS path (`open_config`, the MCP route — no advisory flock),
/// bound to a LIVE-generation chunk rowid that the flip + gc then retire, survives with full
/// integrity: the memory is intact, `memory_validate` re-anchors its binding by content hash, no
/// FK violation exists, and the poison sibling is untouched.
#[test]
fn a_memory_written_mid_rebuild_survives_the_flip_intact() {
    let (root, config) = generation_fixture(&[
        ("anchor.rs", "pub fn interleave_anchor() -> u32 {\n    777\n}\n"),
        ("other.rs", "pub fn other() -> u32 { 1 }\n"),
    ]);
    IndexDatabase::rebuild(&config).unwrap();

    // Pause the SECOND rebuild between committed waves, then write a memory through the lockless
    // open_config path exactly as the MCP tools do — mid-rebuild, against the LIVE generation.
    let (_barrier, reached_rx, resume_tx, handle) = spawn_paused_rebuild(&config);
    reached_rx.recv().unwrap();

    // The interleaved writer: no WriteLock taken anywhere on this path.
    let writer = IndexDatabase::open_config(&config).unwrap();
    let live_chunk_id: i64 = writer
        .storage
        .connection()
        .query_row(
            "SELECT chunks.id FROM chunks JOIN files ON files.id = chunks.file_id
             WHERE files.path = 'src/anchor.rs' ORDER BY chunks.id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let created = writer
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "interleave_anchor returns 777".to_string(),
            body: "Written mid-rebuild through the lockless path; must survive the flip."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: crate::query::memory::RepoMemoryBindTarget {
                chunk_id: Some(live_chunk_id),
                ..Default::default()
            },
        })
        .expect("a mid-rebuild memory write on the lockless path must succeed");
    drop(writer);

    resume_tx.send(()).unwrap();
    handle.join().unwrap();

    // Post-flip + gc: the chunk rowid the binding captured is retired with the dead generation.
    let db = IndexDatabase::open_config(&config).unwrap();
    db.garbage_collect().unwrap();
    let old_chunk_live: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.chunks WHERE id = ?1", [live_chunk_id], |r| r.get(0))
        .unwrap();
    assert_eq!(old_chunk_live, 0, "the bound chunk rowid was retired by the flip + gc");

    // Full integrity: the memory is intact and re-anchors by content hash onto the published
    // generation's chunk (the ordinary reindex lifecycle — rowids re-mint, content anchors hold).
    let report = db.memory_validate().unwrap();
    assert_eq!(
        report.relocated, 1,
        "the mid-rebuild binding relocates onto the published generation: {report:?}"
    );
    let (status, rebound_chunk): (String, Option<i64>) = db
        .storage
        .connection()
        .query_row(
            "SELECT anchor_status, chunk_id FROM repo_memory_bindings WHERE memory_id = ?1",
            [&created.memory.memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "relocated", "the binding re-anchored (the healthy relocated state)");
    assert_ne!(rebound_chunk, Some(live_chunk_id), "the binding points at the NEW chunk row");
    let fk_violations: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk_violations, 0, "no FK violation from the interleaved write");
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());

    drop(db);
    let _ = fs::remove_dir_all(root);
}

/// P2 (incremental.rs / repo_brief): `summary_counts.graph_edges` is an EXACTNESS counter — it
/// must count only the ACTIVE generation's edges, not the superseded generation's (visible until
/// gc) or a staged one's.
#[test]
fn repo_brief_edge_count_is_generation_scoped() {
    let (root, config) = generation_fixture(&[(
        "lib.rs",
        "pub fn callee() -> u32 { 1 }\npub fn caller() -> u32 { callee() }\n",
    )]);
    IndexDatabase::rebuild(&config).unwrap();
    // Second rebuild: the first generation's edges linger until gc.
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    let live_edges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges_data e
             JOIN main.files f ON f.id = e.source_file_id
             WHERE f.repo_id = ?1 AND f.generation = ?2",
            rusqlite::params![db.active_repo_id, db.active_generation],
            |r| r.get(0),
        )
        .unwrap();
    let all_repo_edges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges_data e
             JOIN main.files f ON f.id = e.source_file_id
             WHERE f.repo_id = ?1",
            [&db.active_repo_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(live_edges > 0, "the fixture produces at least one live edge");
    assert!(
        all_repo_edges > live_edges,
        "pre-gc the repo carries the dead generation's edges too ({all_repo_edges} vs \
         {live_edges}) — the very over-count the predicate exists to exclude"
    );

    let counts = crate::query::repo_brief::summary_counts(conn).unwrap();
    assert_eq!(
        counts.graph_edges, live_edges as u64,
        "summary_counts.graph_edges counts exactly the live generation's edges"
    );

    drop(db);
    let _ = fs::remove_dir_all(root);
}

/// Install the keyed pause barrier for `config`'s database and spawn a full rebuild on a thread,
/// returning `(barrier_guard, reached_rx, resume_tx, join_handle)`. The rebuild pauses after its
/// FIRST committed wave until `resume_tx` fires. Shared by the interleaving tests.
#[allow(clippy::type_complexity)]
fn spawn_paused_rebuild(
    config: &Config,
) -> (
    crate::index::rebuild::WaveBarrierGuard,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let (reached_tx, reached_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let guard = {
        let paused = AtomicBool::new(false);
        let reached_tx = Mutex::new(reached_tx);
        let resume_rx = Mutex::new(resume_rx);
        crate::index::rebuild::set_after_wave_commit(
            &config.database,
            Arc::new(move || {
                if !paused.swap(true, Ordering::SeqCst) {
                    reached_tx.lock().unwrap().send(()).unwrap();
                    resume_rx.lock().unwrap().recv().unwrap();
                }
            }),
        )
    };
    let rebuild_config = config.clone();
    // Propagate the calling test's poison opt-out onto the worker thread: the harness flag is
    // THREAD-local (deliberately — parallel `cargo test` isolation), so a spawned rebuild would
    // otherwise re-seed the sibling behind a test that disabled it.
    let poison_disabled = crate::index::poison_sibling::poison_disabled_on_this_thread();
    let handle = std::thread::spawn(move || {
        let _poison_off =
            poison_disabled.then(crate::index::poison_sibling::disable_poison_sibling);
        // `rebuild_with_progress` acquires the per-repo write flock ITSELF (batch 6) — every
        // rebuild entry holds it by construction, which is what gc's `!= live` deadness predicate
        // relies on (holding the flock proves no rebuild is mid-flight, so above-live rows are
        // abandoned staging, not in-progress). The barrier fires AFTER the first wave commit, well
        // past that acquisition, so the flock is held for the whole pause — a main-thread collector
        // that takes it then genuinely contends.
        IndexDatabase::rebuild(&rebuild_config).unwrap();
    });
    (guard, reached_rx, resume_tx, handle)
}

/// P2 batch 5 (gc.rs, lock-as-discriminator): with deadness `generation != live` under the
/// flock precondition, a collector racing a mid-flight rebuild SERIALIZES on the per-repo write
/// flock (which every production rebuild entry holds) instead of sweeping around it — and once
/// the rebuild completes, gc reclaims the superseded generation.
#[test]
fn gc_serializes_on_the_flock_while_a_rebuild_is_mid_flight() {
    let (root, config) = generation_fixture(&[
        ("a.rs", "pub fn a() -> u32 { 1 }\n"),
        ("b.rs", "pub fn b() -> u32 { 2 }\n"),
    ]);
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    let old_live = live_generation(&db_path, &repo_id);

    let (_barrier, reached_rx, resume_tx, handle) = spawn_paused_rebuild(&config);
    reached_rx.recv().unwrap();

    // Mid-pause the staged (above-live) generation is committed. A production collector takes
    // the per-repo flock first — held by the paused rebuild — so it SERIALIZES rather than
    // sweeping the in-progress staging (the `!= live` predicate is only safe under that lock).
    let lock_repo = rag_rat_base::locks::write_lock_repo_id(&config);
    let contended = rag_rat_base::locks::WriteLock::acquire_timeout(
        &config.database,
        &lock_repo,
        std::time::Duration::from_millis(150),
    )
    .unwrap();
    assert!(
        contended.is_none(),
        "a flock-taking collector must block while the rebuild is mid-flight"
    );

    // Resume; the rebuild flips; the flock frees; the collector proceeds and reclaims the now-
    // superseded generation.
    resume_tx.send(()).unwrap();
    handle.join().unwrap();
    assert!(live_generation(&db_path, &repo_id) > old_live, "the rebuild flipped");
    let _flock =
        rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, &lock_repo).unwrap();
    let db = IndexDatabase::open_config(&config).unwrap();
    db.garbage_collect().unwrap();
    assert_eq!(
        files_at_generation(&db_path, &repo_id, old_live),
        0,
        "the superseded generation is reclaimed once the rebuild completed"
    );
    assert_eq!(reader_scoped_file_count(&db_path, &root), 2);
    drop(db);
    let _ = fs::remove_dir_all(root);
}

/// P2 (file_rows.rs, generation-bounded deletes): a LOCKLESS heal replacing a path mid-rebuild
/// operates at the LIVE generation — it must not remove the STAGED generation's committed row for
/// the same scope key (the V043 UNIQUE admits one row per scope key PER generation, so a
/// generation-less delete over-matched both).
#[test]
fn a_lockless_heal_mid_rebuild_does_not_remove_the_staged_row() {
    let (root, config) = generation_fixture(&[
        ("a.rs", "pub fn heal_target() -> u32 { 1 }\n"),
        ("b.rs", "pub fn bystander() -> u32 { 2 }\n"),
    ]);
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    let old_live = live_generation(&db_path, &repo_id);

    let (_barrier, reached_rx, resume_tx, handle) = spawn_paused_rebuild(&config);
    reached_rx.recv().unwrap();

    let staged_rows_for_path = |db_path: &Path, path: &str| -> i64 {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM main.files
             WHERE repo_id = ?1 AND generation > ?2 AND path = ?3",
            rusqlite::params![repo_id, old_live, path],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        staged_rows_for_path(&db_path, "src/a.rs"),
        1,
        "the paused rebuild staged the path's fresh row"
    );

    // The lockless heal (open_config — no flock anywhere on this path) replaces the path at the
    // LIVE generation: remove_file_in_scope + re-index. Its generation-bounded delete must leave
    // the staged row alone.
    let writer = IndexDatabase::open_config(&config).unwrap();
    writer.heal_file(Path::new("src/a.rs")).unwrap();
    drop(writer);
    assert_eq!(
        staged_rows_for_path(&db_path, "src/a.rs"),
        1,
        "a live-generation heal must not remove the staged generation's row for the same scope \
         key (P2: deletes carry the writer's generation)"
    );

    // The rebuild resumes, flips, and the published generation serves the path.
    resume_tx.send(()).unwrap();
    handle.join().unwrap();
    assert!(
        live_generation(&db_path, &repo_id) > old_live,
        "the rebuild flipped despite the interleaved heal"
    );
    assert!(
        reader_sees_path(&db_path, &root, "src/a.rs"),
        "the published generation serves the healed path's content"
    );
    assert_eq!(reader_scoped_file_count(&db_path, &root), 2);

    let _ = fs::remove_dir_all(root);
}

/// P2 (lifecycle.rs, bare-open scope view): the bare `IndexDatabase::open` (the MCP
/// `call_tool(database, …)` read path) resolves `files` through a repo+generation view — a reader
/// on that path sees only the LIVE generation during a mid-flight rebuild, and only the NEW
/// generation after the flip while the superseded one lingers pre-gc.
#[test]
fn a_bare_open_reader_is_generation_scoped() {
    // Single-repo by nature: post-A7 the bare open is DEFINED only for a single-repo DB (it
    // refuses the multi-repo shape the registered poison sibling would make real on this git
    // fixture), and this test's subject is exactly that bare-open path.
    let _poison_off = crate::index::poison_sibling::disable_poison_sibling();
    let (root, config) = generation_fixture(&[
        ("a.rs", "pub fn a() -> u32 { 1 }\n"),
        ("b.rs", "pub fn b() -> u32 { 2 }\n"),
    ]);
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();

    let (_barrier, reached_rx, resume_tx, handle) = spawn_paused_rebuild(&config);
    reached_rx.recv().unwrap();

    // Mid-rebuild: the bare open's `files` view serves only the live generation — the committed
    // staged waves are invisible (pre-fix, unqualified `files` = main.files showed both).
    let bare = IndexDatabase::open(&db_path).unwrap();
    let view_count: i64 = bare
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    let raw_count: i64 = bare
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(view_count, 2, "the bare open serves exactly the live generation mid-rebuild");
    assert!(
        raw_count > view_count,
        "main.files holds the staged rows too ({raw_count} vs {view_count}) — the view is what \
         hides them"
    );
    drop(bare);

    resume_tx.send(()).unwrap();
    handle.join().unwrap();

    // Post-flip, pre-gc: the superseded generation lingers in main.files, but a fresh bare open
    // serves only the NEW generation.
    let bare = IndexDatabase::open(&db_path).unwrap();
    let view_count: i64 = bare
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    let raw_count: i64 = bare
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(view_count, 2, "the bare open serves exactly the new live generation post-flip");
    assert!(
        raw_count > view_count,
        "the superseded generation (and the sibling repo's rows) linger in main.files pre-gc; the \
         repo+generation view keeps them out of every unqualified `files` read"
    );
    drop(bare);

    let _ = fs::remove_dir_all(root);
}

/// P2 batch 4 (schema-apply race): every path that can APPLY schema serializes on the GLOBAL
/// schema lock with a double-checked state probe — two repos' concurrent `index --full` against
/// one shared Missing-schema DB reach `create_or_migrate` simultaneously, and un-serialized
/// `schema::apply` races itself (check-then-ALTER duplicate columns, dirty-marker churn).
#[test]
fn concurrent_create_or_migrate_applies_the_schema_exactly_once() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let db_path = root.join("index.sqlite");

    let a = db_path.clone();
    let b = db_path.clone();
    let t1 = std::thread::spawn(move || IndexDatabase::create_or_migrate(&a).map(|_| ()));
    let t2 = std::thread::spawn(move || IndexDatabase::create_or_migrate(&b).map(|_| ()));
    t1.join().unwrap().expect("one concurrent applier applies");
    t2.join().unwrap().expect("the other no-ops under the schema lock");

    // The ladder is intact: Compatible at LATEST, one ledger row per migration, no dirty marker
    // (a lingering dirty marker or duplicate-column failure would surface as Dirty / an error
    // above).
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let status = crate::index::schema::status(&conn).unwrap();
    assert_eq!(status.state, crate::index::schema::SchemaState::Compatible);
    assert_eq!(status.current_version, crate::index::schema::LATEST_SCHEMA_VERSION);
    assert_eq!(
        status.migrations.len(),
        crate::index::schema::LATEST_SCHEMA_VERSION as usize,
        "exactly one schema_version row per migration"
    );

    let _ = fs::remove_dir_all(root);
}

/// P2 batch 6 #1 (git history rides the flip): a tail failure must leave the WHOLE git-history
/// domain reporting the OLD state — `git_commit` meta, the `git_history_indexed_*` reload-gate
/// cursors, AND the `git_commits`/`git_file_changes` ROWS themselves. Batch 4 landed the rows early
/// as "keyed, inert facts," but they are read DIRECTLY by orientation / churn / commit search
/// (`recent_commit_subjects` et al.), so a rebuild that observed the new history then failed
/// pre-flip would let those surfaces show H2's commit while readers stay on the old file
/// generation. Now the rows fold into the terminal transaction, so a tail failure rolls them back
/// with the pointer, and the retry publishes files + history together.
#[test]
fn a_tail_failure_leaves_git_meta_and_history_cursors_on_the_old_state() {
    let (root, config) = generation_fixture(&[("a.rs", "pub fn a() -> u32 { 1 }\n")]);
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    let (h1, _) = crate::index::resolve_git_context(&root);

    let git_commit_meta = |db_path: &Path, repo_id: &str| -> Option<String> {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        crate::index::repo_meta(&conn, repo_id, "git_commit").unwrap()
    };
    let history_current = |db_path: &Path, root: &Path| -> bool {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        crate::index::git_history::is_history_current(&conn, root)
    };
    assert_eq!(git_commit_meta(&db_path, &repo_id).as_deref(), Some(h1.as_str()));
    assert!(history_current(&db_path, &root), "history is current at H1 after the first rebuild");

    // Move HEAD to H2, then fail the next rebuild in its terminal transaction.
    fs::write(root.join("src/b.rs"), "pub fn b() -> u32 { 2 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "h2"]);
    let (h2, _) = crate::index::resolve_git_context(&root);
    assert_ne!(h1, h2);
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_logical_symbols BEFORE INSERT ON logical_symbols
             BEGIN SELECT RAISE(ABORT, 'injected logical-symbol failure'); END;",
        )
        .unwrap();
    }
    assert!(IndexDatabase::rebuild(&config).is_err(), "the injected failure fails the rebuild");

    // The WHOLE history domain still reports the OLD state: status() would show H1, a history
    // reload stays owed (the cursors did not advance), AND the H2 commit ROW never landed — it was
    // written inside the terminal transaction and rolled back with the failed tail (batch 6 #1), so
    // orientation / churn / commit search (which read `git_commits` directly) never see H2 while
    // the old file generation is live.
    assert_eq!(
        git_commit_meta(&db_path, &repo_id).as_deref(),
        Some(h1.as_str()),
        "git_commit meta must not report H2 while the old generation is live"
    );
    assert!(
        !history_current(&db_path, &root),
        "the history cursors must not claim H2 — a reload is still owed after the tail failure"
    );
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let h2_row: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM git_commits WHERE repo_id = ?1 AND hash = ?2",
                rusqlite::params![repo_id, h2],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            h2_row, 0,
            "the H2 commit ROW must roll back with the failed terminal txn (batch 6 #1) — the git \
             rows are read directly by orientation/search, so they cannot precede the flip"
        );
        // Orientation reads the newest indexed subjects straight from `git_commits`; the H2
        // subject must be absent after the tail failure (the exact surface batch 6 #1 protects).
        let h2_subject_visible: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM git_commits WHERE repo_id = ?1 AND subject = 'h2')",
                rusqlite::params![repo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !h2_subject_visible,
            "orientation/commit-search must not surface H2's subject while readers are on the old \
             generation"
        );
    }

    // The retry publishes files + git authority together.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("DROP TRIGGER fail_logical_symbols;").unwrap();
    }
    IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(git_commit_meta(&db_path, &repo_id).as_deref(), Some(h2.as_str()));
    assert!(history_current(&db_path, &root), "cursors advanced with the successful flip");

    let _ = fs::remove_dir_all(root);
}

/// P2 batch 5 (abandoned staging): failed rebuilds leave committed-but-never-flipped generations
/// ABOVE live — with `!= live` deadness under the flock precondition, gc reclaims them (a
/// persistently failing tail must not leak a full staged copy per retry).
#[test]
fn gc_reclaims_abandoned_staged_generations_from_failed_rebuilds() {
    let (root, config) = generation_fixture(&[
        ("a.rs", "pub fn a() -> u32 { 1 }\n"),
        ("b.rs", "pub fn b() -> u32 { 2 }\n"),
    ]);
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    let live = live_generation(&db_path, &repo_id);

    // Two consecutive tail failures abandon two staged generations above live.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_logical_symbols BEFORE INSERT ON logical_symbols
             BEGIN SELECT RAISE(ABORT, 'injected logical-symbol failure'); END;",
        )
        .unwrap();
    }
    assert!(IndexDatabase::rebuild(&config).is_err());
    assert!(IndexDatabase::rebuild(&config).is_err());
    let above_live = |db_path: &Path| -> i64 {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT COUNT(DISTINCT generation) FROM main.files
             WHERE repo_id = ?1 AND generation > ?2",
            rusqlite::params![repo_id, live],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(above_live(&db_path), 2, "each failed rebuild abandoned one staged generation");

    // A production collector (flock held — no rebuild can be mid-flight) reclaims BOTH.
    let lock_repo = rag_rat_base::locks::write_lock_repo_id(&config);
    let _flock =
        rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, &lock_repo).unwrap();
    let db = IndexDatabase::open_config(&config).unwrap();
    db.garbage_collect().unwrap();
    drop(db);
    assert_eq!(above_live(&db_path), 0, "both abandoned staged generations reclaimed (P2)");
    assert_eq!(live_generation(&db_path, &repo_id), live, "the live pointer never moved");
    assert_eq!(reader_scoped_file_count(&db_path, &root), 2, "the live generation is untouched");

    // The fault removed, the retry stages and flips normally.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("DROP TRIGGER fail_logical_symbols;").unwrap();
    }
    IndexDatabase::rebuild(&config).unwrap();
    assert!(live_generation(&db_path, &repo_id) > live);
    assert_eq!(reader_scoped_file_count(&db_path, &root), 2);

    let _ = fs::remove_dir_all(root);
}

/// P2 batch 5 (source_root joins cursors-last): a rebuild from a NEW checkout root that fails
/// pre-publish must leave the persisted `repo_meta[source_root]` on the OLD root — old-generation
/// readers resolve fs-fallback paths (memory validation, heals) against it; the retry publishes
/// the new root together with the flip.
#[test]
fn a_tail_failure_leaves_source_root_on_the_old_checkout() {
    let (root, config) = generation_fixture(&[("a.rs", "pub fn a() -> u32 { 1 }\n")]);
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    let persisted_root = |db_path: &Path| -> Option<String> {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        crate::index::repo_meta(&conn, &repo_id, "source_root").unwrap()
    };
    assert_eq!(persisted_root(&db_path).as_deref(), Some(root.display().to_string().as_str()));

    // A SECOND checkout of the same repo (same root commit → same repo id), rebuilt against the
    // SAME database.
    let root2 = unique_temp_root();
    let _ = fs::remove_dir_all(&root2);
    run_git(&root, &["clone", "-q", ".", root2.to_str().unwrap()]);
    let mut config2 = config.clone();
    config2.root = root2.clone();

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_logical_symbols BEFORE INSERT ON logical_symbols
             BEGIN SELECT RAISE(ABORT, 'injected logical-symbol failure'); END;",
        )
        .unwrap();
    }
    assert!(IndexDatabase::rebuild(&config2).is_err(), "the tail failure aborts the rebuild");
    assert_eq!(
        persisted_root(&db_path).as_deref(),
        Some(root.display().to_string().as_str()),
        "source_root must stay on the OLD checkout after a failed rebuild from a new one (P2)"
    );

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("DROP TRIGGER fail_logical_symbols;").unwrap();
    }
    IndexDatabase::rebuild(&config2).unwrap();
    assert_eq!(
        persisted_root(&db_path).as_deref(),
        Some(root2.display().to_string().as_str()),
        "the retry publishes the new root together with the flip"
    );

    let _ = fs::remove_dir_all(&root2);
    let _ = fs::remove_dir_all(root);
}

/// A git fixture that is also a Cargo crate (`[package] name = crate_name` + `src/lib.rs`), so
/// `refresh_packages` records a real `packages` row + `repo_meta.local_crate_roots`.
fn cargo_generation_fixture(crate_name: &str) -> (PathBuf, Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), format!("[package]\nname=\"{crate_name}\"\n")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn lib_fn() -> u32 { 1 }\n").unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    (root, config)
}

/// Batch 6 #4 (package roots ride the flip): `refresh_packages` writes the generation-less
/// `packages` rows + `repo_meta.local_crate_roots` INSIDE the terminal publish transaction now
/// (folded into `finalize_base_edges`), so a rebuild that observes CHANGED Cargo roots then fails
/// pre-flip leaves both on the OLD state — a concurrent reader/heal never resolves old-generation
/// files against the new package map, and a failed tail strands nothing.
#[test]
fn a_tail_failure_leaves_package_roots_on_the_old_state() {
    let (root, config) = cargo_generation_fixture("alpha");
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    let local_roots = |db_path: &Path| -> Option<String> {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        crate::index::repo_meta(&conn, &repo_id, "local_crate_roots").unwrap()
    };
    assert_eq!(
        local_roots(&db_path).as_deref(),
        Some("alpha"),
        "the first rebuild recorded the alpha crate root"
    );

    // Change the crate root (dirty Cargo.toml edit — the rebuild reads disk), then fail the next
    // rebuild in its terminal transaction.
    fs::write(root.join("Cargo.toml"), "[package]\nname=\"beta\"\n").unwrap();
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_logical_symbols BEFORE INSERT ON logical_symbols
             BEGIN SELECT RAISE(ABORT, 'injected logical-symbol failure'); END;",
        )
        .unwrap();
    }
    assert!(IndexDatabase::rebuild(&config).is_err(), "the tail failure aborts the rebuild");
    assert_eq!(
        local_roots(&db_path).as_deref(),
        Some("alpha"),
        "local_crate_roots must stay on the OLD roots after a failed rebuild (batch 6 #4) — the \
         package-root write rides the flip, so it rolled back with the terminal transaction"
    );

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("DROP TRIGGER fail_logical_symbols;").unwrap();
    }
    IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        local_roots(&db_path).as_deref(),
        Some("beta"),
        "the retry publishes the new crate root together with the flip"
    );

    let _ = fs::remove_dir_all(root);
}

/// Batch 6 #3 (overlay package roots carried across a base HEAD advance): a full rebuild that
/// advances the base HEAD while a linked-worktree overlay is carried forward moves the overlay's
/// FILE rows by `worktree_id` (commit-agnostic) but leaves its `packages` rows keyed to the OLD
/// base commit — so the terminal-txn edge re-resolution (view installed at the NEW HEAD) would read
/// the overlay's imports against NO package map and resolve them fall-open.
/// `carry_forward_overlay_packages` re-keys them to the rebuilt HEAD first.
#[test]
fn a_head_advancing_rebuild_carries_overlay_package_roots_onto_the_new_base_commit() {
    let (root, config) = cargo_generation_fixture("alpha");
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);
    let (old_head, _) = crate::index::resolve_git_context(&root);

    // A linked worktree that CHANGES a file (so the overlay refresh runs and writes its own
    // `packages` rows from the linked checkout's manifest).
    let linked =
        root.parent().unwrap().join(format!("{}-wt", root.file_name().unwrap().to_string_lossy()));
    run_git(&root, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/caller.rs"), "pub fn overlay_caller() -> u32 { 7 }\n").unwrap();
    let mut db = IndexDatabase::open_config(&config).unwrap();
    let overlay_wt = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap().worktree_id;
    drop(db);
    assert!(!overlay_wt.is_empty(), "the linked worktree produced an overlay scope");

    let overlay_pkg_commit = |db_path: &Path| -> Option<String> {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT DISTINCT commit_sha FROM packages WHERE repo_id = ?1 AND worktree_id = ?2",
            rusqlite::params![repo_id, overlay_wt],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .unwrap()
    };
    assert_eq!(
        overlay_pkg_commit(&db_path).as_deref(),
        Some(old_head.as_str()),
        "the overlay indexed its packages at the old base HEAD"
    );

    // Advance the base HEAD, then full-rebuild: the overlay is carried forward and its packages
    // re-keyed to the new HEAD (batch 6 #3).
    fs::write(root.join("src/added.rs"), "pub fn added() -> u32 { 9 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "advance"]);
    let (new_head, _) = crate::index::resolve_git_context(&root);
    assert_ne!(old_head, new_head, "HEAD advanced");
    IndexDatabase::rebuild(&config).unwrap();

    assert_eq!(
        overlay_pkg_commit(&db_path).as_deref(),
        Some(new_head.as_str()),
        "the carried overlay's packages must be re-keyed to the rebuilt HEAD (batch 6 #3), so \
         load_package_roots_into_scope finds the overlay package map under the new base commit"
    );

    let _ = fs::remove_dir_all(&linked);
    let _ = fs::remove_dir_all(root);
}

/// Batch 6 (count-scoping class, the adversary's repro): during a DEAD-GENERATION window (two
/// rebuilds, no gc) the graph-traversal SUMMARY counts must equal the view-joined ROWS — an
/// unscoped `COUNT(*) FROM edges` would double `total_matching_edges` over the superseded
/// generation and mis-flip `truncated`, and an unscoped `unique_symbol_name` would see the live
/// name twice and suppress a live hop. The row query was always view-joined; the counts now are
/// too.
#[test]
fn graph_traversal_summary_counts_match_the_live_rows_during_a_dead_generation_window() {
    let (root, config) = generation_fixture(&[(
        "lib.rs",
        "pub fn callee() -> u32 { 1 }\npub fn caller() -> u32 { callee() }\n",
    )]);
    IndexDatabase::rebuild(&config).unwrap();
    // Second rebuild WITHOUT gc: the first generation's edges/symbols linger (the dead-generation
    // window the adversary's repro exploits).
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // Confirm we ARE in a dead-generation window: the repo carries a `callee` symbol at a dead
    // generation too, so an unscoped COUNT would double.
    let dead_callee: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM main.symbols s JOIN main.files f ON f.id = s.file_id
             WHERE f.repo_id = ?1 AND s.name = 'callee' AND f.generation != ?2",
            rusqlite::params![db.active_repo_id, db.active_generation],
            |r| r.get(0),
        )
        .unwrap();
    assert!(dead_callee > 0, "the second rebuild leaves a dead generation's `callee` behind");

    let options = crate::query::graph::GraphTraversalOptions::default();
    let rows = db.find_callers("callee", 50).unwrap();
    let summary =
        crate::query::graph::traversal_summary(conn, "callee", true, 50, &options, rows.len())
            .unwrap();
    assert_eq!(rows.len(), 1, "exactly one LIVE caller of callee");
    assert_eq!(
        summary.total_matching_edges,
        rows.len() as u64,
        "the traversal summary must count only the live generation's edges (batch 6 \
         count-scoping): during a dead-generation window an unscoped COUNT doubles \
         total_matching_edges over the view-joined rows"
    );
    assert!(!summary.truncated, "a live result that fits the limit must not be marked truncated");
    assert!(
        crate::query::graph::unique_symbol_name(conn, "callee").unwrap(),
        "unique_symbol_name must count only the live generation (batch 6): a dead-generation \
         duplicate would read count == 2, disable the short-name fallback, and suppress a live hop"
    );

    drop(db);
    let _ = fs::remove_dir_all(root);
}

/// Batch 7 P2 (incremental.rs, standalone finalize): the public `index_targets()` entry shares
/// the full rebuild's wave loop (`graph.is_some()`), so its parse/read failures STAGE into
/// `temp.rebuild_parser_failures` — and the standalone finalize must PUBLISH them (it has no
/// terminal flip to defer to; failures publish atomically with its own edges). Before the fix a
/// standalone pass's failing files silently vanished from `parser_failures`. Also pins the
/// standalone twin's self-sufficiency: it creates its own chunk-text scratch table and trains the
/// first-index dict (the pre-fix path errored "no such table: temp.rebuild_chunk_text" on any
/// fresh, dict-less index).
#[test]
fn standalone_index_targets_publishes_staged_parser_failures_immediately() {
    let (root, config) = generation_fixture(&[
        ("good.rs", "pub fn fine() -> u32 { 1 }\n"),
        ("parsefail.rs", "pub fn broken( {\n"),
    ]);
    // A PREPARE failure too (invalid UTF-8 — records a failure with NO file row), so both staging
    // arms are exercised.
    fs::write(root.join("src/binfail.rs"), [0xFF, 0xFE, 0x00, 0x9F, 0x92]).unwrap();

    // The standalone driver: create/adopt/scope, then index_targets — NO rebuild anywhere.
    let mut db = IndexDatabase::create_or_migrate(&config.database).unwrap();
    db.adopt_repo_from_config(&config, crate::index::lifecycle::AdoptIntent::Indexing).unwrap();
    let (sha, wt) = crate::index::resolve_git_context(&root);
    db.set_context(&sha, &wt).unwrap();
    db.index_targets(&config).unwrap();

    let db_path = config.database.clone();
    let repo_id = db.active_repo_id.clone();
    assert_eq!(
        failure_count_for_path(&db_path, &repo_id, "src/parsefail.rs"),
        1,
        "a standalone index_targets pass must publish its staged PARSE failure immediately (batch \
         7) — not leave it in the temp table for some later rebuild"
    );
    assert_eq!(
        failure_count_for_path(&db_path, &repo_id, "src/binfail.rs"),
        1,
        "the staged PREPARE failure (no file row) publishes too"
    );
    assert_eq!(failure_count_for_path(&db_path, &repo_id, "src/good.rs"), 0);
    // Self-sufficiency: the standalone finalize built the durable chunk-text store (first-index
    // dict training) — every indexed chunk has its compressed text row.
    let missing_text: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.chunks
             WHERE id NOT IN (SELECT chunk_id FROM main.chunk_text)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(missing_text, 0, "the standalone finalize trains the dict and stores chunk text");
    // The logical-symbol fold ran (the open-time heal re-derives edges only, so the finalize must
    // fold or the fresh symbols stay invisible to symbol_lookup/graph nav).
    let logical_fine: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.logical_symbols WHERE repo_id = ?1 AND logical_name = \
             'fine'",
            [&repo_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(logical_fine, 1, "the standalone finalize folds logical symbols (batch 7 twin)");

    drop(db);
    let _ = fs::remove_dir_all(root);
}

/// Batch 7 P2 (rebuild.rs, overlay package carry vs a fresh refresh): an overlay that ALREADY
/// refreshed after the base HEAD moved holds `packages` rows at the NEW `(commit_sha,
/// worktree_id)` while the old commit's rows linger. The carry's blind re-key then collided with
/// the fresh rows under `UNIQUE(repo_id, manifest_dir, commit_sha, worktree_id)` and ABORTED the
/// whole rebuild. Superseded old rows are deleted instead; the rebuild completes and the overlay
/// package map ends deduplicated at the rebuilt HEAD.
#[test]
fn a_rebuild_after_an_overlay_already_refreshed_at_the_new_head_does_not_collide() {
    let (root, config) = cargo_generation_fixture("alpha");
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);

    // Overlay refresh #1 at H1.
    let linked =
        root.parent().unwrap().join(format!("{}-wt", root.file_name().unwrap().to_string_lossy()));
    run_git(&root, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/caller.rs"), "pub fn overlay_caller() -> u32 { 7 }\n").unwrap();
    let mut db = IndexDatabase::open_config(&config).unwrap();
    let overlay_wt = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap().worktree_id;
    drop(db);
    assert!(!overlay_wt.is_empty());

    // Base HEAD advances to H2, then the overlay refreshes AGAIN — its packages land at (H2, wt)
    // while the (H1, wt) rows linger (refresh_packages deletes only its own scope).
    fs::write(root.join("src/added.rs"), "pub fn added() -> u32 { 9 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "advance"]);
    let (new_head, _) = crate::index::resolve_git_context(&root);
    fs::write(linked.join("src/caller.rs"), "pub fn overlay_caller() -> u32 { 8 }\n").unwrap();
    let mut db = IndexDatabase::open_config(&config).unwrap();
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    drop(db);
    let overlay_pkg_commits = |db_path: &Path| -> i64 {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT COUNT(DISTINCT commit_sha) FROM packages WHERE repo_id = ?1 AND worktree_id = \
             ?2",
            rusqlite::params![repo_id, overlay_wt],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        overlay_pkg_commits(&db_path),
        2,
        "precondition: fresh rows at the new HEAD coexist with the old commit's lingering rows"
    );

    // The full rebuild must COMPLETE (the pre-fix blind re-key aborted on the UNIQUE here) and
    // leave exactly one overlay package row per manifest_dir, at the rebuilt HEAD.
    IndexDatabase::rebuild(&config).unwrap();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let (rows, distinct_dirs, at_new_head): (i64, i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COUNT(DISTINCT manifest_dir),
                    SUM(CASE WHEN commit_sha = ?3 THEN 1 ELSE 0 END)
             FROM packages WHERE repo_id = ?1 AND worktree_id = ?2",
            rusqlite::params![repo_id, overlay_wt, new_head],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(rows, distinct_dirs, "exactly one overlay package row per manifest_dir (batch 7)");
    assert_eq!(rows, at_new_head, "every surviving overlay package row is at the rebuilt HEAD");
    assert!(rows > 0, "the overlay package map survived the carry");

    let _ = fs::remove_dir_all(&linked);
    let _ = fs::remove_dir_all(root);
}

/// Batch 7 (the re-key's second collision shape): overlay rows at TWO different STALE commits
/// (no fresh row at the new HEAD) would BOTH re-key to the same new key and collide with each
/// other. The carry keeps only the most recent stale row per manifest_dir (highest rowid) and
/// re-keys it.
#[test]
fn a_rebuild_dedupes_multi_stale_overlay_package_rows_before_the_re_key() {
    let (root, config) = cargo_generation_fixture("alpha");
    IndexDatabase::rebuild(&config).unwrap();
    let db_path = config.database.clone();
    let repo_id = resolve_repo_id(&db_path, &root);

    let linked =
        root.parent().unwrap().join(format!("{}-wt", root.file_name().unwrap().to_string_lossy()));
    run_git(&root, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/caller.rs"), "pub fn overlay_caller() -> u32 { 7 }\n").unwrap();
    let mut db = IndexDatabase::open_config(&config).unwrap();
    let overlay_wt = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap().worktree_id;
    drop(db);

    // A second, MORE RECENT stale row for the same manifest_dir at another dead commit (the
    // two-stale-refreshes-two-HEAD-moves-ago shape), marked so the survivor is identifiable.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO packages(manifest_dir, commit_sha, worktree_id, local_roots_json, \
             repo_id)
             SELECT manifest_dir, 'stalecafe', worktree_id, '[\"survivor_marker\"]', repo_id
             FROM packages WHERE repo_id = ?1 AND worktree_id = ?2",
            rusqlite::params![repo_id, overlay_wt],
        )
        .unwrap();
    }

    // Advance the base HEAD WITHOUT another overlay refresh, then rebuild: both stale commits'
    // rows meet the re-key; the pre-fix UPDATE collided them into one UNIQUE key and aborted.
    fs::write(root.join("src/added.rs"), "pub fn added() -> u32 { 9 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "advance"]);
    let (new_head, _) = crate::index::resolve_git_context(&root);
    IndexDatabase::rebuild(&config).unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let survivors: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT commit_sha, local_roots_json FROM packages
                 WHERE repo_id = ?1 AND worktree_id = ?2",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![repo_id, overlay_wt], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(survivors.len(), 1, "one overlay package row survives the multi-stale dedup");
    assert_eq!(survivors[0].0, new_head, "the survivor is re-keyed to the rebuilt HEAD");
    assert_eq!(
        survivors[0].1, "[\"survivor_marker\"]",
        "the MOST RECENT stale row (highest rowid) wins the dedup"
    );

    let _ = fs::remove_dir_all(&linked);
    let _ = fs::remove_dir_all(root);
}

/// Batch 6 (concurrency HIGH): `rag-rat init`'s `setup_index` reaches the rebuild through
/// `index_discover`'s missing-DB fallback — a FLOCK-LESS production writer before batch 6.
/// `rebuild_with_progress` now takes the per-repo write flock ITSELF, so even that entry holds it:
/// a flock-holding collector racing the staging SERIALIZES instead of cascading the mid-flight
/// generation, and the published generation is never empty (the adversary's repro shape, promoted).
#[test]
fn a_lockless_init_rebuild_serializes_a_concurrent_flock_gc() {
    let (root, config) = generation_fixture(&[
        ("a.rs", "pub fn a() -> u32 { 1 }\n"),
        ("b.rs", "pub fn b() -> u32 { 2 }\n"),
    ]);
    let db_path = config.database.clone();

    // Pause the FIRST index between waves — driven through `index_discover` (the init path), NOT
    // the library `rebuild`: a fresh DB → `index_incremental`'s missing-DB fallback → the SAME
    // `rebuild_with_progress`, which acquires the flock at its top (before the barrier fires).
    let (reached_tx, reached_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let _guard = {
        let paused = AtomicBool::new(false);
        let reached_tx = Mutex::new(reached_tx);
        let resume_rx = Mutex::new(resume_rx);
        crate::index::rebuild::set_after_wave_commit(
            &config.database,
            Arc::new(move || {
                if !paused.swap(true, Ordering::SeqCst) {
                    reached_tx.lock().unwrap().send(()).unwrap();
                    resume_rx.lock().unwrap().recv().unwrap();
                }
            }),
        )
    };
    let discover_config = config.clone();
    let handle = std::thread::spawn(move || {
        IndexDatabase::index_discover(&discover_config).unwrap();
    });
    reached_rx.recv().unwrap();

    // A production collector takes the per-repo flock first — held by the paused init rebuild — so
    // it SERIALIZES rather than cascading the in-progress staging (the exact race that
    // published empty).
    let lock_repo = rag_rat_base::locks::write_lock_repo_id(&config);
    let contended = rag_rat_base::locks::WriteLock::acquire_timeout(
        &config.database,
        &lock_repo,
        std::time::Duration::from_millis(150),
    )
    .unwrap();
    assert!(
        contended.is_none(),
        "a flock-taking collector must block while the lockless init rebuild is mid-flight"
    );

    resume_tx.send(()).unwrap();
    handle.join().unwrap();

    // The published generation is NON-EMPTY: the collector could not cascade the staging.
    assert_eq!(
        reader_scoped_file_count(&db_path, &root),
        2,
        "the init-path rebuild published a non-empty generation despite the racing collector"
    );
    let _ = fs::remove_dir_all(root);
}
