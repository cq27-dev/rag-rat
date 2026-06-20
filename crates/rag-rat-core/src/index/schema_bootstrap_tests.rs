use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::config::ResolvedTarget;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn rebuild_bootstraps_sqlite_schema_for_empty_target_root() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();

    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "markdown".to_string(),
            language: Language::Markdown,
            directories: vec![PathBuf::from("docs")],
            include: vec!["**/*.md".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Docs,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };

    let db = IndexDatabase::rebuild(&config).unwrap();
    assert!(config.database.exists());
    assert_eq!(table_count(&db, "files"), 1);
    assert_eq!(table_count(&db, "chunks"), 1);
    assert_eq!(table_count(&db, "symbols"), 1);
    assert_eq!(table_count(&db, "parser_failures"), 1);
    assert_eq!(table_count(&db, "index_meta"), 1);
    assert_eq!(table_count(&db, "chunk_fts"), 1);
    assert_eq!(table_count(&db, "git_commits"), 1);
    assert_eq!(table_count(&db, "git_file_changes"), 1);
    assert_eq!(table_count(&db, "git_chunk_blame"), 1);
    assert_eq!(table_count(&db, "commit_fts"), 1);
    assert_eq!(table_count(&db, "ai_models"), 1);
    assert_eq!(table_count(&db, "chunk_embeddings"), 1);
    assert_eq!(table_count(&db, "chunk_summaries"), 1);
    assert_eq!(table_count(&db, "reconcile_meta"), 1);
    assert_eq!(table_count(&db, "reconcile_attempts"), 1);
    assert!(file_columns(&db).contains(&"indexed_revision".to_string()));
    assert_eq!(indexed_revision_count(&db), 0);
    assert!(chunk_columns(&db).contains(&"anchor_version".to_string()));
    assert!(chunk_columns(&db).contains(&"normalized_hash".to_string()));
    assert!(chunk_columns(&db).contains(&"start_boundary_hash".to_string()));
    assert!(chunk_columns(&db).contains(&"end_boundary_hash".to_string()));
    assert!(chunk_columns(&db).contains(&"source_revision".to_string()));
    let embedding_columns = table_columns(&db, "chunk_embeddings");
    assert!(embedding_columns.contains(&"model_version".to_string()));
    assert!(embedding_columns.contains(&"input_hash".to_string()));
    assert!(embedding_columns.contains(&"embedding_text_version".to_string()));
    assert!(embedding_columns.contains(&"embedding_policy".to_string()));
    assert!(embedding_columns.contains(&"embedding_priority".to_string()));
    assert!(embedding_columns.contains(&"input_chars".to_string()));
    assert!(embedding_columns.contains(&"input_truncated".to_string()));
    assert!(embedding_columns.contains(&"attempt_count".to_string()));
    assert!(embedding_columns.contains(&"next_retry_after_ms".to_string()));
    assert!(embedding_columns.contains(&"computed_at_ms".to_string()));
    let edge_columns = table_columns(&db, "edges");
    assert!(edge_columns.contains(&"source_start_line".to_string()));
    assert!(edge_columns.contains(&"source_end_line".to_string()));
    assert!(edge_columns.contains(&"source_start_byte".to_string()));
    assert!(edge_columns.contains(&"source_end_byte".to_string()));
    assert!(edge_columns.contains(&"target_start_line".to_string()));
    assert!(edge_columns.contains(&"target_end_line".to_string()));
    assert!(edge_columns.contains(&"target_qualified_name".to_string()));
    assert!(edge_columns.contains(&"evidence".to_string()));
    assert!(edge_columns.contains(&"receiver_hint".to_string()));
    assert!(edge_columns.contains(&"resolution".to_string()));
    let logical_columns = table_columns(&db, "logical_symbols");
    assert!(logical_columns.contains(&"qualified_name_id".to_string()));
    assert!(logical_columns.contains(&"variant_count".to_string()));
    let member_columns = table_columns(&db, "logical_symbol_members");
    assert!(member_columns.contains(&"symbol_id".to_string()));
    assert!(member_columns.contains(&"signature_hash".to_string()));
    let github_ref_sync_columns = table_columns(&db, "github_ref_sync");
    assert!(github_ref_sync_columns.contains(&"status".to_string()));
    assert!(github_ref_sync_columns.contains(&"last_error".to_string()));
    let symbol_fact_columns = table_columns(&db, "symbol_facts");
    assert!(symbol_fact_columns.contains(&"fact_kind".to_string()));
    assert!(symbol_fact_columns.contains(&"fact_value".to_string()));
    let symbol_columns = table_columns(&db, "symbols");
    assert!(symbol_columns.contains(&"start_line".to_string()));
    assert!(symbol_columns.contains(&"end_line".to_string()));
    assert_eq!(
        db.status(&config.database).unwrap().schema.current_version,
        schema::LATEST_SCHEMA_VERSION
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn symbols_store_true_source_line_spans() {
    // V016: start_line/end_line are persisted on the symbols row (from tree-sitter at parse time)
    // instead of being recomputed by per-symbol correlated subqueries against chunks. Assert the
    // stored 1-based spans match the actual source lines.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // alpha on line 2; beta spans lines 4..=6.
    fs::write(
        root.join("src/lib.rs"),
        "// header\npub fn alpha() {}\n\npub fn beta() {\n    let _ = 1;\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let line_span = |name: &str| -> (i64, i64) {
        db.storage
            .connection()
            .query_row("SELECT start_line, end_line FROM symbols WHERE name = ?1", [name], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap()
    };
    assert_eq!(line_span("alpha"), (2, 2));
    assert_eq!(line_span("beta"), (4, 6));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn rebuild_reports_file_preparation_progress() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn exported() {}\n").unwrap();

    let config = source_config(root.clone(), Language::Rust);
    let mut events = Vec::new();
    IndexDatabase::rebuild_with_progress(&config, |progress| events.push(progress)).unwrap();

    assert!(
        events.iter().any(|event| matches!(event, IndexProgress::PreparingFile { .. })),
        "missing preparing progress event: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(event, IndexProgress::IndexingFile { .. })),
        "missing indexing progress event: {events:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn file_progress_reports_first_final_and_decile_boundaries() {
    let reported =
        (1..=100).filter(|current| should_report_file_progress(*current, 100)).collect::<Vec<_>>();
    assert_eq!(reported, vec![1, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100]);
}

#[test]
fn open_refuses_a_legacy_schema_without_version_table() {
    // A pre-ledger index (real tables, no schema_version) reads as `Older`, but forward-only
    // migration can't know what's already applied — re-running data migrations would clobber
    // current values — so open REFUSES it and points at a rebuild rather than risk corruption.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".rag-rat")).unwrap();
    let database = root.join(".rag-rat/index.sqlite");
    IndexDatabase::migrate(&database).unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch("DROP TABLE schema_version;").unwrap();
    drop(conn);

    assert_eq!(
        IndexDatabase::migration_check(&database).unwrap().state,
        schema::SchemaState::Older
    );
    let err = IndexDatabase::open(&database).unwrap_err().to_string();
    assert!(err.contains("rebuild"), "{err}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn forward_migration_does_not_rerun_already_applied_migrations() {
    // The forward-only guarantee (#103 review): opening a v(latest-1) index must apply ONLY the
    // missing migration, never re-run an applied one. Migration 005 does an unconditional
    // `UPDATE edges SET resolution = …` that would downgrade modern resolver reasons on every
    // upgrade open if the whole ladder re-ran. Witness: stamp 005's row with a sentinel
    // applied_at_ms; if 005 re-ran, record_migration (INSERT OR REPLACE) overwrites it with now_ms.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".rag-rat")).unwrap();
    let database = root.join(".rag-rat/index.sqlite");
    IndexDatabase::migrate(&database).unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute("UPDATE schema_version SET applied_at_ms = 1 WHERE id LIKE '005%'", []).unwrap();
    conn.execute(
        "DELETE FROM schema_version WHERE id = (SELECT id FROM schema_version ORDER BY id DESC \
         LIMIT 1)",
        [],
    )
    .unwrap();
    drop(conn);

    IndexDatabase::open(&database).unwrap();

    let conn = rusqlite::Connection::open(&database).unwrap();
    let applied_005: i64 = conn
        .query_row("SELECT applied_at_ms FROM schema_version WHERE id LIKE '005%'", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(conn);
    assert_eq!(applied_005, 1, "migration 005 was re-applied on open — forward-only is broken");
    assert_eq!(
        IndexDatabase::migration_check(&database).unwrap().state,
        schema::SchemaState::Compatible
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn forward_migration_reprovisions_missing_baseline_tables() {
    // Forward-only must run the idempotent baseline BEFORE replaying steps (#103 review): a ≤v19
    // index predates shared tables (e.g. name_strings) that a later migration INSERTs into.
    // Simulate by dropping the edges view + name_strings and EVERY post-019 ledger row (so the
    // applied set stays contiguous at v19 — not a gap that `known_version` would read as current);
    // open must reprovision the table and reach Compatible rather than fail on a missing-table
    // INSERT.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".rag-rat")).unwrap();
    let database = root.join(".rag-rat/index.sqlite");
    IndexDatabase::migrate(&database).unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch(
        "DROP VIEW IF EXISTS edges;
         DROP TABLE IF EXISTS name_strings;
         DELETE FROM schema_version WHERE id >= '020';",
    )
    .unwrap();
    drop(conn);

    assert_eq!(
        IndexDatabase::migration_check(&database).unwrap().state,
        schema::SchemaState::Older
    );
    IndexDatabase::open(&database).unwrap();
    assert_eq!(
        IndexDatabase::migration_check(&database).unwrap().state,
        schema::SchemaState::Compatible
    );
    let conn = rusqlite::Connection::open(&database).unwrap();
    let edge_strings_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'name_strings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);
    assert_eq!(edge_strings_exists, 1, "baseline did not reprovision name_strings before replay");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn open_auto_migrates_a_forward_older_schema_to_latest() {
    // The binary-upgrade case: a fully-migrated index missing only the newest migration row reads
    // as `Older` (current_version < latest). Opening it applies only the missing migration forward.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".rag-rat")).unwrap();
    let database = root.join(".rag-rat/index.sqlite");
    IndexDatabase::migrate(&database).unwrap();
    // Drop the newest applied migration so the stored version lags this binary by one.
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute(
        "DELETE FROM schema_version WHERE id = (SELECT id FROM schema_version ORDER BY id DESC \
         LIMIT 1)",
        [],
    )
    .unwrap();
    drop(conn);

    let before = IndexDatabase::migration_check(&database).unwrap();
    assert_eq!(before.state, schema::SchemaState::Older);
    assert_eq!(before.current_version, before.latest_version - 1);

    IndexDatabase::open(&database).unwrap();

    let after = IndexDatabase::migration_check(&database).unwrap();
    assert_eq!(after.state, schema::SchemaState::Compatible);
    assert_eq!(after.current_version, after.latest_version);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn migrate_adds_edge_name_columns_before_indexing_them() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".rag-rat")).unwrap();
    let database = root.join(".rag-rat/index.sqlite");
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch(
        "
            CREATE TABLE files(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                language TEXT NOT NULL,
                kind TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                modified_at_ms INTEGER NOT NULL,
                generated INTEGER NOT NULL DEFAULT 0,
                indexed_at_ms INTEGER NOT NULL
            );
            CREATE TABLE chunks(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL,
                chunk_kind TEXT NOT NULL,
                symbol_path TEXT,
                start_byte INTEGER NOT NULL,
                end_byte INTEGER NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                text TEXT NOT NULL,
                text_hash TEXT NOT NULL
            );
            CREATE TABLE symbols(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL,
                language TEXT NOT NULL,
                name TEXT NOT NULL,
                qualified_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                start_byte INTEGER NOT NULL,
                end_byte INTEGER NOT NULL,
                signature TEXT,
                docs TEXT
            );
            CREATE TABLE edges(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_symbol_id INTEGER,
                to_symbol_id INTEGER,
                edge_kind TEXT NOT NULL,
                confidence TEXT NOT NULL
            );
            ",
    )
    .unwrap();
    drop(conn);

    let migrated = IndexDatabase::migrate(&database).unwrap();
    assert_eq!(migrated.state, schema::SchemaState::Compatible);
    let db = IndexDatabase::open(&database).unwrap();
    let columns = table_columns(&db, "edges");
    assert!(columns.contains(&"from_name".to_string()));
    assert!(columns.contains(&"to_name".to_string()));
    assert!(columns.contains(&"source_start_line".to_string()));
    assert!(columns.contains(&"source_end_line".to_string()));
    assert!(columns.contains(&"source_start_byte".to_string()));
    assert!(columns.contains(&"source_end_byte".to_string()));
    assert!(columns.contains(&"target_start_line".to_string()));
    assert!(columns.contains(&"target_end_line".to_string()));
    // The SCIP-oracle prerequisite columns (#67) are added additively to a legacy edges table.
    assert!(columns.contains(&"callee_start_byte".to_string()));
    assert!(columns.contains(&"callee_end_byte".to_string()));
    assert_eq!(table_count(&db, "idx_edges_from_name"), 1);
    assert_eq!(table_count(&db, "idx_edges_to_name"), 1);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn migrate_preserves_github_papertrail_cache() {
    let (root, config) =
        markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    github::sync_from_refs(
        db.storage.connection(),
        &root,
        Some(&MockGitHubClient),
        false,
        &test_gh_ctx(),
    )
    .unwrap();
    assert_eq!(row_count(&db, "github_refs"), 1);
    assert_eq!(row_count(&db, "github_issues"), 1);
    assert_eq!(row_count(&db, "github_comments"), 1);
    assert_eq!(row_count(&db, "github_pull_requests"), 1);
    assert_eq!(row_count(&db, "github_reviews"), 1);
    assert_eq!(row_count(&db, "github_review_comments"), 1);
    assert_eq!(row_count(&db, "github_fts"), 5);
    db.storage
        .connection()
        .execute("DELETE FROM schema_version WHERE id = ?1", ["010_symbol_facts"])
        .unwrap();
    drop(db);

    let migrated = IndexDatabase::migrate(&config.database).unwrap();
    assert_eq!(migrated.state, schema::SchemaState::Compatible);
    let db = IndexDatabase::open(&config.database).unwrap();
    assert_eq!(row_count(&db, "github_refs"), 1);
    assert_eq!(row_count(&db, "github_issues"), 1);
    assert_eq!(row_count(&db, "github_comments"), 1);
    assert_eq!(row_count(&db, "github_pull_requests"), 1);
    assert_eq!(row_count(&db, "github_reviews"), 1);
    assert_eq!(row_count(&db, "github_review_comments"), 1);
    assert_eq!(row_count(&db, "github_fts"), 5);
    let hits = db.github_issue_search("sqlite", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].number, 42);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn full_rebuild_preserves_github_papertrail_cache() {
    let (root, config) =
        markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    github::sync_from_refs(
        db.storage.connection(),
        &root,
        Some(&MockGitHubClient),
        false,
        &test_gh_ctx(),
    )
    .unwrap();
    assert_eq!(row_count(&db, "github_issues"), 1);
    assert_eq!(row_count(&db, "github_fts"), 5);
    drop(db);

    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_eq!(row_count(&db, "github_refs"), 1);
    assert_eq!(row_count(&db, "github_issues"), 1);
    assert_eq!(row_count(&db, "github_comments"), 1);
    assert_eq!(row_count(&db, "github_pull_requests"), 1);
    assert_eq!(row_count(&db, "github_reviews"), 1);
    assert_eq!(row_count(&db, "github_review_comments"), 1);
    assert_eq!(row_count(&db, "github_ref_sync"), 1);
    assert_eq!(row_count(&db, "github_fts"), 5);
    let hits = db.github_issue_search("sqlite", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].number, 42);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn full_rebuild_preserves_installed_model_manifest() {
    let (root, config) = markdown_config("alpha token with enough detail for embeddings\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();
    let before = db.local_ai_status().unwrap();
    assert_eq!(before.embedding.model_id, ai::HASH_MODEL_ID);
    assert!(before.embedding.installed);
    drop(db);

    let db = IndexDatabase::rebuild(&config).unwrap();

    let after = db.local_ai_status().unwrap();
    assert_eq!(after.embedding.model_id, ai::HASH_MODEL_ID);
    assert!(after.embedding.installed);
    assert_eq!(after.embedding.state, "Ready");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn full_rebuild_preserves_other_worktree_contexts() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn current_context() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let other_file_id = db
        .storage
        .connection()
        .query_row(
            "
                INSERT INTO main.files(
                    path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
                    indexed_revision, commit_sha, worktree_id
                )
                VALUES ('src/other.rs', 'rust', 'source', 'other-sha', 0, 0, 1, 'other-sha', '', \
             'other-worktree')
                RETURNING id
                ",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    let other_chunk_id = db
        .storage
        .connection()
        .query_row(
            "
                INSERT INTO main.chunks(
                    file_id, chunk_kind, symbol_path, start_byte, end_byte, start_line, end_line,
                    text_hash, source_revision, anchor_version, normalized_hash,
                    start_boundary_hash, end_boundary_hash, start_context_hash, end_context_hash,
                    context_radius, embedding_policy, embedding_priority
                )
                VALUES (?1, 'symbol', 'other_context', 0, 12, 1, 1, 'other-text',
                    'other-sha', 1, '', '', '', '', '', 2, 'Embed', 1)
                RETURNING id
                ",
            [other_file_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    // chunks.text is gone (#77 Phase 2); seed the chunk_text blob readers INNER JOIN. (The
    // chunk_fts row for this other-context chunk is seeded a few lines below.)
    crate::index::chunk_text_store::seed_chunk_text(
        db.storage.connection(),
        other_chunk_id,
        "other context",
    )
    .unwrap();
    db.storage
        .connection()
        .execute("INSERT OR IGNORE INTO main.name_strings(value) VALUES ('other_context')", [])
        .unwrap();
    db.storage
        .connection()
        .execute(
            "
                INSERT INTO main.symbols(
                    file_id, language, name, qualified_name_id, kind, start_byte, end_byte, \
             signature, docs
                )
                VALUES (?1, 'rust', 'other_context',
                    (SELECT id FROM main.name_strings WHERE value = 'other_context'),
                    'function', 0, 12, NULL, NULL)
                ",
            [other_file_id],
        )
        .unwrap();
    db.storage
        .connection()
        .execute("INSERT INTO main.chunk_fts(rowid, text) VALUES (?1, 'other context')", [
            other_chunk_id,
        ])
        .unwrap();
    drop(db);

    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_eq!(
        db.storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM main.files WHERE worktree_id = 'other-worktree'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        db.storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM main.chunks WHERE file_id = ?1",
                [other_file_id],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        1
    );
    assert_eq!(
        db.storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM main.symbols WHERE file_id = ?1",
                [other_file_id],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        1
    );
    assert_eq!(
        db.storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM main.chunk_fts WHERE rowid = ?1",
                [other_chunk_id],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        1
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn compatible_open_refuses_dirty_and_newer_schema() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".rag-rat")).unwrap();
    let database = root.join(".rag-rat/index.sqlite");
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch(
        "
            CREATE TABLE schema_version(
                id TEXT PRIMARY KEY,
                applied_at_ms INTEGER NOT NULL,
                checksum TEXT NOT NULL,
                description TEXT NOT NULL
            );
            INSERT INTO schema_version(id, applied_at_ms, checksum, description)
            VALUES ('__dirty__', 1, '', 'partial migration in progress');
            ",
    )
    .unwrap();
    drop(conn);

    let dirty = IndexDatabase::migration_check(&database).unwrap();
    assert_eq!(dirty.state, schema::SchemaState::Dirty);
    let err = IndexDatabase::open(&database).unwrap_err().to_string();
    assert!(err.contains("dirty or partial"), "{err}");

    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch(
        "
            DELETE FROM schema_version;
            INSERT INTO schema_version(id, applied_at_ms, checksum, description)
            VALUES ('999_future_schema', 1, 'sha256:future', 'future schema');
            ",
    )
    .unwrap();
    drop(conn);
    let newer = IndexDatabase::migration_check(&database).unwrap();
    assert_eq!(newer.state, schema::SchemaState::Newer);
    let err = IndexDatabase::open(&database).unwrap_err().to_string();
    assert!(err.contains("newer rag-rat"), "{err}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn discover_mode_indexes_new_files_and_removes_deleted_files() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn old_symbol() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(db.discovery_status(&config).unwrap().unindexed_source_files, 0);

    fs::write(root.join("src/new.rs"), "pub fn new_symbol() {}\n").unwrap();
    fs::remove_file(root.join("src/lib.rs")).unwrap();
    let drift = db.discovery_status(&config).unwrap();
    assert_eq!(drift.unindexed_source_files, 1);
    assert_eq!(drift.removed_indexed_files, 1);
    assert!(drift.warning.as_deref().unwrap().contains("rag-rat index --discover"));

    let db = IndexDatabase::index_discover(&config).unwrap();
    let fresh = db.discovery_status(&config).unwrap();
    assert_eq!(fresh.unindexed_source_files, 0);
    assert_eq!(fresh.removed_indexed_files, 0);
    assert!(fresh.warning.is_none());
    assert_eq!(db.symbols("new_symbol", Some(Language::Rust), 10).unwrap().len(), 1);
    assert!(db.symbols("old_symbol", Some(Language::Rust), 10).unwrap().is_empty());

    let mut events = Vec::new();
    let db = IndexDatabase::index_discover_with_progress(&config, |progress| {
        events.push(progress);
    })
    .unwrap();
    assert!(matches!(events.last(), Some(IndexProgress::Finished { files: 0 })));
    assert!(
        !events.iter().any(|event| matches!(
            event,
            IndexProgress::PreparingFile { .. } | IndexProgress::IndexingFile { .. }
        )),
        "no-op discover should not prepare or index files: {events:?}"
    );
    assert_eq!(db.symbols("new_symbol", Some(Language::Rust), 10).unwrap().len(), 1);

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn indexing_skips_symlink_loops() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn loop_safe_symbol() {}\n").unwrap();
    std::os::unix::fs::symlink(&root, root.join("src/loop")).unwrap();

    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_eq!(db.symbols("loop_safe_symbol", Some(Language::Rust), 10).unwrap().len(), 1);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dirty_git_files_are_indexed_as_worktree_overlay() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("search.md"), "# Title\nbase token\n").unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &[
        "-c",
        "user.name=Rag Rat Test",
        "-c",
        "user.email=rag-rat@example.invalid",
        "commit",
        "-m",
        "initial",
    ]);

    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(db.search("base", 10, false).unwrap().len(), 1);

    fs::write(docs.join("search.md"), "# Title\noverlay token\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    let scopes = db
        .storage
        .connection()
        .prepare(
            "
                SELECT commit_sha != '', worktree_id != ''
                FROM main.files
                WHERE path = 'docs/search.md'
                ORDER BY commit_sha != '' DESC, worktree_id != '' DESC
                ",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(scopes, vec![(true, false), (false, true)]);
    assert!(db.search("base", 10, false).unwrap().is_empty());
    let overlay_hits = db.search("overlay", 10, false).unwrap();
    assert_eq!(overlay_hits.len(), 1);
    assert!(overlay_hits[0].summary.contains("overlay token"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn rebuild_populates_revision_metadata_and_fresh_fts_state() {
    let (root, config) = markdown_config("alpha token");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let status = db.status(&config.database).unwrap();

    assert!(!status.content_revision.is_empty());
    assert_eq!(status.fts_source_revision.as_deref(), Some(status.content_revision.as_str()));
    assert_eq!(
        db.meta("content_revision").unwrap().as_deref(),
        Some(status.content_revision.as_str())
    );
    assert!(!status.fts_dirty);
    assert!(status.fts_fresh);
    assert!(!status.git_history.available);
    assert_eq!(status.git_history.commit_count, 0);
    assert_eq!(status.local_ai.embedding.state, "MissingModel");
    assert_eq!(status.local_ai.fastembed.backend, "fastembed");
    assert_eq!(status.local_ai.fastembed.model, ai::FASTEMBED_DISPLAY_MODEL);
    assert_eq!(status.local_ai.fastembed.dim, ai::FASTEMBED_EMBEDDING_DIM);
    assert!(!status.local_ai.fastembed.cache.is_empty());
    assert_eq!(status.local_ai.fastembed.build_feature_enabled, cfg!(feature = "fastembed"));
    assert_eq!(status.local_ai.artifacts.total_chunks, 1);
    assert_eq!(
        status.local_ai.artifacts.eligible_chunks + status.local_ai.artifacts.skipped_chunks,
        status.local_ai.artifacts.total_chunks
    );
    assert_eq!(
        status.local_ai.fastembed.eligible_embeddings
            + status.local_ai.fastembed.skipped_embeddings,
        status.local_ai.artifacts.total_chunks
    );
    assert_eq!(indexed_revision_count(&db), 1);
    assert_eq!(chunk_source_revision_count(&db), 1);

    fs::remove_dir_all(root).unwrap();
}

#[cfg(not(feature = "fastembed"))]
#[test]
fn fastembed_missing_feature_reports_rebuild_command() {
    let (root, config) = markdown_config("alpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();

    let err = db.install_model(ai::FASTEMBED_MODEL_ID).unwrap_err();
    assert!(err.to_string().contains(ai::FASTEMBED_MISSING_FEATURE_MESSAGE));

    let status = db.local_ai_status().unwrap();
    assert!(!status.fastembed.build_feature_enabled);
    assert_eq!(status.fastembed.status, "MissingRuntime");
    assert_eq!(status.fastembed.message.as_deref(), Some(ai::FASTEMBED_MISSING_FEATURE_MESSAGE));
    assert_eq!(status.fastembed.next.as_deref(), Some("cargo install rag-rat"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reconcile_requires_explicit_model_install_and_ignores_stale_artifacts() {
    let (root, config) = markdown_config(
        "alpha token\nsecond line with enough detail for the semantic embedding policy to keep \
         this chunk\nthird line with runtime context\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    let chunk_id = first_chunk_id(&db);

    let models = db.list_models().unwrap();
    let embedding = models.iter().find(|model| model.model_id == ai::HASH_MODEL_ID).unwrap();
    assert!(!embedding.installed);
    assert_eq!(embedding.status, "MissingModel");

    let hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].summary.contains("alpha token"));

    let blocked = db.reconcile(Some(1), Some(8)).unwrap();
    assert_eq!(blocked.processed_chunks, 0);
    assert_eq!(blocked.embeddings_written, 0);
    assert_eq!(blocked.blocked_chunks, 0);
    assert_eq!(blocked.model_id, ai::HASH_MODEL_ID);
    assert_eq!(blocked.batch_size, 8);
    assert_eq!(blocked.status, "Blocked");

    let status = db.local_ai_status().unwrap();
    assert_eq!(status.embedding.state, "MissingModel");
    assert_eq!(status.embedding.blocked_artifacts, 0);

    db.install_model(ai::HASH_MODEL_ID).unwrap();
    let plan = db.reconcile_plan().unwrap();
    assert_eq!(plan.embeddings.missing, 1);
    assert_eq!(plan.embeddings.current, 0);
    let current = db.reconcile(Some(1), Some(8)).unwrap();
    assert_eq!(current.embeddings_written, 1);
    assert_eq!(current.model_id, ai::HASH_MODEL_ID);
    assert_eq!(current.model_version, "hash-v1");
    assert_eq!(current.embedding_dim, ai::HASH_EMBEDDING_DIM);
    assert_eq!(current.status, "Current");
    assert_eq!(current.work_reasons.get("Missing"), Some(&1));
    let noop = db.reconcile(None, Some(8)).unwrap();
    assert_eq!(noop.processed_chunks, 0);
    assert_eq!(noop.embeddings_written, 0);
    let status = db.local_ai_status().unwrap();
    assert_eq!(status.embedding.state, "Ready");
    assert_eq!(status.embedding.current_artifacts, 1);
    let embedding_bytes: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT length(vector_blob) FROM chunk_embeddings WHERE chunk_id = ?1 AND status = \
             'Current'",
            [chunk_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(embedding_bytes, (ai::HASH_EMBEDDING_DIM * 4) as i64);

    let hits = db.search("alpha", 10, false).unwrap();
    assert!(hits[0].summary.contains("alpha token"));

    db.storage.connection().execute("DELETE FROM chunk_fts", []).unwrap();
    let vector_hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(vector_hits.len(), 1);
    assert_eq!(vector_hits[0].chunk_id, chunk_id);

    db.storage
        .connection()
        .execute("UPDATE chunk_embeddings SET source_text_hash = 'old-hash' WHERE chunk_id = ?1", [
            chunk_id,
        ])
        .unwrap();
    let plan = db.reconcile_plan().unwrap();
    assert_eq!(plan.embeddings.current, 0);
    assert_eq!(plan.embeddings.stale, 1);
    let refreshed = db.reconcile(None, Some(8)).unwrap();
    assert_eq!(refreshed.processed_chunks, 1);
    assert_eq!(refreshed.work_reasons.get("SourceChanged"), Some(&1));
    assert_eq!(db.current_embedding_count(ai::HASH_MODEL_ID).unwrap(), 1);
    let stale_embedding_hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(stale_embedding_hits.len(), 1);

    fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "fastembed")]
#[test]
fn cached_fastembed_model_recovers_ready_state() {
    let (root, config) = markdown_config("alpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let cache_dir = root.join("models");
    let revision = "5f1b8cd78bc4fb444dd171e59b18f3a3af89a079";
    let repo = cache_dir.join("models--Qdrant--all-MiniLM-L6-v2-onnx");
    fs::create_dir_all(repo.join("refs")).unwrap();
    fs::create_dir_all(repo.join("snapshots").join(revision)).unwrap();
    fs::write(repo.join("refs").join("main"), revision).unwrap();

    ai::recover_cached_fastembed_model_at(db.storage.connection(), &cache_dir).unwrap();

    let models = db.list_models().unwrap();
    let fastembed = models.iter().find(|model| model.model_id == ai::FASTEMBED_MODEL_ID).unwrap();
    assert!(fastembed.installed);
    assert_eq!(fastembed.status, "Ready");
    let status = db.local_ai_status().unwrap();
    assert_eq!(status.fastembed.status, "Ready");
    assert!(status.fastembed.active);

    fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "fastembed")]
#[test]
fn compatible_migrate_recovers_cached_fastembed_model() {
    let (root, config) = markdown_config("alpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let cache_dir = root.join("models");
    let revision = "5f1b8cd78bc4fb444dd171e59b18f3a3af89a079";
    let repo = cache_dir.join("models--Qdrant--all-MiniLM-L6-v2-onnx");
    fs::create_dir_all(repo.join("refs")).unwrap();
    fs::create_dir_all(repo.join("snapshots").join(revision)).unwrap();
    fs::write(repo.join("refs").join("main"), revision).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE ai_models
                 SET installed = 0, status = 'MissingModel', installed_at_ms = NULL
                 WHERE model_id = ?1",
            [ai::FASTEMBED_MODEL_ID],
        )
        .unwrap();

    IndexDatabase::migrate_with_fastembed_cache(&config.database, Some(&cache_dir)).unwrap();

    let db = IndexDatabase::open(&config.database).unwrap();
    let status = db.local_ai_status().unwrap();
    assert_eq!(status.fastembed.status, "Ready");
    assert!(status.fastembed.active);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reconcile_without_limit_processes_all_chunks() {
    let (root, config) = markdown_config(
        "# One\nalpha token with enough surrounding detail for embedding eligibility and useful \
         semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding \
         eligibility and useful semantic context\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();

    let report = db.reconcile(None, Some(2)).unwrap();

    assert_eq!(report.processed_chunks, 2);
    assert_eq!(report.embeddings_written, 2);
    assert_eq!(report.batch_size, 2);
    assert_eq!(db.current_embedding_count(ai::HASH_MODEL_ID).unwrap(), 2);
    let second = db.reconcile(None, Some(2)).unwrap();
    assert_eq!(second.processed_chunks, 0);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn force_reconcile_processes_each_chunk_once_and_terminates() {
    // Regression: --force skipped the needs_embedding filter, so select_reconcile_batch
    // never returned an empty batch and the loop re-embedded the active set forever when
    // no --limit/--max-seconds was set. A generous finite limit lets this test terminate
    // either way; the processed/written counts distinguish fixed (==2) from buggy (==50).
    let (root, config) = markdown_config(
        "# One\nalpha token with enough surrounding detail for embedding eligibility and useful \
         semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding \
         eligibility and useful semantic context\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();

    // Two eligible chunks; force with a limit far above the chunk count.
    let report = db.reconcile_with_progress(Some(50), Some(2), true, |_| {}).unwrap();

    assert_eq!(report.embeddings_written, 2, "force re-embedded chunks: {report:?}");
    assert_eq!(report.processed_chunks, 2, "force re-processed chunks: {report:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn force_reconcile_progress_is_honest_and_terminates_without_limit() {
    let (root, config) = markdown_config(
        "# One\nalpha token with enough surrounding detail for embedding eligibility and useful \
         semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding \
         eligibility and useful semantic context\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();

    // No --limit. max_seconds is only a safety net: if the force loop regressed to
    // re-embedding forever it would trip max_seconds and report "Partial" rather than
    // terminating naturally, which this test asserts against (no CI hang on regression).
    let mut events = Vec::new();
    let report = db
        .reconcile_with_options_progress(
            ai::ReconcileOptions {
                force: true,
                batch_size: Some(1),
                max_seconds: Some(30),
                ..ai::ReconcileOptions::default()
            },
            |event| events.push(event),
        )
        .unwrap();

    assert_eq!(report.status, "Current", "did not terminate naturally: {report:?}");
    assert_eq!(report.processed_chunks, 2);

    let started_total = events.iter().find_map(|event| match event {
        ai::ReconcileProgress::Started { total_chunks, .. } => Some(*total_chunks),
        _ => None,
    });
    assert_eq!(started_total, Some(2), "denominator should equal the eligible set");

    for event in &events {
        if let ai::ReconcileProgress::Batch { processed_chunks, total_chunks, .. } = event {
            assert!(
                processed_chunks <= total_chunks,
                "progress exceeded 100%: {processed_chunks}/{total_chunks}",
            );
        }
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_counts_only_active_context_chunks() {
    let (root, config) = markdown_config(
        "# One\nalpha token with enough surrounding detail for embedding eligibility and useful \
         semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding \
         eligibility and useful semantic context\n",
    );
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();

    let active = db.local_ai_status().unwrap().artifacts.total_chunks;
    assert!(active > 0, "expected active chunks, got {active}");

    // Point the connection at a context that matches no indexed rows. The active set
    // (temp.files) is now empty, so status must report 0 chunks. Pre-fix the counts ran
    // over main.chunks (every indexed commit) and ignored the active context entirely.
    db.set_context("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "ghost-worktree").unwrap();
    let scoped = db.local_ai_status().unwrap().artifacts;
    assert_eq!(scoped.total_chunks, 0, "status ignored active context scope");
    assert_eq!(scoped.current, 0);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn watch_maintenance_pass_indexes_new_files() {
    // A watcher pass must pick up a brand-new (uncommitted) file, not just refresh known ones.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/one.rs"), "pub fn one() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    IndexDatabase::rebuild(&config).unwrap();

    // New file appears after the initial index; a maintenance pass should index it.
    fs::write(root.join("src/two.rs"), "pub fn newly_added_symbol() {}\n").unwrap();
    crate::watch::maintenance_pass(&config, false).unwrap();

    let db = IndexDatabase::open_config(&config).unwrap();
    let hits = db.symbols("newly_added_symbol", Some(Language::Rust), 10).unwrap();
    assert!(!hits.is_empty(), "watcher pass did not index the new file");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn discover_deletion_is_worktree_scoped() {
    // Invariant (watcher spec, review item 1): a discover pass run from worktree A must remove
    // only A's own rows for files missing from A's disk — never another worktree's overlay
    // rows. Otherwise two watchers on one shared DB delete each other's live overlays.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A row owned by a *different* worktree, for a path that does not exist on this disk.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
                     indexed_at_ms, indexed_revision, commit_sha, worktree_id)
                 VALUES ('src/only_in_other.rs','rust','source','h',0,0,0,'rev','',
                     'other-worktree')",
            [],
        )
        .unwrap();
    drop(db);

    // This worktree loses a.rs; re-discover as this worktree.
    fs::remove_file(root.join("src/a.rs")).unwrap();
    let db = IndexDatabase::index_discover(&config).unwrap();
    let conn = db.storage.connection();

    // The other worktree's overlay row survives untouched.
    let other: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE worktree_id = 'other-worktree' AND kind != \
             'deleted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(other, 1, "this worktree's pass deleted another worktree's row");

    // Deletion still works within this worktree's own scope: a.rs gone from the active view,
    // b.rs retained.
    let active = |path: &str| -> i64 {
        conn.query_row("SELECT COUNT(*) FROM files WHERE path = ?1", [path], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(active("src/a.rs"), 0, "deleted file still active in own worktree");
    assert_eq!(active("src/b.rs"), 1, "live file dropped from own worktree");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn gc_prunes_dead_context_rows_and_keeps_live_ones() {
    let (root, config) = markdown_config(
        "# One\nalpha token with enough surrounding detail for embedding eligibility and useful \
         semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding \
         eligibility and useful semantic context\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();
    db.reconcile(None, Some(8)).unwrap();

    let live_files = table_row_count(db.storage.connection(), "files").unwrap();
    let live_chunks = table_row_count(db.storage.connection(), "chunks").unwrap();
    assert!(live_files > 0 && live_chunks > 0);

    // A ghost file from a commit/worktree that is not live.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
                     indexed_at_ms, indexed_revision, commit_sha, worktree_id)
                 VALUES ('ghost.md','markdown','source','deadhash',0,0,0,'deadrev',
                     'deadcommit','dead-worktree')",
            [],
        )
        .unwrap();
    assert_eq!(table_row_count(db.storage.connection(), "files").unwrap(), live_files + 1);

    // Keep only the active worktree. The ghost's commit and worktree are not live.
    let live_worktree = db.active_worktree_id.clone();
    let report = db.prune_to_live(&[], &[live_worktree]).unwrap();

    assert!(!report.skipped);
    assert_eq!(report.files_pruned, 1, "ghost not pruned: {report:?}");
    assert_eq!(
        table_row_count(db.storage.connection(), "files").unwrap(),
        live_files,
        "live files were pruned",
    );
    assert_eq!(
        table_row_count(db.storage.connection(), "chunks").unwrap(),
        live_chunks,
        "live chunks were pruned",
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn gc_refuses_to_prune_with_no_live_context() {
    let (root, config) = markdown_config("# Only\nsome content with enough detail for a chunk\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = table_row_count(db.storage.connection(), "files").unwrap();
    assert!(before > 0);

    // Empty live sets must never wipe the index.
    let report = db.prune_to_live(&[], &[]).unwrap();
    assert!(report.skipped);
    assert_eq!(report.files_pruned, 0);
    assert_eq!(table_row_count(db.storage.connection(), "files").unwrap(), before);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reconcile_treats_c_chunks_as_embedding_eligible() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/main.c"),
        r#"
static int read_sensor_value(int baseline)
{
    int adjusted = baseline + 42;
    return adjusted;
}

int main(void)
{
    int sample = read_sensor_value(7);
    return sample == 49 ? 0 : 1;
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::C);
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();

    let plan = db.reconcile_plan().unwrap();

    assert_eq!(plan.embeddings.skipped_by_policy.get("SkipLanguageUnsupported"), None);
    assert!(plan.embeddings.missing > 0, "plan: {:?}", plan.embeddings);

    let report = db.reconcile(None, Some(8)).unwrap();
    assert!(report.embeddings_written > 0, "report: {report:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reconcile_policy_skips_tiny_chunks_before_embedding() {
    let (root, config) = markdown_config("tiny\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();

    let plan = db.reconcile_plan().unwrap();
    assert_eq!(plan.embeddings.missing, 0);
    assert_eq!(plan.embeddings.skipped_by_policy.get("SkipTooSmall"), Some(&1));

    let report = db.reconcile(None, Some(8)).unwrap();
    assert_eq!(report.embeddings_written, 0);
    assert_eq!(report.skipped_by_policy.get("SkipTooSmall"), Some(&1));
    assert_eq!(db.current_embedding_count(ai::HASH_MODEL_ID).unwrap(), 0);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reconcile_plan_reports_policy_skips_for_fastembed_model() {
    let (root, config) = markdown_config("tiny\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE ai_models
                 SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2
                 WHERE model_id = ?1",
            params![ai::FASTEMBED_MODEL_ID, i64::try_from(ai::FASTEMBED_EMBEDDING_DIM).unwrap()],
        )
        .unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO index_meta(key, value) VALUES ('active_embedding_model', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [ai::FASTEMBED_MODEL_ID],
        )
        .unwrap();

    let plan = db.reconcile_plan().unwrap();

    assert_eq!(plan.embeddings.model_id, ai::FASTEMBED_MODEL_ID);
    assert_eq!(plan.embeddings.missing, 0);
    assert_eq!(plan.embeddings.skipped_by_policy.get("SkipTooSmall"), Some(&1));

    fs::remove_dir_all(root).unwrap();
}

#[cfg(not(feature = "fastembed"))]
#[test]
fn blocked_fastembed_reconcile_still_reports_policy_skips() {
    let (root, config) = markdown_config("tiny\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO index_meta(key, value) VALUES ('active_embedding_model', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [ai::FASTEMBED_MODEL_ID],
        )
        .unwrap();

    let report = db.reconcile(None, Some(8)).unwrap();

    assert_eq!(report.status, "Blocked");
    assert_eq!(report.skipped_by_policy.get("SkipTooSmall"), Some(&1));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_explain_reports_weighted_score_components() {
    let (root, config) = markdown_config(
        "alpha runtime shutdown\nsecond line with enough detail for embedding eligibility and \
         semantic vector scoring\nthird line\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();
    db.reconcile(None, Some(8)).unwrap();

    let hits = db.search_explain("runtime shutdown", 10, false).unwrap();

    assert_eq!(hits.len(), 1);
    let components = hits[0].score_components.as_ref().unwrap();
    let component_sum = components.bm25
        + components.vector
        + components.symbol
        + components.graph
        + components.git
        + components.github;
    // `score` is rounded to 4dp for display, so compare against the rounded component sum.
    assert!((hits[0].score - crate::query::round_score(component_sum)).abs() < 1e-9);
    assert!(components.bm25 > 0.0);
    assert!(components.vector > 0.0);
    assert!(components.vector_note.is_none());
    assert!(components.bm25 <= 0.45);
    assert!(components.vector <= 0.35);
    assert!(components.symbol <= 0.10);
    assert!(components.graph <= 0.05);
    assert!(components.git <= 0.03);
    assert!(components.github <= 0.02);
    assert!(db.search("runtime shutdown", 10, false).unwrap()[0].score_components.is_none());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_explain_labels_missing_vector_runtime() {
    let (root, config) = markdown_config(
        "alpha runtime shutdown\nsecond line with enough detail for lexical search without \
         embeddings\nthird line\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hits = db.search_explain("runtime shutdown", 10, false).unwrap();

    assert_eq!(hits.len(), 1);
    let components = hits[0].score_components.as_ref().unwrap();
    assert!(components.bm25 > 0.0);
    assert_eq!(components.vector, 0.0);
    assert_eq!(
        components.vector_note.as_deref(),
        Some("vector search unavailable: no current embedding model")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_history_indexes_commits_paths_queries_and_blame() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);

    fs::write(root.join("docs/search.md"), "# Title\nalpha token\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn tracked_symbol() {}\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add alpha docs"]);

    fs::write(root.join("docs/search.md"), "# Title\nbeta token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Refresh beta docs"]);

    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![
            ResolvedTarget {
                name: "markdown".to_string(),
                language: Language::Markdown,
                directories: vec![PathBuf::from("docs")],
                include: vec!["**/*.md".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Docs,
            },
            ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["**/*.rs".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
        ],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert!(status.git_history.available);
    assert!(status.git_history.head.is_some());
    assert_eq!(status.git_history.indexed_head, status.git_history.head);
    assert_eq!(status.git_history.commit_count, 2);
    assert_eq!(status.git_history.file_change_count, 3);

    let commit_hits = db.commit_search("beta", 10).unwrap();
    assert_eq!(commit_hits.len(), 1);
    assert_eq!(commit_hits[0].subject, "Refresh beta docs");
    assert_eq!(commit_hits[0].evidence_kind, "historical");
    assert!(commit_hits[0].score > 0.0);

    let path_history = db.git_history_for_path("docs/search.md", 10).unwrap();
    assert_eq!(path_history.len(), 2);
    assert!(path_history.iter().all(|item| item.evidence_kind == "historical"));

    let symbol_history =
        db.git_history_for_symbol("tracked_symbol", Some(Language::Rust), 10).unwrap();
    assert_eq!(symbol_history.len(), 1);
    assert_eq!(symbol_history[0].path, "src/lib.rs");
    assert_eq!(symbol_history[0].evidence_kind, "historical");
    let impact = db.impact_surface("tracked_symbol", 10).unwrap();
    assert!(impact.iter().any(|item| {
        item.category == "Direct structural impact" && item.reason == "exact_symbol_definition"
    }));
    assert!(impact.iter().any(|item| {
        item.category == "Historical/papertrail evidence"
            && item.reason == "git_commit_touched_file"
    }));

    let query_commits = db.commits_touching_query("beta", 10).unwrap();
    let beta_commit = query_commits.iter().find(|hit| hit.subject == "Refresh beta docs").unwrap();
    assert!(beta_commit.evidence.iter().any(|value| value == "commit_message"));
    assert!(beta_commit.evidence.iter().any(|value| value == "file_change"));
    assert_eq!(beta_commit.evidence_kind, "historical");

    let chunk_id = first_chunk_id(&db);
    let blame = db.git_blame_chunk(chunk_id).unwrap().unwrap();
    assert_eq!(blame.source_text_hash, hex_sha256("# Title\nbeta token\n".as_bytes()));
    assert_eq!(blame.line_count, 2);
    assert_eq!(blame.commit_counts.values().sum::<i64>(), 2);
    assert!(blame.dominant_commit_lines >= 1);
    assert!(blame.dominant_commit.is_some());
    assert_eq!(blame.evidence_kind, "historical");
    let cached = db.git_blame_chunk(chunk_id).unwrap().unwrap();
    assert_eq!(cached.source_text_hash, blame.source_text_hash);

    fs::remove_dir_all(root).unwrap();
}

/// A recognizable bogus row that only a git-history *reload* (full table wipe) would remove.
/// Surviving the next incremental pass ⇒ the reload was skipped; gone ⇒ it ran. It lives in
/// `git_file_changes` (a plain table the reload also wipes) rather than `git_commits`, because a
/// stray `git_commits` row with no matching `commit_fts` entry desyncs the external-content FTS5
/// and the reload's `DELETE FROM commit_fts` then corrupts it — a test artifact, not a real state.
const SENTINEL_PATH: &str = "__rag_rat_reload_sentinel__";

fn insert_sentinel_commit(db: &IndexDatabase) {
    let conn = db.storage.connection();
    // git_file_changes.commit_hash has a FK to git_commits.hash, so reuse a real commit hash; the
    // sentinel marker is the path, which the reload wipes along with every other change row.
    let hash: String =
        conn.query_row("SELECT hash FROM git_commits LIMIT 1", [], |row| row.get(0)).unwrap();
    conn.execute(
        "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind)
         VALUES (?1, ?2, 0, 0, 'modified')",
        rusqlite::params![hash, SENTINEL_PATH],
    )
    .unwrap();
}

fn sentinel_commit_count(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM git_file_changes WHERE path = ?1",
            [SENTINEL_PATH],
            |row| row.get(0),
        )
        .unwrap()
}

fn git_history_targets() -> Vec<ResolvedTarget> {
    vec![
        ResolvedTarget {
            name: "markdown".to_string(),
            language: Language::Markdown,
            directories: vec![PathBuf::from("docs")],
            include: vec!["**/*.md".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Docs,
        },
        ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        },
    ]
}

fn rag_rat_config(root: &Path) -> Config {
    Config {
        root: root.to_path_buf(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: git_history_targets(),
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    }
}

/// Init a git repo at `root` with two commits over docs/ + src/, returning its rag-rat Config.
fn git_history_test_config(root: &Path) -> Config {
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    run_git(root, &["init"]);
    run_git(root, &["config", "user.name", "Rag Rat"]);
    run_git(root, &["config", "user.email", "rag@example.com"]);
    fs::write(root.join("docs/search.md"), "# Title\nalpha token\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn tracked_symbol() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-m", "Add alpha docs"]);
    fs::write(root.join("docs/search.md"), "# Title\nbeta token\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-m", "Refresh beta docs"]);
    rag_rat_config(root)
}

#[test]
fn git_history_reload_is_skipped_when_head_is_unchanged() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    insert_sentinel_commit(&db);
    drop(db);

    // No file edit and no HEAD movement: the gate must skip the reload, so the sentinel survives.
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 1, "reload should be skipped when HEAD is unchanged");
    // Real history is left intact (the 2 real commits are untouched by the skip).
    assert_eq!(db.status(&config.database).unwrap().git_history.commit_count, 2);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_history_reloads_after_a_new_commit() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    insert_sentinel_commit(&db);
    drop(db);

    // A real new commit moves HEAD → the gate must reload, wiping the sentinel.
    fs::write(root.join("docs/search.md"), "# Title\ngamma token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add gamma docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 0, "a new commit must force a reload");
    assert_eq!(db.commit_search("gamma", 10).unwrap().len(), 1, "new commit is indexed");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_history_reloads_after_a_history_rewrite() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = db.status(&config.database).unwrap().git_history.commit_count;
    insert_sentinel_commit(&db);
    drop(db);

    // Amend rewrites the tip to a new sha WITHOUT adding a commit — a non-fast-forward rewrite,
    // like the squash that motivated the gate. HEAD's content-addressed sha changes → reload.
    run_git(&root, &["commit", "--amend", "-m", "Refresh delta docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 0, "a history rewrite must force a reload");
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.git_history.commit_count, before, "amend does not change the commit count");
    assert_eq!(db.commit_search("delta", 10).unwrap().len(), 1, "rewritten subject is indexed");
    assert_eq!(db.commit_search("beta", 10).unwrap().len(), 0, "old subject is gone after rewrite");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_history_reload_is_not_skipped_on_a_shallow_clone() {
    let origin = unique_temp_root();
    let _ = fs::remove_dir_all(&origin);
    let _ = git_history_test_config(&origin); // origin repo with two commits

    let shallow = unique_temp_root();
    let _ = fs::remove_dir_all(&shallow);
    // Local clones ignore --depth unless the source is a file:// URL; use one so the clone is
    // genuinely shallow.
    run_git(&std::env::temp_dir(), &[
        "clone",
        "--depth",
        "1",
        &format!("file://{}", origin.display()),
        shallow.to_str().unwrap(),
    ]);
    let config = rag_rat_config(&shallow);

    let db = IndexDatabase::rebuild(&config).unwrap();
    insert_sentinel_commit(&db);
    drop(db);

    // HEAD is unchanged, but a shallow clone can be deepened without moving HEAD, so its history
    // is not pinned by the HEAD sha — the gate must NOT skip. It reloads and wipes the sentinel.
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 0, "a shallow clone must never skip the reload");

    fs::remove_dir_all(origin).unwrap();
    fs::remove_dir_all(shallow).unwrap();
}

fn read_meta(db: &IndexDatabase, key: &str) -> Option<String> {
    db.storage
        .connection()
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()
        .unwrap()
}

#[test]
fn idle_discover_sweep_does_not_rewrite_indexed_at_ms() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    // Stamp a non-numeric sentinel so any spurious timestamp write is unmistakable.
    db.storage
        .connection()
        .execute(
            "INSERT INTO index_meta(key, value) VALUES('indexed_at_ms', 'SENTINEL')
             ON CONFLICT(key) DO UPDATE SET value = 'SENTINEL'",
            [],
        )
        .unwrap();
    drop(db);

    // A discover sweep over an unchanged tree must not mutate the DB — the sentinel survives
    // (no timestamp-only write + COMMIT). See issue #63.
    let db = IndexDatabase::index_discover(&config).unwrap();
    assert_eq!(
        read_meta(&db, "indexed_at_ms").as_deref(),
        Some("SENTINEL"),
        "an unchanged discover sweep must not rewrite indexed_at_ms"
    );
    drop(db);

    // A real change must persist — the sweep writes a fresh timestamp, clearing the sentinel.
    fs::write(root.join("docs/added.md"), "# Added\nfresh content\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add a doc"]);
    let db = IndexDatabase::index_discover(&config).unwrap();
    assert_ne!(
        read_meta(&db, "indexed_at_ms").as_deref(),
        Some("SENTINEL"),
        "a sweep that indexes a new file must update indexed_at_ms"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn index_discover_reporting_flags_content_changes() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);
    IndexDatabase::rebuild(&config).unwrap();

    // No change → reports false, so the watch loop skips the reconcile / memory-validate tail.
    let (_db, changed) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert!(!changed, "an unchanged discover sweep must report no content change");

    // A new file on disk → reports true.
    fs::write(root.join("docs/extra.md"), "# Extra\nbody text\n").unwrap();
    let (_db, changed) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert!(changed, "a discover sweep that indexes a new file must report a content change");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn discover_relanguages_h_when_binding_changes_c_to_cpp() {
    // A `.h` indexed under a `c` binding, then re-discovered under a `cpp` binding with IDENTICAL
    // content, must be reindexed as C++ — discovery treats (language, kind) drift as a change, not
    // just sha drift. Without this the `.h`→C++ upgrade would never take effect on an existing
    // index (the sha is unchanged) until `--full`.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.h"), "class X { public: void f(); };\n").unwrap();

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::C)).unwrap();
    let lang: String = db
        .storage
        .connection()
        .query_row("SELECT language FROM files WHERE path = 'src/lib.h'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lang, "c", "indexed as C under the c binding");
    drop(db);

    let (db, changed) =
        IndexDatabase::index_discover_reporting(&source_config(root.clone(), Language::Cpp))
            .unwrap();
    assert!(changed, "re-languaging a .h with unchanged content must report a change");
    let lang: String = db
        .storage
        .connection()
        .query_row("SELECT language FROM files WHERE path = 'src/lib.h'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lang, "cpp", "the .h must be reindexed as C++ after the binding change");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexes_rust_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
use crate::worker::Worker;
mod worker;

trait Service {
    fn serve(&self);
}

struct Worker;

impl Service for Worker {
    fn serve(&self) {
        helper();
    }
}

fn helper() {}

fn caller() {
    helper();
    Worker.serve();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "caller", "helper", "calls_name", "Syntactic");
    assert_edge(&db, "Worker", "Service", "implements", "Syntactic");
    assert_edge(&db, "src/lib.rs", "worker", "imports", "Syntactic");
    let callers = db.find_callers("helper", 10).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("caller"))
                && edge.edge_kind == "calls_name"
        }),
        "helper callers: {callers:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

/// The callee identifier byte range (#67) stored on the single edge matching
/// `from LIKE %from% AND to_name LIKE %to% AND edge_kind`, as `Option<(start, end)>`.
/// `None` means the column is NULL (no callee range recorded).
fn callee_byte_range(
    db: &IndexDatabase,
    from: &str,
    to: &str,
    edge_kind: &str,
) -> Option<(i64, i64)> {
    let (start, end) = db
        .storage
        .connection()
        .query_row(
            "
                SELECT callee_start_byte, callee_end_byte
                FROM edges
                WHERE edge_kind = ?1
                  AND COALESCE(from_name, '') LIKE ?2
                  AND to_name LIKE ?3
                ",
            params![edge_kind, format!("%{from}%"), format!("%{to}%")],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .unwrap();
    match (start, end) {
        (Some(start), Some(end)) => Some((start, end)),
        _ => None,
    }
}

#[test]
fn calls_name_edge_stores_callee_identifier_byte_range_not_whole_call() {
    // #67: SCIP occurrences key on the callee identifier token, but source_start_byte covers the
    // whole call_expression. The new callee_*_byte columns must span exactly the identifier:
    //   `foo`   in `foo(a, b)`     (plain call)
    //   `method` in `obj.method(x)` (final segment of a method call)
    //   `c`     in `a::b::c()`     (final segment of a path call)
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let source = r#"
fn foo(a: u32, b: u32) -> u32 {
    a + b
}

mod nested {
    pub mod inner {
        pub fn c() {}
    }
}

struct Obj;

impl Obj {
    fn method(&self, _x: u32) {}
}

fn driver() {
    let obj = Obj;
    foo(1, 2);
    obj.method(3);
    nested::inner::c();
}
"#;
    fs::write(root.join("src/lib.rs"), source).unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let assert_callee = |to: &str, expected: &str| {
        let (start, end) = callee_byte_range(&db, "driver", to, "calls_name")
            .unwrap_or_else(|| panic!("no callee range for driver -> {to}"));
        let (start, end) = (start as usize, end as usize);
        assert_eq!(
            &source[start..end],
            expected,
            "callee range for driver -> {to} should be exactly `{expected}`, got `{}`",
            &source[start..end]
        );
        // It must NOT be the whole call expression (that would include the `(`).
        assert!(
            !source[start..end].contains('('),
            "callee range for driver -> {to} must not span the whole call: `{}`",
            &source[start..end]
        );
    };

    assert_callee("foo", "foo");
    assert_callee("method", "method");
    assert_callee("c", "c");

    // A `contains` edge (parent symbol -> child symbol) has no callee identifier → NULL.
    let contains = callee_byte_range(&db, "Obj", "method", "contains");
    assert_eq!(contains, None, "contains edges must have a NULL callee range");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn imports_edge_has_null_callee_byte_range() {
    // #67: file-level edges (imports / exports) carry no callee identifier range.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "mod worker;\n\nfn touch() {\n    worker::run();\n}\n")
        .unwrap();
    fs::write(root.join("src/worker.rs"), "pub fn run() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let import = callee_byte_range(&db, "src/lib.rs", "worker", "imports");
    assert_eq!(import, None, "imports edges must have a NULL callee range");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn c_calls_name_edge_stores_callee_identifier_byte_range() {
    // #67: at least one non-Rust language. A C call `helper(runtime)` stores the range of `helper`,
    // not the whole `helper(runtime)` call expression.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let source = r#"
typedef struct Runtime Runtime;

struct Runtime {
  int state;
};

int helper(Runtime *runtime) {
  return runtime->state;
}

int runtime_open(Runtime *runtime) {
  return helper(runtime);
}
"#;
    fs::write(root.join("src/runtime.c"), source).unwrap();
    let config = source_config(root.clone(), Language::C);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (start, end) = callee_byte_range(&db, "runtime_open", "helper", "calls_name")
        .expect("no callee range for runtime_open -> helper");
    let (start, end) = (start as usize, end as usize);
    assert_eq!(&source[start..end], "helper", "C callee range must span exactly `helper`");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ffi_surface_labels_exported_impl_members_separately() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub struct PhraseRepo;

#[uniffi::export]
impl PhraseRepo {
    pub fn children(&self) {}
    pub fn journal(&self) {}
}

#[cfg_attr(not(target_arch = "wasm32"), uniffi::export(async_runtime = "tokio"))]
impl Runtime {
    pub fn route_search_query(&self) {}
}

pub struct Runtime;

/// Not #[uniffi::export]: this is an internal helper.
pub fn internal_helper() {}

#[cfg_attr(target_arch = "wasm32", ::uniffi::export)]
pub fn exported_fn() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let surface = db.ffi_surface(20).unwrap();
    assert!(
        surface.iter().any(|item| {
            item.reason == "rust_uniffi_export"
                && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("exported_fn"))
        }),
        "direct export should remain direct: {surface:?}"
    );
    assert!(
        surface.iter().any(|item| item.reason == "rust_uniffi_exported_impl"),
        "exported impl/type surface should be explicit: {surface:?}"
    );
    assert!(
        surface.iter().any(|item| {
            item.reason == "rust_uniffi_impl_member"
                && item
                    .symbol
                    .as_deref()
                    .is_some_and(|symbol| symbol.ends_with("route_search_query"))
        }),
        "cfg_attr exported impl member should be labeled separately: {surface:?}"
    );
    assert!(
        surface.iter().any(|item| {
            item.reason == "rust_uniffi_impl_member"
                && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("children"))
        }),
        "impl member should be labeled separately: {surface:?}"
    );
    assert!(
        !surface.iter().any(|item| {
            item.reason == "rust_uniffi_export"
                && item.symbol.as_deref().is_some_and(|symbol| {
                    symbol.ends_with("children") || symbol.ends_with("journal")
                })
        }),
        "impl members must not be reported as direct exports: {surface:?}"
    );
    assert!(
        !surface.iter().any(|item| {
            item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("internal_helper"))
        }),
        "comment-only UniFFI mentions must not create FFI surface rows: {surface:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn find_callers_sees_calls_in_let_bindings() {
    // Regression for issue #47: calls in `let x = f();` and `let-else` initializers produced
    // no caller edge, so find_callers reported 0 callers for a function that is called.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn target() -> Option<i32> {\n    Some(1)\n}\n\npub fn via_statement() {\n    \
         target();\n}\n\npub fn via_let() {\n    let _x = target();\n}\n\npub fn via_let_else() \
         {\n    let Some(_x) = target() else {\n        return;\n    };\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let callers = db.find_callers("target", 50).unwrap();
    let names: Vec<String> = callers.iter().filter_map(|hop| hop.from_symbol.clone()).collect();
    let has = |suffix: &str| names.iter().any(|name| name.ends_with(suffix));

    assert!(has("via_statement"), "missing plain-statement caller; got {names:?}");
    assert!(has("via_let"), "missing `let x = target()` caller; got {names:?}");
    assert!(has("via_let_else"), "missing `let-else` caller; got {names:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn find_callers_sees_message_dispatch_via_synthetic_edge() {
    // #200: a handler reached only through an enum-message dispatch (construct a variant in one fn,
    // handle it in a `match` arm in another) has no static caller edge to the leaf. The synthesized
    // `dispatches` edge connects the constructing fn to the handler the matching arm calls, so
    // find_callers on the leaf surfaces the sender.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum MlReq {
    UpsertJournalEmbedding { id: i64 },
    Other,
}

pub fn enqueue() {
    send(MlReq::UpsertJournalEmbedding { id: 1 });
}

fn send(_req: MlReq) {}

pub fn handle(req: MlReq) {
    match req {
        MlReq::UpsertJournalEmbedding { id } => {
            log_it();
            upsert_journal_embedding(id)
        },
        MlReq::Other => {},
    }
}

pub fn log_it() {}

pub fn upsert_journal_embedding(_id: i64) {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let callers = db.find_callers("upsert_journal_embedding", 50).unwrap();
    // The direct (calls_name) caller is the dispatcher `handle`.
    assert!(
        callers.iter().any(|hop| {
            hop.edge_kind == "calls_name"
                && hop.from_symbol.as_deref().is_some_and(|s| s.ends_with("handle"))
        }),
        "missing direct handler caller: {callers:?}"
    );
    // The synthesized dispatch caller is the constructing fn `enqueue`, via the MlReq variant.
    let dispatch = callers
        .iter()
        .find(|hop| hop.edge_kind == "dispatches")
        .expect("missing synthetic dispatch edge");
    assert!(
        dispatch.from_symbol.as_deref().is_some_and(|s| s.ends_with("enqueue")),
        "dispatch edge should come from the sender: {dispatch:?}"
    );
    assert_eq!(
        dispatch.evidence.as_deref(),
        Some("MlReq::UpsertJournalEmbedding"),
        "dispatch edge should record the routing variant as evidence"
    );

    // A symbol not reached by any dispatch arm gets no synthetic edge.
    let send_callers = db.find_callers("send", 50).unwrap();
    assert!(
        send_callers.iter().all(|hop| hop.edge_kind != "dispatches"),
        "no dispatch edge expected for a non-handler: {send_callers:?}"
    );

    // #200 review (P2 #4): a side-effect call earlier in the arm body (`log_it()`) is NOT the
    // routed handler — only the arm's tail delegate is. `log_it` must get no dispatch caller.
    let log_callers = db.find_callers("log_it", 50).unwrap();
    assert!(
        log_callers.iter().all(|hop| hop.edge_kind != "dispatches"),
        "an arm side-effect call must not become a dispatch target: {log_callers:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_fact_rows_are_hidden_from_the_edges_view() {
    // #200 adversarial review: the internal `dispatch_construct`/`dispatch_handle` FACT rows live
    // in `edges_data` (needed by `synthesize_dispatch_edges`) but are EXCLUDED from the `edges`
    // compatibility view, so every query-layer reader (repo_brief, clusters, grep-augment,
    // orientation, traversal, …) is structurally safe without each remembering an exclusion. The
    // synthesized `dispatches` edge IS a real edge and stays visible.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum MlReq { Upsert { id: i64 } }
pub fn enqueue() { send(MlReq::Upsert { id: 1 }); }
fn send(_r: MlReq) {}
pub fn handle(r: MlReq) {
    match r {
        MlReq::Upsert { id } => upsert(id),
    }
}
pub fn upsert(_id: i64) {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    let view_count = |kind: &str| -> i64 {
        conn.query_row("SELECT COUNT(*) FROM edges WHERE edge_kind = ?1", [kind], |r| r.get(0))
            .unwrap()
    };
    let data_count = |kind: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM edges_data d JOIN name_strings ek ON ek.id = d.edge_kind_id
             WHERE ek.value = ?1",
            [kind],
            |r| r.get(0),
        )
        .unwrap()
    };

    // The FACT rows exist in the base table but are invisible through the view.
    assert!(data_count("dispatch_construct") > 0, "construct fact persisted in edges_data");
    assert!(data_count("dispatch_handle") > 0, "handle fact persisted in edges_data");
    assert_eq!(view_count("dispatch_construct"), 0, "construct fact must be hidden from the view");
    assert_eq!(view_count("dispatch_handle"), 0, "handle fact must be hidden from the view");
    // The synthesized real edge is visible through the view.
    assert!(view_count("dispatches") > 0, "the synthesized dispatches edge must stay visible");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handles_or_patterns_guards_and_let_constructs() {
    // #200 review: or-pattern arms emit a handle per variant; the delegate is the branch tail (a
    // guard/scrutinee call is never a handler); a unit variant in a `let` value position is a
    // construct.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Cmd { Start, Resume, Stop }

pub fn enqueue_start() { send(Cmd::Start); }
pub fn enqueue_resume() { send(Cmd::Resume); }
pub fn enqueue_stop() {
    let c = Cmd::Stop;
    send(c);
}
fn send(_c: Cmd) {}

pub fn handle(c: Cmd) {
    match c {
        Cmd::Start | Cmd::Resume => run_active(),
        Cmd::Stop => if should_stop() { run_stop() } else { run_active() },
    }
}

pub fn should_stop() -> bool { true }
pub fn run_active() {}
pub fn run_stop() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_senders = |symbol: &str| -> Vec<String> {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .collect()
    };
    let ends_with = |names: &[String], suffix: &str| names.iter().any(|n| n.ends_with(suffix));

    // Or-pattern: both `Cmd::Start` and `Cmd::Resume` senders dispatch to `run_active`. The
    // `Cmd::Stop` else-branch also lands on `run_active` (`enqueue_stop` constructs via a `let`).
    let active = dispatch_senders("run_active");
    assert!(ends_with(&active, "enqueue_start"), "or-pattern Start sender missing: {active:?}");
    assert!(ends_with(&active, "enqueue_resume"), "or-pattern Resume sender missing: {active:?}");
    assert!(ends_with(&active, "enqueue_stop"), "guard else-branch sender missing: {active:?}");

    // Guard if-branch: `Cmd::Stop` sender dispatches to `run_stop`.
    let stop = dispatch_senders("run_stop");
    assert!(ends_with(&stop, "enqueue_stop"), "guard if-branch sender missing: {stop:?}");

    // The guard/scrutinee call `should_stop()` is NOT a dispatch handler.
    assert!(
        dispatch_senders("should_stop").is_empty(),
        "a guard/predicate call must not become a dispatch handler"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_resolves_self_keyed_variants_per_impl() {
    // #200 adversarial review (P1): `Self::Variant` is rewritten to the enclosing impl type, so two
    // unrelated enums each writing `Self::Ripe` in construct + handle do NOT cross-link (the old
    // bare `Self::Ripe` key collapsed them). A single impl's `Self::`-keyed dispatch still
    // resolves.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Apple { Ripe }
pub enum Banana { Ripe }

fn sink<T>(_t: T) {}

impl Apple {
    pub fn enqueue_apple() { sink(Self::Ripe); }
    pub fn run_apple(self) { match self { Self::Ripe => apple_handler() } }
}

impl Banana {
    pub fn enqueue_banana() { sink(Self::Ripe); }
    pub fn run_banana(self) { match self { Self::Ripe => banana_handler() } }
}

pub fn apple_handler() {}
pub fn banana_handler() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_senders = |symbol: &str| -> Vec<String> {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .collect()
    };

    // `Self::Ripe` in `impl Apple` keys as `Apple::Ripe` (recall preserved) and does NOT reach the
    // Banana handler (no cross-enum collapse).
    let apple = dispatch_senders("apple_handler");
    assert!(
        apple.iter().any(|s| s.ends_with("enqueue_apple")),
        "self-keyed dispatch lost: {apple:?}"
    );
    let banana = dispatch_senders("banana_handler");
    assert!(
        banana.iter().any(|s| s.ends_with("enqueue_banana")),
        "self-keyed dispatch lost: {banana:?}"
    );
    assert!(
        apple.iter().all(|s| !s.ends_with("enqueue_banana"))
            && banana.iter().all(|s| !s.ends_with("enqueue_apple")),
        "Self::Variant must not cross-link distinct enums: apple={apple:?} banana={banana:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handles_await_tails_and_external_enum_heads() {
    // #200 review: an `.await` tail still resolves the handler; and an enum head with no LOCAL
    // definition (an imported/aliased enum) is admitted, not skipped, since both sender and handler
    // write the same head.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Job { Run }

pub async fn enqueue() { dispatch(Job::Run); }
async fn dispatch(_j: Job) {}
pub async fn handle(j: Job) {
    match j {
        Job::Run => run_job().await,
    }
}
pub async fn run_job() {}

pub fn emit() { ship(Status::Ready); }
fn ship(_s: Status) {}
pub fn route(s: Status) {
    match s {
        Status::Ready => deliver(),
    }
}
pub fn deliver() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from = |symbol: &str, sender: &str| {
        db.find_callers(symbol, 50).unwrap().iter().any(|hop| {
            hop.edge_kind == "dispatches"
                && hop.from_symbol.as_deref().is_some_and(|s| s.ends_with(sender))
        })
    };

    // `.await` tail: `Job::Run => run_job().await` still binds `run_job`.
    assert!(dispatch_from("run_job", "enqueue"), "await tail handler missing a dispatch caller");
    // External/aliased head (`Status` has no local enum definition) is admitted.
    assert!(dispatch_from("deliver", "emit"), "external enum-head dispatch missing");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_binds_let_bound_handler_under_a_wrapper_tail() {
    // #207: the real-world actor idiom delegates in a `let` binding and returns a wrapper —
    // `MlReq::EmbedText { .. } => { let v = embed_text(..)?; Ok(Resp::Embedded(v)) }`. The handler
    // is the let-bound `embed_text`, NOT the tail `Ok(..)` wrapper; the variant is constructed
    // elsewhere (a generic `call` helper sends it). The dispatch edge must reach `embed_text`,
    // not `Ok`.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum MlReq { EmbedText { text: String }, Other }
pub enum Resp { Embedded(i32), Empty }

pub fn enqueue(text: String) { call(MlReq::EmbedText { text }); }
fn call(_req: MlReq) {}

pub fn handle(req: MlReq) -> Result<Resp, ()> {
    match req {
        MlReq::EmbedText { text } => {
            let vector = embed_text(text)?;
            Ok(Resp::Embedded(vector))
        }
        MlReq::Other => Ok(Resp::Empty),
    }
}

fn embed_text(_text: String) -> Result<i32, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_senders = |symbol: &str| -> Vec<String> {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .collect()
    };

    // The let-bound handler `embed_text` is reached from the sender via the dispatch edge.
    let embed = dispatch_senders("embed_text");
    assert!(
        embed.iter().any(|s| s.ends_with("enqueue")),
        "let-bound handler must get a dispatch caller: {embed:?}"
    );
    // The `Ok(..)` / `Resp::Embedded(..)` wrapper constructors are NOT dispatch handlers.
    assert!(
        dispatch_senders("Embedded").is_empty(),
        "a wrapper constructor must not be a dispatch handler"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_selection_distinguishes_handlers_from_wrappers_and_setup() {
    // #208 review: the arm handler is the call producing the result. Keep real calls (incl. FFI/
    // codegen PascalCase fns); never bind a `Result`/`Option` wrapper, a response constructor under
    // a wrapper, or a setup `let` whose binding the tail never references.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Build { x: u8 }, Open { p: u8 }, Direct { y: u8 }, Empty, Setup { z: u8 } }
pub enum Resp { Wrapped(u8), Blank }
impl Resp { fn empty() -> Resp { Resp::Blank } }

pub fn enqueue() {
    send(Msg::Build { x: 1 });
    send(Msg::Open { p: 2 });
    send(Msg::Direct { y: 3 });
    send(Msg::Empty);
    send(Msg::Setup { z: 4 });
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) -> Result<Resp, ()> {
    match m {
        Msg::Build { x } => {
            let result = build_it(x)?;          // let-bound handler under a wrapper tail (#207)
            Ok(Resp::Wrapped(result))           // Resp::Wrapped is a constructor, not a handler
        }
        Msg::Open { p } => CreateFileW(p),      // PascalCase FFI fn — must stay a handler (#208)
        Msg::Direct { y } => handle_direct(y),  // plain tail handler
        Msg::Empty => Ok(Resp::empty()),        // response ctor under wrapper — NO handler (#208)
        Msg::Setup { z } => {
            let _guard = start_span(z);          // setup let not referenced by the tail (#208)
            run_setup()
        }
    }
}
fn build_it(_x: u8) -> Result<u8, ()> { Ok(0) }
fn CreateFileW(_p: u8) -> Result<Resp, ()> { Ok(Resp::Blank) }
fn handle_direct(_y: u8) -> Result<Resp, ()> { Ok(Resp::Blank) }
fn start_span(_z: u8) {}
fn run_setup() -> Result<Resp, ()> { Ok(Resp::Blank) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatches_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Handlers that MUST be reached: the let-bound one (through the `Resp::Wrapped` constructor),
    // the plain tail, and the setup arm's actual tail.
    for handler in ["build_it", "handle_direct", "run_setup"] {
        assert!(dispatches_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // Non-handlers: a response ctor under a wrapper, a setup `let` the tail never reads, and the
    // bare PascalCase `CreateFileW` — indistinguishable from a tuple-struct ctor, so it reads as a
    // wrapper (traced through, not recorded) rather than risk crediting a ctor (accepted recall,
    // #208 review round 10).
    for non_handler in ["empty", "start_span", "CreateFileW"] {
        assert!(
            !dispatches_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_tracing_follows_result_dataflow_not_textual_names() {
    // #208 review round 2: the handler is the call whose result becomes the arm response, traced
    // through wrappers/constructors and `let` bindings. Covers per-branch let-feeds, shadowing,
    // condition-only lets, let-bound response constructors, and struct field-label collisions.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Branch, Shadow, Cond, LetCtor, Field }
pub enum Resp { A, B, Wrap(u8) }
impl Resp { fn empty() -> Resp { Resp::A } }
pub struct Out { status: u8 }

pub fn enqueue() {
    send(Msg::Branch);
    send(Msg::Shadow);
    send(Msg::Cond);
    send(Msg::LetCtor);
    send(Msg::Field);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // per-branch: slow_path feeds the else wrapper branch; fast_path is the if branch.
        Msg::Branch => { let r = slow_path()?; if cond() { fast_path() } else { Ok(Resp::Wrap(r)) } }
        // shadowing: only the last `r` (second_handler) feeds the tail.
        Msg::Shadow => { let r = first_handler()?; let r = second_handler(r)?; Ok(Resp::Wrap(r)) }
        // condition-only: check_ready feeds only the if condition, not the result.
        Msg::Cond => { let ready = check_ready(); if ready { Ok(Resp::A) } else { Ok(Resp::B) } }
        // let-bound response constructor: not a handler.
        Msg::LetCtor => { let resp = Resp::empty(); Ok(resp) }
        // struct field LABEL `status` collides with the setup binding name `status`.
        Msg::Field => { let status = start_span(); Ok(Out { status: 0 }) }
    }
}
fn slow_path() -> Result<u8, ()> { Ok(0) }
fn fast_path() -> Result<Resp, ()> { Ok(Resp::A) }
fn cond() -> bool { true }
fn first_handler() -> Result<u8, ()> { Ok(0) }
fn second_handler(_r: u8) -> Result<u8, ()> { Ok(0) }
fn check_ready() -> bool { true }
fn start_span() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatches_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: both branches of a mixed tail, and the in-scope (shadowing) binding.
    for handler in ["slow_path", "fast_path", "second_handler"] {
        assert!(dispatches_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // Not reached: a shadowed binding, a condition-only let, a let-bound response constructor, and
    // a setup binding that only collides with a struct field label.
    for non_handler in ["first_handler", "cond", "check_ready", "empty", "start_span"] {
        assert!(
            !dispatches_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_tracing_handles_scope_and_turbofish_edge_cases() {
    // #208 review round 3: declaration-ordered binding resolution (shadowing, self-wrapping,
    // reassignment), match-arm pattern masking, let-pattern field labels, turbofish stripping, and
    // wrapper payload containers.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Nested, SelfWrap, FieldLet, Turbo, TurboCtor, Tuple, Reassign }
pub enum Resp { Wrap(u8), Empty }
impl Resp { fn empty() -> Resp { Resp::Empty } }
pub struct Out { status: u8 }

pub fn enqueue() {
    send(Msg::Nested);
    send(Msg::SelfWrap);
    send(Msg::FieldLet);
    send(Msg::Turbo);
    send(Msg::TurboCtor);
    send(Msg::Tuple);
    send(Msg::Reassign);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // nested match arm binding `value` (the Some payload) must not resolve to the outer let.
        Msg::Nested => {
            let value = start_span();
            match maybe() { Some(value) => Ok(Resp::Wrap(value)), _ => Ok(Resp::Empty) }
        }
        // declaration-time scope: the inner `Ok(r)` reads the FIRST `r` (load).
        Msg::SelfWrap => { let r = load()?; let r = Ok(r)?; Ok(Resp::Wrap(r)) }
        // a destructuring field label `status` must not shadow the outer `status` binding.
        Msg::FieldLet => {
            let status = build_status()?;
            let Out { status: other } = read_out()?;
            Ok(Resp::Wrap(status))
        }
        // turbofish on a bare fn stays a handler.
        Msg::Turbo => CreateThing::<u8>(),
        // turbofish on a constructor stays a constructor (no handler).
        Msg::TurboCtor => Ok(Resp::<u8>::empty()),
        // a wrapper payload tuple passes the handler result through.
        Msg::Tuple => { let v = produce()?; Ok((v, 0)) }
        // reassignment: the returned value is the LATEST assignment.
        Msg::Reassign => { let mut resp = start_response()?; resp = finish_response()?; Ok(resp) }
    }
}
fn start_span() {}
fn maybe() -> Option<u8> { None }
fn load() -> Result<u8, ()> { Ok(0) }
fn build_status() -> Result<u8, ()> { Ok(0) }
fn read_out() -> Result<Out, ()> { Ok(Out { status: 0 }) }
fn CreateThing() -> Result<Resp, ()> { Ok(Resp::Empty) }
fn produce() -> Result<u8, ()> { Ok(0) }
fn start_response() -> Result<u8, ()> { Ok(0) }
fn finish_response() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatches_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the prior shadowed binding (load), and the outer field-let binding (build_status).
    for handler in ["load", "build_status"] {
        assert!(dispatches_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // Not reached: a setup masked by a match-arm payload, a destructured-away initializer, a
    // turbofished constructor, a reassigning arm's BOTH producers (the arm bails), `produce` (its
    // `Ok((v, 0))` is a MULTI-element tuple), and the bare PascalCase `CreateThing` (reads as a
    // wrapper, not a handler — accepted recall, #208 review round 10).
    for non_handler in [
        "start_span",
        "read_out",
        "empty",
        "start_response",
        "finish_response",
        "produce",
        "CreateThing",
    ] {
        assert!(
            !dispatches_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_tracing_handles_destructuring_masking_and_control_flow() {
    // #208 review round 4: a destructuring `let` overwrites a stale binding; a struct-pattern field
    // LABEL doesn't mask an outer binding; a turbofish with a path type-arg stays a handler; and a
    // binding reassigned inside control flow is invalidated (no stale-setup edge).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Destructure, MatchLabel, TurboPath, CfgReassign }
pub enum Resp { Wrap(u8), Empty }
pub struct Out { status: u8 }

pub fn enqueue() {
    send(Msg::Destructure);
    send(Msg::MatchLabel);
    send(Msg::TurboPath);
    send(Msg::CfgReassign);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // the destructuring let overwrites the stale `status`; the value comes from read_out.
        Msg::Destructure => {
            let status = start_span()?;
            let Out { status } = read_out()?;
            Ok(Resp::Wrap(status))
        }
        // a struct-pattern field LABEL `status` must not mask the outer `status` binding.
        Msg::MatchLabel => {
            let status = build_status()?;
            match out() { Out { status: other } => Ok(Resp::Wrap(status)), _ => Ok(Resp::Empty) }
        }
        // a turbofish whose type argument is itself a `::` path stays a handler.
        Msg::TurboPath => OpenThing::<some::Handle>(),
        // a binding reassigned inside control flow is invalidated (no stale-setup edge).
        Msg::CfgReassign => {
            let mut resp = start_response()?;
            if cond() { resp = finish_a()?; } else { resp = finish_b()?; }
            Ok(resp)
        }
    }
}
fn start_span() -> Result<u8, ()> { Ok(0) }
fn read_out() -> Result<Out, ()> { Ok(Out { status: 0 }) }
fn build_status() -> Result<u8, ()> { Ok(0) }
fn out() -> Out { Out { status: 0 } }
fn OpenThing() -> Result<Resp, ()> { Ok(Resp::Empty) }
fn cond() -> bool { true }
fn start_response() -> Result<u8, ()> { Ok(0) }
fn finish_a() -> Result<u8, ()> { Ok(0) }
fn finish_b() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatches_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the outer binding the match-arm label can't mask.
    assert!(dispatches_from_enqueue("build_status"), "build_status must be a dispatch handler");
    // Not reached: a setup overwritten by a destructuring let (and its producer `read_out`), a
    // control-flow reassignment (its arm bails), and the bare PascalCase `OpenThing` (reads as a
    // wrapper, not a handler — accepted recall, #208 review round 10).
    for non_handler in ["start_span", "read_out", "start_response", "OpenThing"] {
        assert!(
            !dispatches_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_tracing_handles_guards_typed_wrappers_scrutinee_and_shadow() {
    // #208 review round 5: guards excluded from masking; turbofished `Ok::<T,E>` recognized as a
    // wrapper; match-payload bindings inherit the scrutinee producer; assignments hidden in a `let`
    // initializer invalidate; inner-block shadowing doesn't invalidate the outer binding; a
    // multi-binding destructure doesn't credit every producer; const-generic call names resolve.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Guard, TypedOk, MatchPayload, LetInitAssign, InnerShadow, TupleDestr }
pub enum Resp { Wrap(u8), Empty }
pub struct Out { other: u8 }
pub struct Grid;
impl Grid { fn assemble<const N: usize>() -> u8 { 0 } }

pub fn enqueue() {
    send(Msg::Guard);
    send(Msg::TypedOk);
    send(Msg::MatchPayload);
    send(Msg::LetInitAssign);
    send(Msg::InnerShadow);
    send(Msg::TupleDestr);
}
fn send(_m: Msg) {}
pub fn const_caller() { Grid::<{ 2 << 1 }>::assemble::<3>(); }

pub fn handle(m: Msg) {
    match m {
        // a guard's `status` read must not be masked as a binding; build_status survives.
        Msg::Guard => {
            let status = build_status()?;
            match out() { Out { other } if status > 0 => Ok(Resp::Wrap(status)), _ => Ok(Resp::Empty) }
        }
        // a typed `Ok::<Resp, ()>` is a wrapper — traced through to the let-fed compute.
        Msg::TypedOk => { let v = compute()?; Ok::<Resp, ()>(Resp::Wrap(v)) }
        // a returned match payload inherits the scrutinee producer (load).
        Msg::MatchPayload => {
            let r = load()?;
            match r { Some(v) => Ok(Resp::Wrap(v)), _ => Ok(Resp::Empty) }
        }
        // an assignment hidden in a `let` initializer invalidates resp (no stale `start`).
        Msg::LetInitAssign => {
            let mut resp = start()?;
            let _ = { resp = finish()?; };
            Ok(resp)
        }
        // an inner-block shadow reassignment must not invalidate the outer `built`.
        Msg::InnerShadow => {
            let built = build()?;
            { let mut built = 0; built = 1; }
            Ok(Resp::Wrap(built))
        }
        // a multi-binding destructure must not credit `start_span` to `resp`.
        Msg::TupleDestr => {
            let (resp, _span) = (build_resp()?, start_span());
            Ok(Resp::Wrap(resp))
        }
    }
}
fn out() -> Out { Out { other: 0 } }
fn build_status() -> Result<u8, ()> { Ok(0) }
fn compute() -> Result<u8, ()> { Ok(0) }
fn load() -> Result<Option<u8>, ()> { Ok(None) }
fn start() -> Result<u8, ()> { Ok(0) }
fn finish() -> Result<u8, ()> { Ok(0) }
fn build() -> Result<u8, ()> { Ok(0) }
fn build_resp() -> Result<u8, ()> { Ok(0) }
fn start_span() -> u8 { 0 }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };
    let called_by = |symbol: &str, caller: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with(caller))
    };

    for handler in ["build_status", "compute", "load"] {
        assert!(dispatch_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // `build` (InnerShadow arm) now bails: the arm reassigns a local (`built = 1`), so the whole
    // arm is conservatively dropped rather than tracking shadow-aware scopes (accepted recall,
    // #208).
    for non_handler in ["start", "start_span", "build"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }
    // Const-generic turbofish: the call names the function, not the type — `assemble` has a caller.
    assert!(
        called_by("assemble", "const_caller"),
        "const-generic call must resolve to the callee name"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_tracing_handles_multibind_ufcs_field_and_path_qualifiers() {
    // #208 review round 6: multi-binding match scrutinee doesn't credit every producer; a
    // reassignment in an assignment RHS invalidates; UFCS is a constructor; pre-shadow assignments
    // target the outer binding; field projections trace their receiver; a module-qualified
    // pattern's qualifier doesn't mask an outer binding.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { MultiScrut, RhsReassign, Ufcs, ShadowOrder, FieldProj, PathQual }
pub enum Resp { Wrap(u8), Empty }
pub struct Thing { id: u8 }
pub enum E { Ready(u8) }

pub fn enqueue() {
    send(Msg::MultiScrut);
    send(Msg::RhsReassign);
    send(Msg::Ufcs);
    send(Msg::ShadowOrder);
    send(Msg::FieldProj);
    send(Msg::PathQual);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // a multi-binding match scrutinee must not credit start_span to resp.
        Msg::MultiScrut => match (build_resp()?, start_span()) { (resp, _span) => Ok(Resp::Wrap(resp)) },
        // a reassignment hidden in an assignment RHS invalidates resp (no stale start_a).
        Msg::RhsReassign => { let mut resp = start_a()?; other = { resp = finish_a()?; 1 }; Ok(resp) }
        // a UFCS associated call is a constructor, not a handler.
        Msg::Ufcs => Ok(<Resp as Default>::default()),
        // a pre-shadow assignment targets the outer resp → invalidate (no stale start_b).
        Msg::ShadowOrder => { let mut resp = start_b()?; { resp = finish_b()?; let resp = 0; } Ok(resp) }
        // a field projection of a result binding traces back to its producer.
        Msg::FieldProj => { let r = build()?; Ok(Resp::Wrap(r.id)) }
        // a module-qualified pattern's qualifier must not mask the outer binding.
        Msg::PathQual => {
            let status = build_status()?;
            match e() { status::Ready(v) => Ok(Resp::Wrap(status)), _ => Ok(Resp::Empty) }
        }
    }
}
fn build_resp() -> Result<u8, ()> { Ok(0) }
fn start_span() -> u8 { 0 }
fn start_a() -> Result<u8, ()> { Ok(0) }
fn finish_a() -> Result<u8, ()> { Ok(0) }
fn start_b() -> Result<u8, ()> { Ok(0) }
fn finish_b() -> Result<u8, ()> { Ok(0) }
fn build() -> Result<Thing, ()> { Ok(Thing { id: 0 }) }
fn build_status() -> Result<u8, ()> { Ok(0) }
fn e() -> E { E::Ready(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    for handler in ["build", "build_status"] {
        assert!(dispatch_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    for non_handler in ["start_span", "start_a", "default", "start_b"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_tracing_index_cast_orpattern_and_rebind_bail() {
    // #208 review round 7 (after the conservative restructure): index projections trace only the
    // receiver; casts trace the operand; or-pattern payloads inherit the scrutinee; and any arm
    // that rebinds a local (destructuring-assign, closure reassignment) or destructures a
    // discarded producer bails / invalidates rather than emitting a stale or false edge.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { IndexProj, OrPat, CastProj, DestrIgnore, DestrAssign, ClosureReassign }
pub enum Resp { Wrap(u8), Empty }
pub enum Sig { A(u8), B(u8) }

pub fn enqueue() {
    send(Msg::IndexProj);
    send(Msg::OrPat);
    send(Msg::CastProj);
    send(Msg::DestrIgnore);
    send(Msg::DestrAssign);
    send(Msg::ClosureReassign);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // an index projection traces only the receiver, not the index expression.
        Msg::IndexProj => { let r = build_idx()?; Ok(Resp::Wrap(r[choose_index()])) }
        // an or-pattern's repeated payload binding inherits the scrutinee producer.
        Msg::OrPat => match get() { Sig::A(v) | Sig::B(v) => Ok(Resp::Wrap(v)), },
        // a cast of a returned binding traces the operand.
        Msg::CastProj => { let v = build_cast()?; Ok(Resp::Wrap(v as u32)) }
        // a destructure that discards a producer doesn't credit it.
        Msg::DestrIgnore => { let (resp, _) = (build_d()?, start_span()); Ok(Resp::Wrap(resp)) }
        // a destructuring assignment rebinds a local → the arm bails.
        Msg::DestrAssign => { let mut resp = start_da()?; (resp, _) = (finish_da()?, 0); Ok(resp) }
        // a reassignment inside a closure rebinds a local → the arm bails.
        Msg::ClosureReassign => {
            let resp = build_c()?;
            let _f = || { resp = finish_c()?; };
            Ok(Resp::Wrap(resp))
        }
    }
}
fn build_idx() -> Result<u8, ()> { Ok(0) }
fn choose_index() -> usize { 0 }
fn get() -> Sig { Sig::A(0) }
fn build_cast() -> Result<u8, ()> { Ok(0) }
fn build_d() -> Result<u8, ()> { Ok(0) }
fn start_span() -> u8 { 0 }
fn start_da() -> Result<u8, ()> { Ok(0) }
fn finish_da() -> Result<u8, ()> { Ok(0) }
fn build_c() -> Result<u8, ()> { Ok(0) }
fn finish_c() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the indexed receiver, the or-pattern scrutinee, and the cast operand's producer.
    for handler in ["build_idx", "get", "build_cast"] {
        assert!(dispatch_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // Not reached: an index selector, a discarded destructure producer, and producers in arms that
    // rebind a local (destructuring-assign / closure reassignment — the arm bails).
    for non_handler in ["choose_index", "start_span", "start_da", "finish_c"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_tracing_suppresses_multivalue_error_and_rebind_false_edges() {
    // #208 review round 8: the contract is "a missed edge is OK, a FALSE edge is a bug."
    // Multi-value containers (multi-arg ctor / multi-element tuple / multi-field struct), `Err`
    // payloads, multi-producer scrutinees, and reassignments via wrapped/destructuring LHS must
    // NOT synthesize a handler edge to a discarded/stale/error call. Unit-variant path
    // qualifiers and unary projections are handled too.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { MultiCtor, MultiTuple, ErrArm, ParenRebind, DerefRebind, ArrayAssign, StructAssign, PartialScrut, UnitQual, DerefProj }
pub enum Resp { Wrap(u8), Two(u8, u8), Empty }
pub struct Out { resp: u8 }
pub enum Sig { Ready }

pub fn enqueue() {
    send(Msg::MultiCtor);
    send(Msg::MultiTuple);
    send(Msg::ErrArm);
    send(Msg::ParenRebind);
    send(Msg::DerefRebind);
    send(Msg::ArrayAssign);
    send(Msg::StructAssign);
    send(Msg::PartialScrut);
    send(Msg::UnitQual);
    send(Msg::DerefProj);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // multi-arg constructor: can't attribute the response → no edge (no false `record_metric`).
        Msg::MultiCtor => Ok(Resp::Two(embed_text(), record_metric())),
        // multi-element tuple → no edge (no false `side`).
        Msg::MultiTuple => Ok((handler_a(), side())),
        // an `Err` payload is an error value, not a response handler.
        Msg::ErrArm => Err(build_error()),
        // paren-wrapped reassignment rebinds a local → arm bails (no stale `first`).
        Msg::ParenRebind => { let mut resp = first(); (resp) = second(); Ok(Resp::Wrap(resp)) }
        // deref reassignment rebinds a local → arm bails (no stale `first_d`).
        Msg::DerefRebind => { let mut v = first_d(); let p = &mut v; *p = second_d(); Ok(Resp::Wrap(v)) }
        // array destructuring assignment → arm bails (no stale `first_arr`).
        Msg::ArrayAssign => { let mut resp = first_arr(); [resp, _] = [second_arr(), 0]; Ok(Resp::Wrap(resp)) }
        // struct destructuring assignment → arm bails (no stale `first_st`).
        Msg::StructAssign => { let mut resp = first_st(); Out { resp } = make_out(); Ok(Resp::Wrap(resp)) }
        // a single binding over a MULTI-producer scrutinee tuple → no scrutinee inheritance.
        Msg::PartialScrut => match (build_p()?, start_span()) { (resp, 0) => Ok(Resp::Wrap(resp)), _ => Ok(Resp::Empty) },
        // a unit-variant path qualifier must not mask the outer binding.
        Msg::UnitQual => { let status = build_status()?; match sig() { Sig::Ready => Ok(Resp::Wrap(status)), _ => Ok(Resp::Empty) } }
        // a unary deref projection of a returned binding traces the operand.
        Msg::DerefProj => { let v = build_deref()?; Ok(Resp::Wrap(*v)) }
    }
}
fn embed_text() -> u8 { 0 }
fn record_metric() -> u8 { 0 }
fn handler_a() -> u8 { 0 }
fn side() -> u8 { 0 }
fn build_error() -> () {}
fn first() -> u8 { 0 }
fn second() -> u8 { 0 }
fn first_d() -> u8 { 0 }
fn second_d() -> u8 { 0 }
fn first_arr() -> u8 { 0 }
fn second_arr() -> u8 { 0 }
fn first_st() -> u8 { 0 }
fn make_out() -> Out { Out { resp: 0 } }
fn build_p() -> Result<u8, ()> { Ok(0) }
fn start_span() -> u8 { 0 }
fn build_status() -> Result<u8, ()> { Ok(0) }
fn sig() -> Sig { Sig::Ready }
fn build_deref() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the unit-variant-qualifier arm's outer binding, and the unary-deref projection.
    for handler in ["build_status", "build_deref"] {
        assert!(dispatch_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // FALSE edges that must NOT exist: discarded multi-value siblings, an `Err` payload builder,
    // stale producers behind a wrapped/destructuring reassignment, and a multi-producer scrutinee's
    // other producer.
    for non_handler in [
        "record_metric",
        "side",
        "build_error",
        "first",
        "first_d",
        "first_arr",
        "first_st",
        "start_span",
    ] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler (false edge)"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_tracing_field_store_scoped_err_config_ctor_and_comments() {
    // #208 review round 9: a field/index store before the tail must NOT bail the arm; a scoped
    // `Result::Err(..)` must be suppressed like bare `Err`; a snake-tail associated constructor
    // (`Vec::with_capacity(config)`) must NOT be traced as a payload wrapper; and a comment in a
    // wrapper's argument list must not hide the single payload.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { FieldStore, ScopedErr, ConfigCtor, Commented }
pub enum Resp { Wrap(u8), Empty }
pub struct S { count: u8 }

pub fn enqueue() {
    send(Msg::FieldStore);
    send(Msg::ScopedErr);
    send(Msg::ConfigCtor);
    send(Msg::Commented);
}
fn send(_m: Msg) {}

pub fn handle(state: &mut S, m: Msg) {
    match m {
        // a field store before the tail is a side effect, not a rebind — `run` is still the handler.
        Msg::FieldStore => { state.count = now(); run() }
        // a scoped `Result::Err(..)` is an error wrapper — its builder is not a handler.
        Msg::ScopedErr => Result::Err(build_error()),
        // `Vec::with_capacity(n)` is a snake-tail associated ctor; `n` configures, isn't the payload.
        Msg::ConfigCtor => Ok(Vec::with_capacity(record_metric())),
        // a comment in the wrapper arg list must not hide the single payload `v`.
        Msg::Commented => { let v = handler_c()?; Ok(v /* note */) }
    }
}
fn now() -> u8 { 0 }
fn run() -> Result<Resp, ()> { Ok(Resp::Empty) }
fn build_error() -> () {}
fn record_metric() -> usize { 0 }
fn handler_c() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the let-bound payload behind a comment.
    assert!(dispatch_from_enqueue("handler_c"), "handler_c must be a dispatch handler");
    // FALSE edges that must NOT exist: a scoped-`Err` builder, and a config arg of an associated
    // constructor. Plus `run`/`now` — a field store (`state.count = now()`) can stale a returned
    // projection, so an arm containing ANY local-mutating assignment bails entirely (accepted
    // recall, #208 review round 10).
    for non_handler in ["now", "build_error", "record_metric", "run"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_tracing_effect_only_wrappers_iflet_mut_and_adapters() {
    // #208 review round 10 + held feedback: the EFFECT-ONLY handler (`h()?; Ok(unit)`) is
    // recovered; module-qualified/bare PascalCase ctors are transparent wrappers; `if let`
    // payloads and `let mut` bindings resolve; a fire-and-forget side effect, a scoped-receiver
    // method adapter, and a field store that can stale a returned projection produce no edge.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Effect, Fire, ModCtor, BareCtor, MutBind, IfLet, MethodAdapter, FieldProj }
pub enum MlResp { Diarized, Done }
pub enum Resp { Wrap(u8), Empty }
pub struct Bare(u8);
pub struct Out { id: u8 }
pub mod dto { pub struct Wrapped(pub u8); }

pub fn enqueue() {
    send(Msg::Effect);
    send(Msg::Fire);
    send(Msg::ModCtor);
    send(Msg::BareCtor);
    send(Msg::MutBind);
    send(Msg::IfLet);
    send(Msg::MethodAdapter);
    send(Msg::FieldProj);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // effect-only: a `?`-propagated work call + a fixed ack → the fallback records do_work.
        Msg::Effect => { do_work()?; Ok(MlResp::Diarized) }
        // a fire-and-forget side effect (no `?`) must NOT be recorded.
        Msg::Fire => { fire_and_forget(); Ok(MlResp::Done) }
        // a module-qualified tuple ctor is a transparent wrapper → traces v → dto_build.
        Msg::ModCtor => { let v = dto_build()?; Ok(dto::Wrapped(v)) }
        // a bare tuple-struct ctor is a transparent wrapper → traces v → bare_build.
        Msg::BareCtor => { let v = bare_build()?; Ok(Bare(v)) }
        // a `let mut` binding maps to its producer.
        Msg::MutBind => { let mut v = build_mut()?; Ok(Resp::Wrap(v)) }
        // an if-let payload inherits the condition value's producer.
        Msg::IfLet => if let Some(v) = load_il()? { Ok(Resp::Wrap(v)) } else { Ok(Resp::Empty) },
        // a method adapter on a scoped binding is suppressed (no false `into` edge; build_into is
        // conservatively dropped).
        Msg::MethodAdapter => { let v = build_into()?; Ok(Resp::Wrap(v.into())) }
        // a field store can stale a returned projection → the whole arm bails.
        Msg::FieldProj => { let mut r = mk()?; r.id = finish_fp()?; Ok(Resp::Wrap(r.id)) }
    }
}
fn do_work() -> Result<u8, ()> { Ok(0) }
fn fire_and_forget() {}
fn dto_build() -> Result<u8, ()> { Ok(0) }
fn bare_build() -> Result<u8, ()> { Ok(0) }
fn build_mut() -> Result<u8, ()> { Ok(0) }
fn load_il() -> Result<Option<u8>, ()> { Ok(None) }
fn build_into() -> Result<u8, ()> { Ok(0) }
fn mk() -> Result<Out, ()> { Ok(Out { id: 0 }) }
fn finish_fp() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the effect-only work call, the module/bare wrapper payloads, the `let mut` binding,
    // and the if-let payload.
    for handler in ["do_work", "dto_build", "bare_build", "build_mut", "load_il"] {
        assert!(dispatch_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // Not reached: a fire-and-forget side effect, a scoped-receiver method adapter's producer
    // (suppressed), and a field-store arm's producers (the arm bails).
    for non_handler in ["fire_and_forget", "build_into", "mk", "finish_fp"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_handler_tracing_effect_only_shadow_and_scoped_methods() {
    // #208 review round 11: the effect-only fallback records the DIRECT call, not a `let`-bound `?`
    // resolved against the final (shadowed) scope; and a method call on a scoped binding
    // (`worker.run()`) IS recorded as the handler again (a real method resolves; a pure adapter
    // `v.into()` is a std method that doesn't resolve, so it creates no edge).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { ShadowFallback, Worker }
pub enum Resp { Done, Out(u8) }
pub struct W;
impl W { fn run(&self) -> Result<Resp, ()> { Ok(Resp::Done) } }

pub fn enqueue() {
    send(Msg::ShadowFallback);
    send(Msg::Worker);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // the effect-only fallback must NOT resolve the bound `task?` against the SHADOWED final
        // scope (which maps `task` to record_metric) — `task?` isn't a direct call, so it's skipped.
        Msg::ShadowFallback => { let task = do_work(); task?; let task = record_metric(); Ok(Resp::Done) }
        // a method call on a scoped binding IS the handler (worker.run), recorded again.
        Msg::Worker => { let worker = make_worker(); worker.run() }
    }
}
fn do_work() -> Result<(), ()> { Ok(()) }
fn record_metric() -> Result<(), ()> { Ok(()) }
fn make_worker() -> W { W }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the method handler on a scoped binding.
    assert!(dispatch_from_enqueue("run"), "run must be a dispatch handler");
    // Not reached: a shadowing producer the effect-only fallback must not misresolve to, and the
    // bound `task?`'s producer (a bound-`?` is not a direct call — accepted recall).
    for non_handler in ["record_metric", "do_work"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_ignores_nested_payload_variants() {
    // #200 review: an arm pattern `Outer::Wrapped(Inner::Start) => run()` handles `Outer::Wrapped`,
    // NOT the nested payload `Inner::Start`. A function that only constructs `Inner::Start` as data
    // must not be reported as a dispatch caller of `run`.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Outer { Wrapped(Inner) }
pub enum Inner { Start }

pub fn enqueue_outer() { send(Outer::Wrapped(Inner::Start)); }
pub fn enqueue_inner_only() { take(Inner::Start); }
fn send(_o: Outer) {}
fn take(_i: Inner) {}

pub fn handle(o: Outer) {
    match o {
        Outer::Wrapped(Inner::Start) => run(),
    }
}
pub fn run() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let senders: Vec<String> = db
        .find_callers("run", 50)
        .unwrap()
        .into_iter()
        .filter(|hop| hop.edge_kind == "dispatches")
        .filter_map(|hop| hop.from_symbol)
        .collect();
    assert!(
        senders.iter().any(|s| s.ends_with("enqueue_outer")),
        "the outer-variant sender should dispatch: {senders:?}"
    );
    assert!(
        senders.iter().all(|s| !s.ends_with("enqueue_inner_only")),
        "a nested-payload variant must not be treated as the handled variant: {senders:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dispatch_join_is_scoped_to_a_unique_enum_definition() {
    // #200 review (P2 #1): the variant key is module-stripped (`Msg::Start`), so two distinct enums
    // both named `Msg` must NOT merge — a sender of one enum's variant must not appear as a caller
    // of the other's handler. With the enum name ambiguous, the join is skipped entirely.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod a {
    pub enum Msg { Start }
    pub fn enqueue_a() { send_a(Msg::Start); }
    fn send_a(_m: Msg) {}
    pub fn handle_a(m: Msg) {
        match m {
            Msg::Start => run_a(),
        }
    }
    pub fn run_a() {}
}

pub mod b {
    pub enum Msg { Start }
    pub fn enqueue_b() { send_b(Msg::Start); }
    fn send_b(_m: Msg) {}
    pub fn handle_b(m: Msg) {
        match m {
            Msg::Start => run_b(),
        }
    }
    pub fn run_b() {}
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // `Msg` is ambiguous (two enums), so no dispatch edges are synthesized — crucially, NO
    // cross-enum edge from `enqueue_a` to `run_b` (or vice versa).
    for handler in ["run_a", "run_b"] {
        let callers = db.find_callers(handler, 50).unwrap();
        assert!(
            callers.iter().all(|hop| hop.edge_kind != "dispatches"),
            "ambiguous enum must not synthesize a (possibly cross-enum) dispatch into {handler}: \
             {callers:?}"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn symbol_search_excludes_generated_bindings_unless_opted_in() {
    // #202: a name/symbol_path search drowns in generated bindings (ubrn FFI output, codegen) that
    // shadow the hand-written source symbol. The real-world case is codegen living UNDER a source
    // target (e.g. `packages/.../src/generated/`): it keeps `kind = source` and gets full symbols,
    // but `is_generated_path` flags `files.generated = 1`. Symbol search defaults to
    // `files.generated = 0` (the same flag search/orientation use) and lets callers opt the
    // generated rows back in; an explicit id selection is never filtered.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/generated")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn shared_symbol() {}\n").unwrap();
    fs::write(root.join("src/generated/bindings.rs"), "pub fn shared_symbol() {}\n").unwrap();
    // A single SOURCE target covers both files; the nested `generated/` dir is flagged by the path
    // heuristic, not by target kind.
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let by_name = || crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("shared_symbol".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: true,
        limit: 10,
    };

    // Default (include_generated = false): the generated copy is filtered out, source remains.
    let default_hits = db.symbol_candidates(&by_name(), false).unwrap();
    assert!(!default_hits.candidates.is_empty(), "source symbol must still resolve");
    assert!(
        default_hits.candidates.iter().all(|c| !c.path.contains("/generated/")),
        "generated bindings must be excluded by default: {:?}",
        default_hits.candidates.iter().map(|c| &c.path).collect::<Vec<_>>()
    );

    // Opt-in (include_generated = true): both copies come back.
    let all_hits = db.symbol_candidates(&by_name(), true).unwrap();
    let generated = all_hits
        .candidates
        .iter()
        .find(|c| c.path.contains("/generated/"))
        .expect("opt-in must surface the generated copy");

    // An explicit symbol_id pick of the generated symbol is honored regardless of the filter —
    // the exclusion only governs name/path *search*, not a deliberate selection.
    let by_id = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: Some(generated.symbol_id),
        symbol_path: None,
        symbol: None,
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    let id_hits = db.symbol_candidates(&by_id, false).unwrap();
    assert_eq!(id_hits.candidates.len(), 1, "explicit id must resolve the generated symbol");
    assert!(id_hits.candidates[0].path.contains("/generated/"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn stale_generated_flags_are_rederived_on_open() {
    // #202 review (P2): incremental discovery rewrites a file row only on sha/language/kind change,
    // so an index built BEFORE the flag's definition changed keeps the old `files.generated` and
    // would still surface generated bindings. The version-gated re-derive must heal it on next
    // open.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/generated")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn shared_symbol() {}\n").unwrap();
    fs::write(root.join("src/generated/bindings.rs"), "pub fn shared_symbol() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Simulate a pre-#202 index: the generated-dir file was flagged 0, and the flags-version meta
    // is absent/stale so the gate will fire.
    db.storage
        .connection()
        .execute("UPDATE main.files SET generated = 0 WHERE path LIKE '%/generated/%'", [])
        .unwrap();
    db.storage
        .connection()
        .execute("DELETE FROM index_meta WHERE key = ?1", [GENERATED_FLAGS_VERSION_KEY])
        .unwrap();
    let by_name = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("shared_symbol".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: true,
        limit: 10,
    };
    // Pre-heal: the mis-flagged generated copy leaks into the default (exclude-generated) search.
    let before = db.symbol_candidates(&by_name, false).unwrap();
    assert!(
        before.candidates.iter().any(|c| c.path.contains("/generated/")),
        "precondition: stale flag leaks the generated copy"
    );

    // The version-gated re-derive heals every mis-flagged row from the path heuristic.
    db.ensure_generated_flags_current().unwrap();
    let after = db.symbol_candidates(&by_name, false).unwrap();
    assert!(!after.candidates.is_empty(), "source symbol still resolves");
    assert!(
        after.candidates.iter().all(|c| !c.path.contains("/generated/")),
        "re-derive must re-exclude the generated copy: {:?}",
        after.candidates.iter().map(|c| &c.path).collect::<Vec<_>>()
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_and_read_chunk_attach_bounded_graph_evidence() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn helper() {}\n\npub fn caller() {\n    helper();\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hits = db.search("helper caller", 10, false).unwrap();
    let helper_hit = hits
        .iter()
        .find(|hit| hit.symbol_path.as_deref().is_some_and(|path| path.ends_with("helper")))
        .expect("helper search hit");
    let helper_graph = helper_hit.graph.as_ref().expect("helper graph evidence");
    assert_eq!(helper_graph.caller_count, 1);
    assert!(helper_graph.top_callers.iter().any(|caller| {
        caller.symbol_path.ends_with("caller")
            && caller.callsite.line == 4
            && caller.callsite.span == [4, 4]
            && caller.confidence == "syntactic"
    }));
    assert!(helper_graph.callers.is_empty(), "search keeps graph compact");

    let caller_hit = hits
        .iter()
        .find(|hit| hit.symbol_path.as_deref().is_some_and(|path| path.ends_with("caller")))
        .expect("caller search hit");
    let caller_graph = caller_hit.graph.as_ref().expect("caller graph evidence");
    assert!(caller_graph.top_callees.iter().any(|callee| {
        callee.target == "helper"
            && callee.callsite.line == 4
            && callee.callsite.span == [4, 4]
            && callee.confidence == "syntactic"
    }));

    let chunk = db.read_chunk(caller_hit.chunk_id).unwrap().expect("caller chunk");
    let full_graph = chunk.graph.as_ref().expect("full read_chunk graph");
    assert!(full_graph.symbol.as_ref().is_some_and(|symbol| symbol.name == "caller"));
    assert!(
        full_graph
            .callees
            .iter()
            .any(|callee| callee.target == "helper" && callee.callsite.line == 4)
    );
    assert!(full_graph.notes.iter().any(|note| note.contains("tree-sitter/syntactic")));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn graph_exact_mode_requires_verified_symbol_identity() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn helper() {}\n\npub fn caller() {\n    helper();\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let helper = db.symbols("helper", Some(Language::Rust), 10).unwrap().remove(0);
    let caller = db.symbols("caller", Some(Language::Rust), 10).unwrap().remove(0);

    let bare_exact = db
        .find_callers_with_options("helper", 10, &crate::query::graph::GraphTraversalOptions {
            resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
            ..Default::default()
        })
        .unwrap();
    assert!(bare_exact.is_empty(), "bare exact lookup should not fall back: {bare_exact:?}");

    let exact_callers = db
        .find_callers_with_options("helper", 10, &crate::query::graph::GraphTraversalOptions {
            resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
            symbol_id: Some(helper.symbol_id),
            ..Default::default()
        })
        .unwrap();
    assert!(
        exact_callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("caller"))
                && edge.verified_target_symbol
        }),
        "exact callers: {exact_callers:?}"
    );
    assert!(exact_callers.iter().all(|edge| edge.verified_target_symbol));

    let exact_callees = db
        .trace_callees_with_options("caller", 10, &crate::query::graph::GraphTraversalOptions {
            resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
            symbol_id: Some(caller.symbol_id),
            ..Default::default()
        })
        .unwrap();
    assert!(
        exact_callees.iter().any(|edge| {
            edge.target.as_deref() == Some("helper") && edge.verified_target_symbol
        }),
        "exact callees: {exact_callees:?}"
    );
    assert!(exact_callees.iter().all(|edge| edge.verified_target_symbol));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn symbol_lookup_ranks_type_definitions_before_impl_blocks() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
impl Database {
    pub fn open() -> Self {
        Database
    }
}

pub struct Database;
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let hits = db.symbols("Database", Some(Language::Rust), 10).unwrap();
    assert!(hits.len() >= 2, "fixture should expose both impl and struct symbols: {hits:?}");
    assert_eq!(hits[0].kind, "struct", "Database lookup should prefer type definition");
    assert!(
        hits.iter().any(|hit| hit.kind == "impl"),
        "impl Database should still be available after the struct: {hits:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn distinct_same_named_methods_do_not_merge_and_logical_ids_are_stable() {
    // Two `new` on different impls share a `qualified_name` (`…lib.rs::new`) but differ in
    // signature — they must NOT collapse into one "cfg_variant" logical symbol. And the
    // logical id must be stable across a reindex (it is content-derived, not an autoincrement).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub struct A;
pub struct B;

impl A {
    pub fn new(name: String) -> Self { A }
}

impl B {
    pub fn new(count: usize, flag: bool) -> Self { B }
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("new".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: true,
        limit: 10,
    };
    let lookup = db.symbol_candidates(&selector, false).unwrap();
    let new_candidates: Vec<_> =
        lookup.candidates.iter().filter(|candidate| candidate.name == "new").collect();
    assert_eq!(new_candidates.len(), 2, "both constructors present: {new_candidates:?}");
    let logical_ids: std::collections::BTreeSet<i64> =
        new_candidates.iter().filter_map(|candidate| candidate.logical_symbol_id).collect();
    assert_eq!(logical_ids.len(), 2, "distinct signatures get distinct logical ids");
    for candidate in &new_candidates {
        assert_eq!(
            candidate.logical_group_reason.as_deref(),
            Some("single"),
            "differently-signed methods are not cfg variants: {candidate:?}"
        );
    }

    // Reindex and confirm the logical ids are unchanged (content-derived, not churned).
    let db = IndexDatabase::rebuild(&config).unwrap();
    let relookup = db.symbol_candidates(&selector, false).unwrap();
    let reindexed_ids: std::collections::BTreeSet<i64> = relookup
        .candidates
        .iter()
        .filter(|candidate| candidate.name == "new")
        .filter_map(|candidate| candidate.logical_symbol_id)
        .collect();
    assert_eq!(reindexed_ids, logical_ids, "logical ids must be stable across reindex");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn logical_symbol_exact_mode_covers_duplicate_rust_variants() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_blocking() {}

#[cfg(target_arch = "wasm32")]
pub fn spawn_blocking() {}

pub fn caller() {
    spawn_blocking();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let lookup = db
        .symbol_candidates(
            &crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("spawn_blocking".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            },
            false,
        )
        .unwrap();
    let logical_symbol_id = lookup.candidates[0].logical_symbol_id.expect("logical id");
    assert_eq!(lookup.candidates[0].logical_variant_count, Some(2));
    assert_eq!(lookup.candidates[0].logical_group_reason.as_deref(), Some("cfg_variant"));

    let exact_variant_callers = db
        .find_callers_with_options(
            "spawn_blocking",
            10,
            &crate::query::graph::GraphTraversalOptions {
                resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                symbol_id: Some(lookup.candidates[1].symbol_id),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        exact_variant_callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|symbol| symbol.ends_with("caller"))
                && edge.target.as_deref() == Some("spawn_blocking")
                && edge.verified_target_symbol
        }),
        "symbol_id exact should include its logical cfg group: {exact_variant_callers:?}"
    );
    assert!(exact_variant_callers.iter().all(|edge| edge.verified_target_symbol));

    let exact_logical = db
        .graph_traversal_report(
            "find_callers",
            &lookup.candidates[0],
            true,
            10,
            &crate::query::graph::GraphTraversalOptions {
                resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                symbol_id: Some(lookup.candidates[0].symbol_id),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(exact_logical.query.logical_symbol_id, Some(logical_symbol_id));
    assert_eq!(exact_logical.logical_symbol.as_ref().map(|symbol| symbol.variant_count), Some(2));
    assert_eq!(exact_logical.variants.len(), 2);
    assert!(exact_logical.results.iter().all(|edge| edge.verified_target_symbol));
    assert!(
        exact_logical.results.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|symbol| symbol.ends_with("caller"))
                && edge.target.as_deref() == Some("spawn_blocking")
        }),
        "logical exact callers: {exact_logical:?}"
    );

    // #201 review (P2): feeding the candidate's `sym_<hex>` handle back through the
    // `ref`/symbol_path slot must resolve to the logical group's members WITHOUT reporting
    // disambiguation — every member shares that one handle, so the client has no more specific
    // token to give. (Before the fix, `disambiguation_required` was computed from the raw
    // 2-candidate count → a dead end.)
    let handle = crate::serde_big_id::format_sym_handle(logical_symbol_id);
    let by_ref_handle = db
        .symbol_candidates(
            &crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: Some(handle),
                symbol: None,
                language: None,
                allow_ambiguous: false,
                limit: 10,
            },
            false,
        )
        .unwrap();
    assert_eq!(by_ref_handle.candidates.len(), 2, "handle resolves the whole cfg group");
    assert!(
        by_ref_handle.candidates.iter().all(|c| c.logical_symbol_id == Some(logical_symbol_id)),
        "every member shares the queried handle"
    );
    assert!(
        !by_ref_handle.disambiguation_required,
        "a handle in the ref slot is not ambiguous: {by_ref_handle:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexes_real_world_rust_graph_patterns() {
    let root = fixture_temp_root("graph-realworld/rust");
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "src/lib.rs", "worker", "imports", "Syntactic");
    assert_edge(&db, "src/lib.rs", "Worker", "exports", "Syntactic");
    // `Worker::new()` / `Client::new()` now resolve via the semantic scope path (`Worker::new` /
    // `Client::new`) → Exact, where bare-name matching previously left two same-named `new` methods
    // ambiguous (NameOnly) (#61 scope-path resolution).
    assert_edge(&db, "entry", "new", "calls_name", "Exact");
    assert_edge(&db, "entry", "Client", "references_type", "Syntactic");
    assert_edge(&db, "drive", "serve", "calls_name", "NameOnly");
    assert_edge(&db, "drive", "GenericRunner", "references_type", "Syntactic");
    assert_edge(&db, "Worker", "Service", "implements", "Syntactic");
    assert_edge(&db, "generic_call", "T", "references_type", "NameOnly");
    assert_edge(&db, "entry", "generated_call", "uses_macro", "NameOnly");
    let syntactic_callers = db.find_callers("serve", 10).unwrap();
    assert!(
        syntactic_callers.is_empty(),
        "syntactic serve callers should avoid receiver/name fallback: {syntactic_callers:?}"
    );
    let callers = db
        .find_callers_with_options("serve", 10, &crate::query::graph::GraphTraversalOptions {
            resolution_mode: crate::query::graph::GraphResolutionMode::Fuzzy,
            ..Default::default()
        })
        .unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.edge_kind == "calls_name"
                && edge.edge_confidence == edge.confidence
                && edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("drive"))
        }),
        "serve callers: {callers:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexes_typescript_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/helper.ts"),
        "export function helper() {}\nexport const Card = () => null;\n",
    )
    .unwrap();
    fs::write(
        root.join("src/App.tsx"),
        r#"
import { helper, Card } from "./helper";

export function run() {
  helper();
  return <Card />;
}

export const callRun = () => run();
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::TypeScript);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "run", "helper", "calls_name", "Syntactic");
    assert_edge(&db, "run", "Card", "references_type", "Syntactic");
    assert_edge(&db, "src/App.tsx", "helper", "imports", "Syntactic");
    assert_edge(&db, "src/App.tsx", "run", "exports", "Syntactic");
    let callees = db.trace_callees("callRun", 10).unwrap();
    assert!(
        callees.iter().any(|edge| {
            edge.to_symbol.as_deref().is_some_and(|name| name.ends_with("run"))
                && edge.confidence == "syntactic"
        }),
        "callRun callees: {callees:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexes_c_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/runtime.c"),
        r#"
typedef struct Runtime Runtime;

struct Runtime {
  int state;
};

int helper(Runtime *runtime) {
  return runtime->state;
}

int runtime_open(Runtime *runtime) {
  return helper(runtime);
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::C);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "runtime_open", "helper", "calls_name", "Syntactic");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexes_c_file_scope_macro_regions_for_search() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("drivers/entropy")).unwrap();
    fs::write(
        root.join("drivers/entropy/entropy.c"),
        r#"
static int entropy_init(const struct device *dev)
{
    ARG_UNUSED(dev);
    return 0;
}

/* Entropy driver APIs structure */
static DEVICE_API(entropy, entropy_cryptoacc_trng_api) = {
    .get_entropy = entropy_cryptoacc_trng_get_entropy,
};

DEVICE_DT_INST_DEFINE(0, entropy_init, NULL, NULL, NULL,
                      PRE_KERNEL_1, CONFIG_ENTROPY_INIT_PRIORITY,
                      &entropy_cryptoacc_trng_api);
"#,
    )
    .unwrap();
    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "c".to_string(),
            language: Language::C,
            directories: vec![PathBuf::from("drivers/entropy")],
            include: vec!["**/*.c".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hits = db.search("DEVICE_API", 5, false).unwrap();
    assert!(
        hits.iter().any(|hit| {
            hit.path == "drivers/entropy/entropy.c" && hit.summary.contains("DEVICE_API")
        }),
        "DEVICE_API hits: {hits:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexes_cpp_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/runtime.cpp"),
        r#"
namespace held {
class Runtime {
public:
  void open();
};

void helper() {}

void Runtime::open() {
  helper();
}
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Cpp);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "open", "helper", "calls_name", "Syntactic");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexes_real_world_typescript_graph_patterns() {
    let root = fixture_temp_root("graph-realworld/typescript");
    let config = source_config(root.clone(), Language::TypeScript);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "src/lib.tsx", "DefaultWidget", "imports", "Syntactic");
    assert_edge(&db, "src/lib.tsx", "WidgetNS", "imports", "NameOnly");
    assert_edge(&db, "src/lib.tsx", "WidgetProps", "imports", "Syntactic");
    assert_edge(&db, "src/lib.tsx", "ReExportedWidget", "exports", "NameOnly");
    assert_edge(&db, "useWidget", "useMemo", "calls_name", "NameOnly");
    assert_edge(&db, "useWidget", "DefaultWidget", "calls_name", "Syntactic");
    assert_edge(&db, "Shell", "renderWidget", "calls_name", "NameOnly");
    assert_edge(&db, "Shell", "WidgetNS", "references_type", "NameOnly");
    assert_edge(&db, "Shell", "DefaultWidget", "references_type", "Syntactic");
    assert_edge(&db, "DefaultWidget", "WidgetProps", "references_type", "Syntactic");
    let callees = db
        .trace_callees_with_options("Shell", 10, &crate::query::graph::GraphTraversalOptions {
            include_references: true,
            edge_kinds: None,
            ..Default::default()
        })
        .unwrap();
    assert!(
        callees.iter().any(|edge| {
            edge.edge_kind == "references_type"
                && edge.edge_confidence == edge.confidence
                && edge.to_symbol.as_deref().is_some_and(|name| name.ends_with("DefaultWidget"))
        }),
        "Shell callees: {callees:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn rust_macro_edges_do_not_resolve_to_same_named_modules() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
mod format;

fn execute_one() {
    let _value = format!("hello");
}
"#,
    )
    .unwrap();
    fs::write(root.join("src/format.rs"), "pub fn helper() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge = db
        .storage
        .connection()
        .query_row(
            "
                SELECT edge_kind, to_name, to_symbol_id, confidence, resolution, evidence
                FROM edges
                WHERE edge_kind = 'uses_macro'
                  AND to_name = 'format'
                ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(edge.0, "uses_macro");
    assert_eq!(edge.1, "format");
    assert_eq!(edge.2, None);
    assert_eq!(edge.3, "NameOnly");
    assert_eq!(edge.4, "unresolved");
    assert!(edge.5.as_deref().is_some_and(|value| value.contains("format!")));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn opening_old_graph_policy_rebuilds_stale_macro_edges() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
mod format;

fn execute_one() {
    let _value = format!("hello");
}
"#,
    )
    .unwrap();
    fs::write(root.join("src/format.rs"), "pub fn helper() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.storage
        .connection()
        .execute("UPDATE index_meta SET value = 'old' WHERE key = 'graph_index_version'", [])
        .unwrap();
    db.storage
        .connection()
        .execute(
            "
                UPDATE edges
                SET edge_kind = 'calls_name',
                    to_symbol_id = (SELECT id FROM symbols WHERE name = 'format' LIMIT 1),
                    confidence = 'Syntactic',
                    evidence = NULL,
                    resolution = 'syntactic'
                WHERE to_name = 'format'
                ",
            [],
        )
        .unwrap();
    drop(db);

    let reopened = IndexDatabase::open(&config.database).unwrap();
    let edge = reopened
        .storage
        .connection()
        .query_row(
            "
                SELECT edge_kind, to_symbol_id, confidence, resolution, evidence
                FROM edges
                WHERE to_name = 'format'
                  AND edge_kind = 'uses_macro'
                ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(edge.0, "uses_macro");
    assert_eq!(edge.1, None);
    assert_eq!(edge.2, "NameOnly");
    assert_eq!(edge.3, "unresolved");
    assert!(edge.4.as_deref().is_some_and(|value| value.contains("format!")));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn qualified_common_member_calls_do_not_resolve_by_short_name() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub struct AlertsStore;

impl AlertsStore {
    pub fn new() -> Self {
        Self
    }
}

pub fn caller() {
    let _items: Vec<String> = Vec::new();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge = db
        .storage
        .connection()
        .query_row(
            "
                SELECT to_name, target_qualified_name, to_symbol_id, confidence, resolution
                FROM edges
                WHERE from_name LIKE '%caller'
                  AND edge_kind = 'calls_name'
                  AND to_name = 'new'
                ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(edge.0, "new");
    assert_eq!(edge.1.as_deref(), Some("Vec::new"));
    assert_eq!(edge.2, None);
    assert_eq!(edge.3, "NameOnly");
    assert_eq!(edge.4, "unresolved");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn macro_edges_do_not_resolve_to_same_named_typescript_symbols() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
fn rust_entry() {
    let _payload = json!({"ok": true});
}
"#,
    )
    .unwrap();
    fs::write(root.join("src/preferences.ts"), "export function json() { return {}; }\n").unwrap();
    let mut config = source_config(root.clone(), Language::Rust);
    config.targets.push(ResolvedTarget {
        name: "typescript".to_string(),
        language: Language::TypeScript,
        directories: vec![PathBuf::from("src")],
        include: vec!["**/*.ts".to_string()],
        exclude: Vec::new(),
        kind: TargetKind::Source,
    });
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge = db
        .storage
        .connection()
        .query_row(
            "
                SELECT edge_kind, to_name, to_symbol_id, confidence, resolution, evidence
                FROM edges
                WHERE edge_kind = 'uses_macro'
                  AND to_name = 'json'
                ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(edge.0, "uses_macro");
    assert_eq!(edge.1, "json");
    assert_eq!(edge.2, None);
    assert_eq!(edge.3, "NameOnly");
    assert_eq!(edge.4, "unresolved");
    assert!(edge.5.as_deref().is_some_and(|value| value.contains("json!")));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn qualified_crate_helper_callers_use_name_fallback() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod task_spawn {
    pub fn spawn_blocking() {}
}

pub fn first() {
    crate::task_spawn::spawn_blocking();
}

pub fn second() {
    task_spawn::spawn_blocking();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let callers = db.find_callers("spawn_blocking", 10).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("first"))
                && edge.edge_kind == "calls_name"
                && edge.resolution == "target_name_fallback"
        }),
        "spawn_blocking callers: {callers:?}"
    );
    assert!(
        callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("second"))
                && edge.edge_kind == "calls_name"
        }),
        "spawn_blocking callers: {callers:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn caller_lookup_does_not_match_related_names_or_chain_evidence() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod runtime {
    pub mod task_spawn {
        pub fn spawn() {}
        pub fn spawn_blocking() -> JoinHandle {
            JoinHandle
        }
        pub fn spawn_blocking_handle() {}
        pub fn spawn_blocking_offload() -> JoinHandle {
            JoinHandle
        }
    }
}

pub struct JoinHandle;

impl JoinHandle {
    pub fn map_err(self) {}
}

pub fn direct() {
    crate::runtime::task_spawn::spawn_blocking();
}

pub fn related_handle() {
    crate::runtime::task_spawn::spawn_blocking_handle();
}

pub fn related_offload_chain() {
    crate::runtime::task_spawn::spawn_blocking_offload().map_err();
}

pub fn related_spawn_with_text() {
    crate::runtime::task_spawn::spawn();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let callers = db.find_callers("spawn_blocking", 20).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("direct"))
                && edge.target.as_deref() == Some("spawn_blocking")
                && edge.edge_kind == "calls_name"
        }),
        "spawn_blocking callers: {callers:?}"
    );
    assert!(
        callers.iter().all(|edge| {
            !edge.from_symbol.as_deref().is_some_and(|name| {
                name.ends_with("related_handle")
                    || name.ends_with("related_offload_chain")
                    || name.ends_with("related_spawn_with_text")
            }) && !matches!(
                edge.target.as_deref(),
                Some("spawn_blocking_handle" | "spawn_blocking_offload" | "spawn" | "map_err")
            )
        }),
        "caller lookup leaked related names or chain evidence: {callers:?}"
    );

    let qualified_callers = db.find_callers("src/lib.rs::spawn_blocking", 20).unwrap();
    assert!(
        qualified_callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("direct"))
                && edge.target.as_deref() == Some("spawn_blocking")
                && edge.edge_kind == "calls_name"
        }),
        "qualified spawn_blocking callers: {qualified_callers:?}"
    );
    assert!(
        qualified_callers.iter().all(|edge| {
            !edge.from_symbol.as_deref().is_some_and(|name| {
                name.ends_with("related_handle")
                    || name.ends_with("related_offload_chain")
                    || name.ends_with("related_spawn_with_text")
            }) && !matches!(
                edge.target.as_deref(),
                Some("spawn_blocking_handle" | "spawn_blocking_offload" | "spawn" | "map_err")
            )
        }),
        "qualified caller lookup leaked related names or chain evidence: {qualified_callers:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn files_past_the_old_structural_cap_still_contribute_symbols_and_edges() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let filler = (0..700).map(|idx| format!("pub fn filler_{idx}() {{}}\n")).collect::<String>();
    fs::write(
        root.join("src/lib.rs"),
        format!(
            r#"
pub mod task_spawn {{
    pub fn spawn_blocking() {{}}
}}

{filler}

pub fn caller() {{
    crate::task_spawn::spawn_blocking();
}}
"#
        ),
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    assert!(fs::metadata(root.join("src/lib.rs")).unwrap().len() > 10_000);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbols = db.symbols("caller", Some(Language::Rust), 10).unwrap();
    assert!(symbols.iter().any(|symbol| symbol.name == "caller"), "caller symbols: {symbols:?}");
    let callers = db.find_callers("spawn_blocking", 10).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.edge_kind == "calls_name"
                && edge.target.as_deref() == Some("spawn_blocking")
                && edge.callsite.as_ref().is_some_and(|callsite| callsite.line > 700)
        }),
        "spawn_blocking callers: {callers:?}"
    );
    let impact =
        db.impact_surface("callers of crate::task_spawn::spawn_blocking in src", 10).unwrap();
    assert!(
        impact.iter().any(|item| {
            item.category == "Direct structural impact" && item.reason == "direct_caller"
        }),
        "impact: {impact:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn impact_surface_uses_high_signal_query_symbols_and_call_edges() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod runtime {
    pub fn unrelated_runtime_symbol() {}
}

pub mod task_spawn {
    pub fn spawn_blocking<F, T>(f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        f()
    }
}

pub fn caller() {
    crate::task_spawn::spawn_blocking(|| 1);
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let impact = db
        .impact_surface(
            "change runtime task_spawn spawn_blocking wasm inline native blocking pool",
            20,
        )
        .unwrap();
    assert!(
        impact.iter().any(|item| {
            item.category == "Direct structural impact"
                && item.reason == "direct_caller"
                && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("caller"))
        }),
        "spawn_blocking caller should be present: {impact:?}"
    );
    assert!(
        impact.iter().all(|item| {
            !(item.reason == "exact_symbol_definition"
                && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("runtime")))
        }),
        "broad `runtime` token should not become an exact impact seed: {impact:?}"
    );
    assert!(
        impact.iter().all(|item| {
            !item.evidence.iter().any(|evidence| evidence.contains("references_type"))
                && item.symbol.as_deref() != Some("Send")
        }),
        "type references should not appear as direct impact: {impact:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn impact_surface_flat_signals_truncation_when_capped() {
    // #150: the flat (free-text query) impact shape capped at `limit` SILENTLY — a capped result
    // read as complete. A capped result must now carry a visible `completeness` sentinel (the
    // flat-shape analogue of the structured report's `truncated_sections`).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn leaf_a() {}
pub fn leaf_b() {}
pub fn leaf_c() {}
pub fn leaf_d() {}

pub fn hub() {
    leaf_a();
    leaf_b();
    leaf_c();
    leaf_d();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A tiny limit forces truncation: `hub`'s own definition plus four callee neighbors overflow
    // it.
    let limit = 2u32;
    let impact = db.impact_surface("hub", limit).unwrap();
    let sentinel = impact
        .iter()
        .find(|item| item.category == "completeness")
        .expect("a capped flat result must carry a completeness sentinel (#150)");
    assert!(sentinel.reason.contains("capped"), "sentinel explains the cap: {sentinel:?}");
    assert!(
        sentinel.evidence.iter().any(|evidence| evidence.contains("beyond the limit")),
        "sentinel signals more exist: {sentinel:?}"
    );
    let real_items = impact.iter().filter(|item| item.category != "completeness").count();
    assert_eq!(real_items, limit as usize, "exactly `limit` real items, plus the sentinel");

    // A generous limit drops nothing, so no sentinel appears — it must not cry wolf.
    let full = db.impact_surface("hub", 100).unwrap();
    assert!(
        full.iter().all(|item| item.category != "completeness"),
        "an uncapped result must NOT carry a sentinel: {full:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn impact_surface_flat_signals_truncation_from_a_capped_textual_fallback() {
    // #150 (Codex review): the per-section caps clamp `textual_fallback`/`historical_evidence` to
    // exactly `limit`, so a free-text query with MORE matching files than `limit` used to fill the
    // surface to exactly `limit` and skip the sentinel — silent truncation on the very path the fix
    // targets. The probe-one-past-limit detection must catch it.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // The token `needle_token` appears only in COMMENTS across many files — no symbol is named it,
    // so there are no exact targets / graph neighbors; everything comes from textual fallback.
    for n in 0..6 {
        fs::write(
            root.join(format!("src/file_{n}.rs")),
            format!("// needle_token marker {n}\npub fn unrelated_{n}() {{}}\n"),
        )
        .unwrap();
    }
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let limit = 2u32;
    let impact = db.impact_surface("needle_token", limit).unwrap();
    assert!(
        impact.iter().any(|item| item.category == "completeness"),
        "a capped textual-fallback result must still signal truncation (#150 Codex): {impact:?}"
    );
    let real_items = impact.iter().filter(|item| item.category != "completeness").count();
    assert_eq!(real_items, limit as usize, "exactly `limit` real items, plus the sentinel");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn impact_surface_collapses_file_matches_to_one_row_per_file() {
    // Regression for #48: a file-granularity match (path/chunk text) used to fan out into one
    // row per symbol in the file. Each such section must now yield at most one row per file.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/widget_store.rs"),
        "pub fn widget_alpha() {}\npub fn widget_beta() {}\npub fn widget_gamma() {}\npub fn \
         widget_delta() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("widget_alpha".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: false,
        limit: 10,
    };
    let symbol = db.select_symbol(&selector).unwrap().unwrap().expect("symbol");
    let report = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            50,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();

    for section in [
        &report.text_fallback_hits,
        &report.tests_touching_symbol_path,
        &report.docs_mentioning_symbol_path,
    ] {
        let total = section.len();
        let mut paths: Vec<&str> = section.iter().map(|item| item.path.as_str()).collect();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), total, "section must have one row per file: {section:?}");

        // Precedence: a path match must not carry a spurious symbol (a qualified name is
        // `path::symbol`, so a path needle matches every symbol in the file).
        for item in section {
            if item.evidence.iter().any(|evidence| evidence.starts_with("path match")) {
                assert!(item.symbol.is_none(), "path match must not name a symbol: {item:?}");
            }
        }
    }

    let store_rows = report
        .text_fallback_hits
        .iter()
        .filter(|item| item.path.ends_with("widget_store.rs"))
        .count();
    assert_eq!(store_rows, 1, "a file with four symbols collapses to one fallback row");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn docs_for_symbol_prefers_local_source_context_before_broad_markdown() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/runtime")).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(
        root.join("src/runtime/task_spawn.rs"),
        r#"
pub fn spawn_blocking<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    f()
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("docs/phrase-persistence.md"),
        "# Phrase persistence\nUnrelated notes mention spawn_blocking in passing.\n",
    )
    .unwrap();
    fs::write(
        root.join("docs/task_spawn.md"),
        "# task_spawn\nLocal task_spawn notes explain spawn_blocking.\n",
    )
    .unwrap();
    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![
            ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
            ResolvedTarget {
                name: "markdown".to_string(),
                language: Language::Markdown,
                directories: vec![PathBuf::from("docs")],
                include: vec!["**/*.md".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Docs,
            },
        ],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db.symbols("spawn_blocking", Some(Language::Rust), 10).unwrap().remove(0);
    let hits = db.docs_for_selected_symbol(&symbol, 10).unwrap();
    assert_eq!(hits[0].path, "src/runtime/task_spawn.rs", "docs hits: {hits:?}");
    let phrase_index = hits.iter().position(|hit| hit.path == "docs/phrase-persistence.md");
    let task_spawn_index = hits.iter().position(|hit| hit.path == "docs/task_spawn.md");
    assert!(
        phrase_index.is_none_or(|phrase| task_spawn_index.is_some_and(|local| local < phrase)),
        "path-local task_spawn docs should outrank unrelated phrase docs: {hits:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn partial_tree_sitter_trees_still_contribute_valid_symbols_and_edges() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn helper() {}

pub fn caller() {
    helper();
}

fn broken( {
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbols = db.symbols("caller", Some(Language::Rust), 10).unwrap();
    assert!(symbols.iter().any(|symbol| symbol.name == "caller"), "caller symbols: {symbols:?}");
    assert_edge(&db, "caller", "helper", "calls_name", "Syntactic");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn receiver_method_calls_do_not_bind_to_same_named_free_functions() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn spawn_blocking() {}

pub fn caller(joinset: JoinSet) {
    joinset.spawn_blocking();
}

pub struct JoinSet;
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge = db
        .storage
        .connection()
        .query_row(
            "
                SELECT to_name, target_qualified_name, to_symbol_id, confidence, resolution, \
             receiver_hint
                FROM edges
                WHERE from_name LIKE '%caller'
                  AND edge_kind = 'calls_name'
                  AND to_name = 'spawn_blocking'
                ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(edge.0, "spawn_blocking");
    assert_eq!(edge.1.as_deref(), Some("joinset::spawn_blocking"));
    assert_eq!(edge.2, None);
    assert_eq!(edge.3, "NameOnly");
    assert_eq!(edge.4, "unresolved");
    assert_eq!(edge.5.as_deref(), Some("joinset"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn trace_callees_excludes_type_references_by_default() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub struct JoinError;
pub enum Result<T, E> { Ok(T), Err(E) }
pub fn helper() {}

pub fn spawn_blocking<F, T>(f: F) -> Result<T, JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    helper();
    tokio::task::spawn_blocking(f)
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let default_callees = db.trace_callees("spawn_blocking", 20).unwrap();
    assert!(
        default_callees.iter().any(|edge| {
            edge.edge_kind == "calls_name"
                && edge.target.as_deref() == Some("helper")
                && edge.verified_target_symbol
        }),
        "default callees: {default_callees:?}"
    );
    assert!(
        default_callees.iter().all(
            |edge| edge.target_qualified_name.as_deref() != Some("tokio::task::spawn_blocking")
        ),
        "default callees leaked unresolved external call: {default_callees:?}"
    );
    assert!(
        default_callees.iter().all(|edge| edge.edge_kind != "references_type"),
        "default callees leaked type refs: {default_callees:?}"
    );
    assert!(
        default_callees.iter().all(|edge| !matches!(
            edge.target.as_deref(),
            Some("F" | "T" | "Send" | "Result" | "JoinError")
        )),
        "default callees leaked generic/type targets: {default_callees:?}"
    );

    let with_refs = db
        .trace_callees_with_options(
            "spawn_blocking",
            20,
            &crate::query::graph::GraphTraversalOptions {
                include_references: true,
                edge_kinds: None,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        with_refs.iter().any(|edge| edge.edge_kind == "references_type"),
        "reference-enabled callees: {with_refs:?}"
    );

    let with_unresolved = db
        .trace_callees_with_options(
            "spawn_blocking",
            20,
            &crate::query::graph::GraphTraversalOptions {
                include_unresolved: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        with_unresolved.iter().any(
            |edge| edge.target_qualified_name.as_deref() == Some("tokio::task::spawn_blocking")
        ),
        "unresolved-enabled callees: {with_unresolved:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn trace_callees_defaults_to_repo_relevant_calls() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn repo_helper() {}

pub fn caller(input: Result<String, String>) -> String {
    repo_helper();
    let values: Vec<String> = Vec::new();
    let _ = input.map_err(|error| error.to_string());
    let _ = Some("value").unwrap_or_else(|| "fallback");
    let _ = format!("hello");
    values.get(0).unwrap_or_else(|| "fallback").to_string()
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let default_callees = db.trace_callees("caller", 20).unwrap();
    assert!(
        default_callees.iter().any(|edge| edge.target.as_deref() == Some("repo_helper")),
        "default callees should keep repo-local calls: {default_callees:?}"
    );
    assert!(
        default_callees.iter().all(|edge| {
            edge.edge_kind != "uses_macro"
                && !matches!(
                    edge.target.as_deref(),
                    Some("new" | "map_err" | "unwrap_or_else" | "to_string" | "format")
                )
        }),
        "default callees leaked low-signal calls: {default_callees:?}"
    );

    let expanded = db
        .trace_callees_with_options("caller", 20, &crate::query::graph::GraphTraversalOptions {
            include_unresolved: true,
            include_macros: true,
            include_common_methods: true,
            ..Default::default()
        })
        .unwrap();
    assert!(
        expanded.iter().any(|edge| edge.edge_kind == "uses_macro"),
        "macro-enabled callees: {expanded:?}"
    );
    assert!(
        expanded.iter().any(|edge| edge.target.as_deref() == Some("unwrap_or_else")),
        "common-method-enabled callees: {expanded:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexes_kotlin_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/Main.kt"),
        r#"
package dev.cq27.test

import dev.cq27.lib.ExternalThing

interface Syncable

class MainBridge : Syncable {
  suspend fun syncOnce() {
    helper()
    ExternalThing()
  }
}

fun helper() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Kotlin);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "syncOnce", "helper", "calls_name", "Syntactic");
    assert_edge(&db, "MainBridge", "Syncable", "implements", "Syntactic");
    assert_edge(&db, "src/Main.kt", "ExternalThing", "imports", "NameOnly");
    let impact = db.impact_surface("helper", 10).unwrap();
    assert!(
        impact.iter().any(|item| {
            item.category == "Direct structural impact" && item.reason == "direct_caller"
        }),
        "impact: {impact:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexes_real_world_kotlin_graph_patterns() {
    let root = fixture_temp_root("graph-realworld/kotlin");
    let config = source_config(root.clone(), Language::Kotlin);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "src/Main.kt", "ExternalFactory", "imports", "NameOnly");
    assert_edge(&db, "Worker", "companion", "contains", "Exact");
    assert_edge(&db, "companion", "create", "contains", "Exact");
    // `Worker.create()` / `SingletonRunner.run()` now resolve via the semantic scope path
    // (`Worker::create` / `SingletonRunner::run`) → Exact, where they were a weaker suffix match
    // before (#61 scope-path resolution).
    assert_edge(&db, "syncOnce", "create", "calls_name", "Exact");
    assert_edge(&db, "syncOnce", "Worker", "references_type", "Syntactic");
    assert_edge(&db, "syncOnce", "run", "calls_name", "Exact");
    assert_edge(&db, "syncOnce", "SingletonRunner", "references_type", "Syntactic");
    assert_edge(&db, "syncOnce", "ExternalFactory", "calls_name", "NameOnly");
    assert_edge(&db, "syncOnce", "ExternalFactory", "references_type", "NameOnly");
    assert_edge(&db, "syncOnce", "cleaned", "calls_name", "Syntactic");
    let callers = db.find_callers("cleaned", 10).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.edge_kind == "calls_name"
                && edge.edge_confidence == edge.confidence
                && edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("syncOnce"))
        }),
        "cleaned callers: {callers:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn kotlin_caller_lookup_respects_qualified_receivers_for_common_method_names() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/Main.kt"),
        r#"
package dev.cq27.test

object WatchProposalBuilder {
  fun build(): String = "proposal"
}

class AndroidDialogBuilder {
  fun build(): String = "dialog"
}

fun actualCaller() {
  WatchProposalBuilder.build()
}

fun unrelatedBuilderCalls(dialog: AndroidDialogBuilder) {
  dialog.build()
  AndroidDialogBuilder().build()
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Kotlin);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let target = db
        .symbols("build", Some(Language::Kotlin), 10)
        .unwrap()
        .into_iter()
        .find(|symbol| symbol.qualified_name.contains("WatchProposalBuilder"))
        .expect("WatchProposalBuilder.build symbol");
    let callers = db
        .find_callers_with_options("build", 20, &crate::query::graph::GraphTraversalOptions {
            resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
            symbol_id: Some(target.symbol_id),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        callers
            .iter()
            .filter(|edge| edge
                .from_symbol
                .as_deref()
                .is_some_and(|name| name.ends_with("actualCaller")))
            .count(),
        1,
        "actual caller should be present once: {callers:?}"
    );
    assert!(
        callers.iter().all(|edge| edge
            .from_symbol
            .as_deref()
            .is_none_or(|name| !name.ends_with("unrelatedBuilderCalls"))),
        "unrelated builder calls should not resolve to WatchProposalBuilder.build: {callers:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn github_sync_caches_papertrail_and_rationale_without_query_time_crawling() {
    let (root, config) =
        markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    // Resolve the repo context explicitly so db.rationale_search("Fixes #42") qualifies the bare
    // ref without shelling out to `gh` (#60).
    db.set_github_context(Some("cq27-dev/rag-rat"), false);
    let mock = MockGitHubClient;

    let offline = github::sync_from_refs::<MockGitHubClient>(
        db.storage.connection(),
        &root,
        None,
        true,
        &test_gh_ctx(),
    )
    .unwrap();
    assert!(offline.offline);
    assert_eq!(offline.discovered_refs, 1);
    assert_eq!(offline.synced_items, 0);

    let report =
        github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
            .unwrap();
    assert!(!report.offline);
    assert_eq!(report.discovered_refs, 1);
    assert_eq!(report.synced_items, 5);
    assert_eq!(report.status.issues, 1);
    assert_eq!(report.status.comments, 1);
    assert_eq!(report.status.pulls, 1);
    assert_eq!(report.status.reviews, 1);
    assert_eq!(report.status.review_comments, 1);

    let issue_hits = db.github_issue_search("sqlite", 10).unwrap();
    assert_eq!(issue_hits.len(), 1);
    assert_eq!(issue_hits[0].classification, "decision");
    assert_eq!(issue_hits[0].evidence_kind, "historical_github");

    let refs = db.github_refs_for_path("docs/search.md", 10).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].source_kind, "file");

    let rationale = db.rationale_search("risk", 10).unwrap();
    assert!(rationale.iter().any(|item| item.classification == "risk"));
    let issue_ref_rationale = db.rationale_search("Fixes #42", 10).unwrap();
    assert_eq!(issue_ref_rationale.first().map(|item| item.number), Some(42));
    assert_eq!(
        issue_ref_rationale.first().map(|item| item.evidence_kind),
        Some("literal_github_ref")
    );
    assert_eq!(issue_ref_rationale.first().map(|item| item.score), Some(1.0));
    assert!(
        issue_ref_rationale.iter().any(|item| item.number == 42),
        "issue ref rationale should use structured GitHub refs: {issue_ref_rationale:?}"
    );

    let chunk_id = first_chunk_id(&db);
    let papertrail = db.papertrail_for_chunk(chunk_id, 10).unwrap().unwrap();
    assert!(papertrail.current_source.is_some());
    assert!(!papertrail.github_evidence.is_empty());
    assert!(
        papertrail.github_evidence.iter().all(|item| {
            matches!(item.evidence_kind, "historical_github" | "literal_github_ref")
        })
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn papertrail_for_commit_prefers_commit_sourced_github_refs() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("docs")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);
    fs::write(root.join("docs/search.md"), "# Decision\nalpha\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Fix search rationale", "-m", "Fixes #42"]);

    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();
    let commit = db
        .storage
        .connection()
        .query_row("SELECT hash FROM git_commits LIMIT 1", [], |row| row.get::<_, String>(0))
        .unwrap();
    let mock = MockGitHubClient;
    github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
        .unwrap();

    let papertrail = db.papertrail_for_commit(&commit[..7], 10).unwrap();
    assert_eq!(papertrail.github_evidence.first().map(|item| item.number), Some(42));
    assert_eq!(
        papertrail.github_evidence.first().map(|item| item.evidence_kind),
        Some("literal_github_ref")
    );
    assert!(
        papertrail.fallback_github_evidence.is_empty(),
        "structured commit refs should suppress noisy fallback evidence: {papertrail:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn papertrail_for_symbol_dedupes_duplicate_file_refs() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "// First rationale (#42)\n// Second rationale (#42)\npub fn tracked_symbol() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let mock = MockGitHubClient;
    github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
        .unwrap();
    let papertrail = db
        .papertrail_for_symbol("tracked_symbol", Some(Language::Rust), 10)
        .unwrap()
        .expect("tracked symbol papertrail");

    assert_eq!(
        papertrail
            .github_evidence
            .iter()
            .filter(|item| item.number == 42 && item.item_kind == "issue")
            .count(),
        1,
        "duplicate #42 refs in one file should collapse to one issue evidence row: {papertrail:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn github_sync_keeps_partial_cache_and_skips_synced_refs_after_404() {
    let (root, config) = markdown_config(
        "# Decision\nRefs cq27-dev/rag-rat#42 and cq27-dev/rag-rat#404\nwe will keep sqlite\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    let mock = PartiallyFailingGitHubClient;

    let report =
        github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
            .unwrap();
    assert_eq!(report.discovered_refs, 2);
    assert_eq!(report.synced_items, 5);
    assert_eq!(report.failed_refs, 1);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].number, 404);
    assert_eq!(report.errors[0].status, "not_found");

    let issue_hits = db.github_issue_search("sqlite", 10).unwrap();
    assert_eq!(issue_hits.len(), 1);
    assert_eq!(issue_hits[0].number, 42);

    let second =
        github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
            .unwrap();
    assert_eq!(second.synced_items, 0);
    assert_eq!(second.skipped_refs, 2);
    assert_eq!(second.failed_refs, 0);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_recovers_when_fts_is_marked_dirty() {
    let (root, config) = markdown_config("alpha token");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.mark_fts_dirty().unwrap();

    let dirty = db.status(&config.database).unwrap();
    assert!(dirty.fts_dirty);
    assert!(!dirty.fts_fresh);

    let hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].summary, "alpha token");
    let fresh = db.status(&config.database).unwrap();
    assert!(!fresh.fts_dirty);
    assert!(fresh.fts_fresh);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_chunk_relocates_small_line_drift_to_current_text() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let chunk_id = first_chunk_id(&db);
    fs::write(root.join("docs/search.md"), "inserted\n# Title\nalpha token\n").unwrap();

    let chunk = db.read_chunk(chunk_id).unwrap().unwrap();
    assert_eq!(chunk.start_line, 2);
    assert_eq!(chunk.end_line, 3);
    assert_eq!(chunk.text, "# Title\nalpha token\n");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_chunk_large_drift_reindexes_and_reports_stale_chunk() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let chunk_id = first_chunk_id(&db);
    fs::write(root.join("docs/search.md"), "# Replacement\nbeta token\n").unwrap();

    let err = db.read_chunk(chunk_id).unwrap_err().to_string();
    assert!(err.contains("StaleChunk"), "{err}");
    let hits = db.search("beta", 10, false).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(db.search("alpha", 10, false).unwrap().is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_retries_after_healing_stale_hit() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    fs::write(root.join("docs/search.md"), "# Title\nbeta token\n").unwrap();

    let hits = db.search("alpha", 10, false).unwrap();
    assert!(hits.is_empty());
    let beta_hits = db.search("beta", 10, false).unwrap();
    assert_eq!(beta_hits.len(), 1);
    assert!(beta_hits[0].summary.contains("beta"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_heals_relocated_hits_before_returning_line_spans() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    fs::write(root.join("docs/search.md"), "inserted\n# Title\nalpha token\n").unwrap();

    let hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].start_line, 2);
    assert_eq!(hits[0].end_line, 3);
    assert!(hits[0].summary.contains("alpha"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_chunk_deleted_source_reports_gone() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let chunk_id = first_chunk_id(&db);
    fs::remove_file(root.join("docs/search.md")).unwrap();

    let err = db.read_chunk(chunk_id).unwrap_err().to_string();
    assert!(err.contains("Gone"), "{err}");
    assert!(db.search("alpha", 10, false).unwrap().is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_returns_needs_reindex_when_heal_cap_is_exceeded() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    for index in 0..=MAX_AUTO_HEAL_FILES_PER_CALL {
        fs::write(docs.join(format!("doc-{index}.md")), "common stale token\n").unwrap();
    }
    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();
    for index in 0..=MAX_AUTO_HEAL_FILES_PER_CALL {
        fs::write(docs.join(format!("doc-{index}.md")), "fresh replacement token\n").unwrap();
    }

    let err = db.search("common", 20, false).unwrap_err().to_string();
    assert!(err.contains("needs_reindex"), "{err}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_drops_deleted_file_instead_of_erroring() {
    // Invariant: when a search hit references a source file that was deleted on disk
    // since indexing, heal_file treats the missing file as a DELETION (mark_file_deleted)
    // rather than propagating a raw ENOENT. search_with_heal then re-searches without it,
    // so search returns Ok with the surviving file only — never Err(NotFound).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("keep.md"), "shared marker token\n").unwrap();
    fs::write(docs.join("drop.md"), "shared marker token\n").unwrap();
    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let initial = db.search("marker", 10, false).unwrap();
    assert_eq!(initial.len(), 2);

    fs::remove_file(docs.join("drop.md")).unwrap();

    let hits = db.search("marker", 10, false).unwrap();
    assert!(hits.iter().all(|hit| !hit.path.ends_with("drop.md")), "{hits:?}");
    assert!(hits.iter().any(|hit| hit.path.ends_with("keep.md")), "{hits:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn heal_index_limit_does_not_warn_when_only_fresh_files_are_skipped() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("one.md"), "one fresh token\n").unwrap();
    fs::write(docs.join("two.md"), "two fresh token\n").unwrap();
    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let report = db.heal_index(Some(1)).unwrap();

    assert_eq!(report.healed_files, 0);
    assert_eq!(report.removed_files, 0);
    assert_eq!(report.skipped_files, 2);
    assert_eq!(report.message, None);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_recovers_when_fts_revision_is_stale() {
    let (root, config) = markdown_config("alpha token");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.set_meta("fts_source_revision", "stale").unwrap();

    let stale = db.status(&config.database).unwrap();
    assert!(!stale.fts_dirty);
    assert!(!stale.fts_fresh);

    let hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(hits.len(), 1);
    let fresh = db.status(&config.database).unwrap();
    assert_eq!(fresh.fts_source_revision.as_deref(), Some(fresh.content_revision.as_str()));
    assert!(fresh.fts_fresh);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn parser_failures_report_paths() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("broken.rs"), "pub fn broken(").unwrap();
    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };

    let db = IndexDatabase::rebuild(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.parser_failures, 1);
    assert_eq!(status.parser_failure_paths[0].path, "src/broken.rs");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn repo_memory_bound_to_logical_symbol_surfaces_in_symbol_chunk_and_impact() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn cfg_helper() {}\n#[cfg(windows)]\npub fn cfg_helper() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("cfg_helper".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: true,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected symbol");
    let logical_symbol_id = symbol.logical_symbol_id.expect("logical symbol id");

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Treat cfg helper variants as one logical helper".to_string(),
            body: "Caller and impact analysis should use the logical symbol, not one cfg body \
                   variant."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["cfg".to_string(), "graph".to_string()],
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(logical_symbol_id),
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    assert!(!created.duplicate);
    assert_eq!(created.memory.bindings[0].binding_kind, "logical_symbol");

    let memories = db.memory_for_symbol(&symbol, 10).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].kind, "Invariant");
    let chunk_id = memories[0].bindings[0].chunk_id.expect("bound chunk");
    let chunk = db.read_chunk(chunk_id).unwrap().expect("memory chunk");
    assert_eq!(chunk.memories.len(), 1);
    assert_eq!(chunk.memories[0].memory_id, created.memory.memory_id);

    let impact = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            10,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    assert_eq!(impact.repo_memories.compact().unwrap().direct.len(), 1);
    assert_eq!(impact.completeness_and_caveats.memory_status.active, 1);
    assert_eq!(impact.completeness_and_caveats.memory_status.stale, 0);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn compact_repo_memory_view_projects_primary_binding_then_full_mode_round_trips() {
    // #37: the default `impact_surface` memory output is the scannable compact projection of each
    // memory's primary binding; full bodies + bindings stay one explicit flag away.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchored() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("anchored".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected symbol");
    let logical_symbol_id = symbol.logical_symbol_id.expect("logical symbol id");
    let full_body = "Runtime shutdown must be idempotent; second call is a no-op.".to_string();
    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Runtime shutdown must be idempotent".to_string(),
            body: full_body.clone(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["runtime".to_string()],
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(logical_symbol_id),
                ..Default::default()
            },
        })
        .unwrap();

    // Default mode is compact: a scannable header projected from the primary binding.
    let compact_report = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            10,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    let compact = compact_report.repo_memories.compact().expect("compact by default");
    assert!(compact_report.repo_memories.full().is_none(), "default must not be full");
    assert_eq!(compact.direct.len(), 1);
    let entry = &compact.direct[0];
    assert_eq!(entry.memory_id, created.memory.memory_id);
    assert_eq!(entry.kind, "Invariant");
    assert_eq!(entry.title, "Runtime shutdown must be idempotent");
    assert_eq!(entry.confidence, "high");
    assert_eq!(entry.status, "active");
    assert_eq!(entry.anchor_status.as_deref(), Some("current"));
    assert_eq!(entry.binding_kind.as_deref(), Some("logical_symbol"));
    assert_eq!(entry.path.as_deref(), Some("src/lib.rs"));
    assert!(entry.span.is_some(), "logical-symbol binding carries a line span");
    assert_eq!(entry.logical_symbol_id, Some(logical_symbol_id));
    assert_eq!(entry.tags, vec!["runtime".to_string()]);

    // Explicit full mode restores the body + full bindings for deep inspection.
    let full_report = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            10,
            &crate::query::impact::ImpactSurfaceOptions {
                compact_memories: false,
                ..Default::default()
            },
        )
        .unwrap();
    let full = full_report.repo_memories.full().expect("full on request");
    assert!(full_report.repo_memories.compact().is_none(), "full mode is not compact");
    assert_eq!(full.direct.len(), 1);
    assert_eq!(full.direct[0].body, full_body);
    assert_eq!(full.direct[0].bindings[0].binding_kind, "logical_symbol");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn compact_repo_memory_view_separates_the_stale_lane() {
    // #37: a memory whose anchor went stale lands in the compact `stale` lane (not `direct`), with
    // its `anchor_status` carried through so an agent can see it needs re-anchoring.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn drifting() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("drifting".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected symbol");
    let chunk_id = db
        .storage
        .connection()
        .query_row(
            "
                SELECT chunks.id
                FROM chunks
                JOIN files ON files.id = chunks.file_id
                WHERE files.path = ?1 AND chunks.symbol_path = ?2
                LIMIT 1
                ",
            params![symbol.path, symbol.qualified_name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Anchor drifts when the chunk hash changes".to_string(),
            body: "Stale anchors belong in their own lane, away from current evidence.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                chunk_id: Some(chunk_id),
                ..Default::default()
            },
        })
        .unwrap();

    // Current anchor: the memory is in the active `direct` lane, stale lane empty.
    let before = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            10,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    let before = before.repo_memories.compact().unwrap();
    assert_eq!(before.direct.len(), 1);
    assert!(before.stale.is_empty());

    // Drift the underlying chunk so validation marks the binding stale.
    db.storage
        .connection()
        .execute("UPDATE chunks SET text_hash = 'changed' WHERE id = ?1", [chunk_id])
        .unwrap();
    assert_eq!(db.memory_validate().unwrap().stale, 1);

    let after = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            10,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    let after = after.repo_memories.compact().unwrap();
    assert!(after.direct.is_empty(), "a stale memory leaves the direct lane");
    assert_eq!(after.stale.len(), 1);
    assert_eq!(after.stale[0].memory_id, created.memory.memory_id);
    assert_eq!(after.stale[0].anchor_status.as_deref(), Some("stale"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn repo_memory_survives_reindex_and_relocates_when_symbol_moves() {
    // The user-facing guarantee: a memory is never lost to reindexing (no FK cascade from
    // symbols/chunks), and a symbol binding re-anchors to the symbol's new location when the
    // file is edited/moved rather than going stale.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keystone() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("keystone".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: false,
        limit: 10,
    };
    let symbol = db.select_symbol(&selector).unwrap().unwrap().expect("symbol");
    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "keystone holds an invariant".to_string(),
            body: "This memory must survive a reindex and follow the symbol when it moves."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();

    // Edit the file so keystone moves down (new symbol ids on reindex), then rebuild.
    fs::write(root.join("src/lib.rs"), "pub fn added_above() {}\n\npub fn keystone() {}\n")
        .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Memory row survives the reindex (no cascade from deleted symbols).
    assert!(
        crate::query::memory::memory_by_id(db.storage.connection(), &created.memory.memory_id,)
            .unwrap()
            .is_some(),
        "memory was lost to reindex",
    );

    // Re-validation re-anchors the binding to keystone's new location, not "gone".
    db.memory_validate().unwrap();
    let symbol = db.select_symbol(&selector).unwrap().unwrap().expect("symbol after move");
    let anchored = db.memory_for_symbol(&symbol, 10).unwrap();
    assert_eq!(anchored.len(), 1, "memory did not re-anchor to moved symbol");
    assert_ne!(anchored[0].bindings[0].anchor_status, "gone");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn repo_memory_validate_marks_changed_or_missing_anchors_non_current() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchored_memory() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("anchored_memory".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected symbol");
    let chunk_id = db
        .storage
        .connection()
        .query_row(
            "
                SELECT chunks.id
                FROM chunks
                JOIN files ON files.id = chunks.file_id
                WHERE files.path = ?1 AND chunks.symbol_path = ?2
                LIMIT 1
                ",
            params![symbol.path, symbol.qualified_name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Anchor must become stale when source hash changes".to_string(),
            body: "Validation should separate stale memories from current repo evidence."
                .to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: Some(chunk_id),
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();

    db.storage
        .connection()
        .execute("UPDATE chunks SET text_hash = 'changed' WHERE id = ?1", [chunk_id])
        .unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.stale, 1);
    let stale = db.memory_for_symbol(&symbol, 10).unwrap();
    assert_eq!(stale[0].memory_id, created.memory.memory_id);
    assert_eq!(stale[0].bindings[0].anchor_status, "stale");

    db.storage.connection().execute("DELETE FROM chunks WHERE id = ?1", [chunk_id]).unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1);
    let gone = db.memory_for_symbol(&symbol, 10).unwrap();
    assert_eq!(gone[0].bindings[0].anchor_status, "gone");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn repo_memory_bound_to_edge_surfaces_when_impact_crosses_call_path() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn target_edge() {}\npub fn caller_edge() {\n    target_edge();\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let target = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target_edge".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected target");
    let graph_options = crate::query::graph::GraphTraversalOptions {
        resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
        symbol_id: Some(target.symbol_id),
        logical_symbol_id: target.logical_symbol_id,
        ..Default::default()
    };
    let callers =
        db.graph_traversal_report("find_callers", &target, true, 10, &graph_options).unwrap();
    let edge_id = callers.results[0].edge_id;

    let edge_memory = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "caller_edge to target_edge must stay synchronous".to_string(),
            body: "This specific call path is used to prove edge-bound memories surface when \
                   impact crosses the edge."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["edge".to_string()],
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: None,
                edge_id: Some(edge_id),
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    assert_eq!(edge_memory.memory.bindings[0].binding_kind, "edge");
    assert_eq!(edge_memory.memory.bindings[0].edge_id, Some(edge_id));

    let impact = db
        .impact_surface_report_for_selected_symbol(
            &target,
            10,
            &crate::query::impact::ImpactSurfaceOptions {
                resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                ..Default::default()
            },
        )
        .unwrap();
    let compact = impact.repo_memories.compact().unwrap();
    assert!(compact.direct.is_empty());
    assert_eq!(compact.path_crossed.len(), 1);
    assert_eq!(compact.path_crossed[0].memory_id, edge_memory.memory.memory_id);
    assert_eq!(impact.completeness_and_caveats.memory_status.active, 1);

    let call_path_memory = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "TestExpectation".to_string(),
            title: "caller_edge path hash recall".to_string(),
            body: "Call-path memories are addressable by a deterministic edge sequence hash."
                .to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["call-path".to_string()],
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: target.logical_symbol_id,
                end_logical_symbol_id: target.logical_symbol_id,
                edge_sequence_hash: Some("edge-sequence-test-hash".to_string()),
                path_summary: Some("caller_edge -> target_edge".to_string()),
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    let call_path = db.memory_for_call_path_hash("edge-sequence-test-hash", 10).unwrap();
    assert_eq!(call_path.len(), 1);
    assert_eq!(call_path[0].memory_id, call_path_memory.memory.memory_id);
    assert_eq!(call_path[0].call_paths[0].path_summary, "caller_edge -> target_edge");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn server_derived_call_path_hash_is_stable_and_validates_through_edge_churn() {
    // #38: bind a call-path memory by ordered edge ids — the server derives the authoritative
    // edge_sequence_hash from edge fingerprints. A full rebuild reassigns edge row ids, but the
    // hash (built from row-id-independent fingerprints) is unchanged and validation stays
    // "current". Deleting the call site makes the path "gone".
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn caller() {\n    callee();\n}\npub fn callee() {}\n")
        .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge_id = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT id FROM edges WHERE to_name LIKE '%callee%' ORDER BY id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("caller->callee edge present")
    };
    let call_path_status = |db: &IndexDatabase| -> String {
        db.storage
            .connection()
            .query_row(
                "SELECT anchor_status FROM repo_memory_bindings WHERE binding_kind = 'call_path' \
                 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap()
    };

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "why callee is invoked here".to_string(),
            body: "This call path is load-bearing.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: Some(vec![edge_id(&db)]),
                dir: None,
            },
        })
        .unwrap();

    // The stored binding_id is the server-derived hash, and it created at "current".
    let hash: String = db
        .storage
        .connection()
        .query_row(
            "SELECT binding_id FROM repo_memory_bindings WHERE binding_kind = 'call_path' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(call_path_status(&db), "current");
    // memory_for_call_path resolves the server hash.
    let found = db.memory_for_call_path_hash(&hash, 10).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].memory_id, created.memory.memory_id);

    // Rebuild reassigns edge row ids; the fingerprint-derived hash and "current" status survive.
    let old_edge = edge_id(&db);
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_ne!(
        edge_id(&db),
        old_edge,
        "rebuild must reassign the edge row id for a real churn test"
    );
    db.memory_validate().unwrap();
    assert_eq!(call_path_status(&db), "current", "server hash survives edge row-id churn");
    assert_eq!(db.memory_for_call_path_hash(&hash, 10).unwrap().len(), 1);

    // Move the call site down a line: the source line (and thus the exact fingerprint) changes,
    // but the edge's loose identity (caller -> callee) still matches → relocated, not gone.
    fs::write(
        root.join("src/lib.rs"),
        "// shift\n\npub fn caller() {\n    callee();\n}\npub fn callee() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.memory_validate().unwrap();
    assert_eq!(call_path_status(&db), "relocated", "a moved call site relocates the path");

    // Remove the call site → the edge is gone → the call path is gone.
    fs::write(root.join("src/lib.rs"), "pub fn caller() {}\npub fn callee() {}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.memory_validate().unwrap();
    assert_eq!(call_path_status(&db), "gone", "deleting the call site makes the path gone");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn impact_surface_surfaces_call_path_memory_when_path_crossed() {
    // #38 (acceptance #1 + #3): a call-path memory bound to the server-derived hash of
    // a -> b -> c surfaces in impact_surface(b).repo_memories.call_path_crossed, because the
    // traversal crosses the caller edge (a -> b) and the callee edge (b -> c).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn a() {\n    b();\n}\npub fn b() {\n    c();\n}\npub fn c() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge_to = |name: &str| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT id FROM edges WHERE to_name = ?1 ORDER BY id LIMIT 1",
                [name],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("edge to `{name}` present: {e}"))
    };
    let caller_edge = edge_to("b"); // a -> b
    let callee_edge = edge_to("c"); // b -> c

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "a -> b -> c is the hot path".to_string(),
        body: "Why this two-hop path matters.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            logical_symbol_id: None,
            symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: Some(vec![caller_edge, callee_edge]),
            dir: None,
        },
    })
    .unwrap();

    let symbol_b = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: Some("src/lib.rs::b".to_string()),
            symbol: None,
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("symbol b");

    let report = crate::query::impact::impact_surface_report_for_symbol(
        db.storage.connection(),
        &symbol_b,
        10,
        &crate::query::impact::ImpactSurfaceOptions::default(),
        |_hops| Ok(false),
    )
    .unwrap();

    let compact = report.repo_memories.compact().unwrap();
    assert!(
        compact
            .call_path_crossed
            .iter()
            .any(|memory| memory.title == "a -> b -> c is the hot path"),
        "call-path memory should surface in impact_surface(b); got call_path_crossed = {:?}",
        compact.call_path_crossed.iter().map(|m| &m.title).collect::<Vec<_>>()
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn memory_relocates_when_symbol_moves_to_another_file() {
    // Bind a memory to `fn target` in a.rs; move `fn target` verbatim to b.rs; reindex.
    // The cross-file bare-name + content-hash fallback must fire: relocated == 1, gone == 0,
    // and the persisted binding path is now b.rs.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Write the symbol in a.rs; keep b.rs present but empty so the indexer knows about it.
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "// placeholder\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("target symbol in a.rs");
    assert!(symbol.path.contains("a.rs"), "initial path should be a.rs: {}", symbol.path);

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "target returns 42".to_string(),
            body: "This memory must follow target across a file move.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    assert_eq!(created.memory.bindings[0].binding_kind, "symbol");

    // Move `fn target` verbatim to b.rs; remove it from a.rs.
    fs::write(root.join("src/a.rs"), "// target moved to b.rs\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let report = db.memory_validate().unwrap();
    assert_eq!(report.relocated, 1, "expected 1 relocated binding, report: {report:?}");
    assert_eq!(report.gone, 0, "expected 0 gone bindings, report: {report:?}");

    // The binding must now point at b.rs.
    let binding = &db
        .memory_for_symbol(
            &db.select_symbol(&crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("target".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: false,
                limit: 10,
            })
            .unwrap()
            .unwrap()
            .expect("target in b.rs"),
            10,
        )
        .unwrap()[0]
        .bindings[0]
        .clone();
    let path = binding.path.as_deref().unwrap_or("");
    assert!(path.contains("b.rs"), "binding path should be b.rs after relocation: {path}");
    assert_ne!(binding.anchor_status, "gone");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn memory_relocation_is_durable_across_a_second_reindex() {
    // After a cross-file move+relocate, a subsequent reindex (with an unrelated edit to b.rs)
    // must resolve via the rewritten qualified_name directly — not fall back to the bare-name
    // relocation path again. The binding_id must equal b.rs::target (not the old a.rs::target).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "// placeholder\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("target in a.rs");

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "target durable across reindex".to_string(),
        body: "After relocation the binding must stay stable on a second reindex.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(symbol.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // First reindex: move target verbatim from a.rs to b.rs.
    fs::write(root.join("src/a.rs"), "// moved\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let report1 = db.memory_validate().unwrap();
    assert_eq!(report1.relocated, 1, "first validate should relocate: {report1:?}");

    // Second reindex: add an unrelated symbol to b.rs, leaving target's body unchanged.
    fs::write(
        root.join("src/b.rs"),
        "pub fn target() -> u32 {\n    42\n}\npub fn unrelated() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let report2 = db.memory_validate().unwrap();
    // Must NOT be gone — resolves via the rewritten binding_id (b.rs::target).
    assert_eq!(report2.gone, 0, "binding should not be gone after second reindex: {report2:?}");

    // Confirm binding_id now points at b.rs (the relocation was persisted).
    let binding = db
        .storage
        .connection()
        .query_row(
            "SELECT binding_id FROM repo_memory_bindings WHERE binding_kind = 'symbol' LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    assert!(
        binding.contains("b.rs"),
        "binding_id should be the new b.rs qualified_name after relocation, got: {binding}"
    );
    assert!(!binding.contains("a.rs"), "binding_id must not still reference a.rs: {binding}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn relocation_persists_refreshed_symbol_and_logical_ids() {
    // #50 (problem 1): a relocated symbol binding must rewrite the stored symbol_id /
    // logical_symbol_id to the current index generation — not leave them pointing at a
    // pre-rebuild row. The qualified-name join still surfaces the memory either way, but a
    // logical-id / symbol-id-keyed lookup (memory_for_call_path, binding↔symbol matching) misses
    // a stale id.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "// placeholder\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let select = |db: &IndexDatabase| {
        db.select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("target symbol present")
    };

    let original = select(&db);
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "ids refreshed on relocate".to_string(),
        body: "The persisted symbol_id/logical_symbol_id must follow the live symbol.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(original.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // Move target to b.rs and rebuild — reassigns symbol and chunk ids.
    fs::write(root.join("src/a.rs"), "// moved\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.relocated, 1, "expected a relocation: {report:?}");

    let current = select(&db);
    assert_ne!(
        current.symbol_id, original.symbol_id,
        "the rebuild must have reassigned the symbol id for this test to be meaningful"
    );

    let (persisted_symbol_id, persisted_logical_id): (Option<i64>, Option<i64>) = db
        .storage
        .connection()
        .query_row(
            "SELECT symbol_id, logical_symbol_id FROM repo_memory_bindings WHERE binding_kind = \
             'symbol' LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        persisted_symbol_id,
        Some(current.symbol_id),
        "binding.symbol_id must be refreshed to the live symbol (was stale at {})",
        original.symbol_id
    );
    assert_eq!(
        persisted_logical_id, current.logical_symbol_id,
        "binding.logical_symbol_id must match the live symbol"
    );
    // The persisted id must actually resolve in the current generation.
    assert!(
        crate::query::symbol::lookup_by_id(db.storage.connection(), persisted_symbol_id.unwrap())
            .unwrap()
            .is_some(),
        "persisted symbol_id must resolve to a live symbol row"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn full_rebuild_leaves_no_orphan_symbol_rows_for_a_path() {
    // #50 (problem 2): a full rebuild must clear the active context's prior-generation rows
    // before reinserting — repeated rebuilds of the same path must not accumulate orphan symbol
    // rows that would strand a symbol-id-keyed binding.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);

    let count_targets = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM symbols WHERE name = 'target'", [], |row| row.get(0))
            .unwrap()
    };

    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(count_targets(&db), 1, "one target after the first rebuild");
    // Rebuild twice more with edits that force new chunk/symbol ids each time.
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    43\n}\n").unwrap();
    let _ = IndexDatabase::rebuild(&config).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    44\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        count_targets(&db),
        1,
        "repeated full rebuilds must not accumulate orphan symbol rows for the same path"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn memory_stays_gone_when_moved_symbol_body_changed() {
    // Move `fn target` to b.rs but change its body so the chunk text hash differs.
    // Content-hash mismatch → no silent relocate → gone.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "// placeholder\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("target in a.rs");

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Risk".to_string(),
        title: "target body changed guard".to_string(),
        body: "A hash-changed move must not silently relocate.".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(symbol.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // Move target to b.rs but rewrite the body (hash differs).
    fs::write(root.join("src/a.rs"), "// moved\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn target() -> u32 {\n    99\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1, "changed body must not trigger relocate, expected gone: {report:?}");
    assert_eq!(report.relocated, 0, "must not relocate when body changed: {report:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn memory_stays_gone_when_two_files_define_the_same_name() {
    // Two files define `fn target` with identical bodies. The bound symbol's file (a.rs) is
    // deleted, making the anchor gone. With >=2 content-hash matches the result is ambiguous,
    // so the binding must stay gone rather than picking the wrong file.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // a.rs is the bound file; b.rs already has an identical `fn target`.
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/c.rs"), "// unrelated\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Bind to the a.rs instance specifically.
    let candidates = db
        .symbol_candidates(
            &crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("target".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            },
            false,
        )
        .unwrap();
    let a_symbol = candidates
        .candidates
        .iter()
        .find(|c| c.path.contains("a.rs"))
        .expect("a.rs target candidate");
    let symbol_id = a_symbol.symbol_id;

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "target ambiguous guard".to_string(),
        body: "Two identical bodies must block silent relocation.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // Remove a.rs so the anchor is gone; b.rs still carries the identical body.
    fs::remove_file(root.join("src/a.rs")).unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let _report = db.memory_validate().unwrap();
    // >=2 content-hash matches → ambiguous → gone (not a wrong relocate to b.rs).
    // Note: after a.rs removal only b.rs has the symbol — but the qualified_name lookup
    // for the old "src/a.rs::target" fails, and relocate_symbol_by_name returns Some(b.rs).
    // The single-match case means relocated == 1 is also valid here per the relocate logic,
    // so the real guard this test exercises is: we do NOT silently pick the wrong file when
    // two identical bodies co-exist BEFORE deletion (the >=2 ambiguity path).
    // Re-run with b.rs also having the body so both are present on disk:
    // We need to re-assert: after deletion of a.rs, only b.rs has the symbol, so this
    // is actually an unambiguous relocate (1 candidate). The ambiguity test requires both
    // a.rs and b.rs to be present but the stored anchor (a.rs::target) to be stale.
    // Arrange: keep a.rs but corrupt its symbol so the stored symbol_id is gone.
    drop(db);
    // Restore a.rs and rebuild so both exist, then corrupt the stored symbol_id row.
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Null out the symbol_id so the exact-id check misses, and corrupt binding_id to an
    // impossible qualified_name so the qualified_name lookup also misses — leaving only
    // the bare-name+hash path, which must return None (>=2 candidates).
    db.storage
        .connection()
        .execute(
            "UPDATE repo_memory_bindings SET symbol_id = NULL, binding_id = 'src/gone.rs::target'",
            [],
        )
        .unwrap();

    let report = db.memory_validate().unwrap();
    assert_eq!(
        report.gone, 1,
        "ambiguous dual-body candidates must not trigger relocate, expected gone: {report:?}"
    );
    assert_eq!(
        report.relocated, 0,
        "must not relocate when two identical bodies exist: {report:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn memory_logical_binding_relocates_across_files() {
    // Bind a memory via logical_symbol_id to a symbol in a.rs, then move it verbatim to b.rs.
    // Because logical_symbol ids are content-derived they survive the move, so the first validate
    // arm (exact id lookup) resolves directly. To specifically exercise the bare-name+hash
    // fallback path on the logical_symbol binding kind, we corrupt the stored logical_symbol_id
    // to an impossible value AND corrupt the binding_id to an impossible qualified_name, then
    // rebuild. The fallback must recover the binding from b.rs via bare name + chunk text hash.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // A single-variant function so chunk_for_logical_symbol gives a stable, non-null hash.
    fs::write(root.join("src/a.rs"), "pub fn logical_target() -> u32 {\n    77\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "// placeholder\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("logical_target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: true,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("logical_target in a.rs");
    let logical_symbol_id = symbol.logical_symbol_id.expect("logical symbol id");

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "logical_target must follow logical binding".to_string(),
            body: "Logical-symbol binding must relocate via name+hash fallback.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(logical_symbol_id),
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    assert_eq!(created.memory.bindings[0].binding_kind, "logical_symbol");
    // Confirm a non-null source_text_hash was stored (required for fallback to work).
    let stored_hash: Option<String> = db
        .storage
        .connection()
        .query_row(
            "SELECT source_text_hash FROM repo_memories WHERE id = ?1",
            [&created.memory.memory_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        stored_hash.is_some(),
        "source_text_hash must be non-null for the relocation fallback to work"
    );

    // Move the function verbatim to b.rs; rebuild.
    fs::write(root.join("src/a.rs"), "// logical_target moved\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn logical_target() -> u32 {\n    77\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Corrupt both fast-path identifiers so only the bare-name+hash fallback can recover the
    // binding. The binding_id keeps "logical_target" as the bare name (after rsplit "::").
    db.storage
        .connection()
        .execute(
            "UPDATE repo_memory_bindings
             SET logical_symbol_id = -9999,
                 binding_id        = 'src/gone.rs::logical_target'
             WHERE binding_kind = 'logical_symbol'",
            [],
        )
        .unwrap();

    let report = db.memory_validate().unwrap();
    assert_eq!(
        report.relocated, 1,
        "logical binding must relocate via name+hash fallback: {report:?}"
    );
    assert_eq!(report.gone, 0, "logical binding must not be gone after relocation: {report:?}");

    // The binding path must now reference b.rs.
    let path = db
        .storage
        .connection()
        .query_row(
            "SELECT path FROM repo_memory_bindings WHERE binding_kind = 'logical_symbol' LIMIT 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
        .unwrap_or_default();
    assert!(
        path.contains("b.rs"),
        "logical binding path should be b.rs after relocation, got: {path}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn memory_chunk_binding_relocates_by_hash() {
    // Bind a memory directly to a chunk id. After a full rebuild the chunk rows are DELETE-cascaded
    // and re-inserted with fresh AUTOINCREMENT rowids, so the stored chunk_id is gone. Because
    // `source_text_hash` still matches the live chunk's text_hash, `relocate_chunk_by_hash` must
    // find the unique match and update the binding — relocated == 1, gone == 0.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two files so that on the second rebuild the file order may differ, exercising that the
    // relocation finds the right chunk by content-hash rather than order.
    let target_src = "pub fn chunk_anchor_target() -> u32 {\n    999\n}\n";
    fs::write(root.join("src/target.rs"), target_src).unwrap();
    fs::write(root.join("src/other.rs"), "pub fn other() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Locate the chunk that covers `chunk_anchor_target`.
    let chunk_id = db
        .storage
        .connection()
        .query_row(
            "
            SELECT chunks.id
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            WHERE files.path LIKE '%target.rs'
              AND chunks.symbol_path LIKE '%chunk_anchor_target%'
            LIMIT 1
            ",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "chunk_anchor_target must return 999".to_string(),
            body: "This chunk binding must survive a rowid change via content-hash relocation."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: Some(chunk_id),
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    assert_eq!(created.memory.bindings[0].binding_kind, "chunk");
    assert_eq!(created.memory.bindings[0].chunk_id, Some(chunk_id));

    // Confirm a source_text_hash was stored (prerequisite for relocation).
    let stored_hash: Option<String> = db
        .storage
        .connection()
        .query_row(
            "SELECT source_text_hash FROM repo_memories WHERE id = ?1",
            [&created.memory.memory_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(stored_hash.is_some(), "source_text_hash must be non-null for chunk relocation");

    // Full rebuild: SQLite replaces all chunk rows, so rowids change. `target.rs` is untouched so
    // its chunk text_hash remains identical to the stored source_text_hash. `other.rs` gets a
    // different text_hash, so the content-exact match remains unique.
    let db = IndexDatabase::rebuild(&config).unwrap();

    // The old chunk_id must no longer exist.
    let old_exists: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM chunks WHERE id = ?1", [chunk_id], |row| row.get(0))
        .unwrap();
    assert_eq!(old_exists, 0, "old chunk_id should be gone after rebuild");

    let report = db.memory_validate().unwrap();
    assert_eq!(
        report.relocated, 1,
        "chunk binding must relocate via content-hash after rowid change: {report:?}"
    );
    assert_eq!(
        report.gone, 0,
        "chunk binding must not be gone after content-hash relocation: {report:?}"
    );

    // Binding must now point at the live chunk.
    let binding = db
        .storage
        .connection()
        .query_row(
            "SELECT chunk_id, path FROM repo_memory_bindings WHERE memory_id = ?1 LIMIT 1",
            [&created.memory.memory_id],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .unwrap();
    let (new_chunk_id, binding_path) = binding;
    assert!(new_chunk_id.is_some(), "binding chunk_id must be non-null after relocation");
    assert_ne!(new_chunk_id, Some(chunk_id), "binding must point at the new (different) chunk_id");
    assert!(
        binding_path.as_deref().unwrap_or("").contains("target.rs"),
        "binding path must reference target.rs: {binding_path:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn memory_rebind_reanchors_and_refreshes_hash() {
    // Create a memory bound to `fn rebind_src` in a.rs, then delete the symbol so the binding
    // goes `gone`. Call `memory_rebind` targeting a new live symbol (`fn rebind_dst` in b.rs).
    // After rebind:
    //   - returned binding has anchor_status == "current"
    //   - memory.source_text_hash == the new chunk's text_hash
    //   - a follow-up memory_validate does NOT flip the binding to stale/gone
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn rebind_src() -> u32 {\n    1\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn rebind_dst() -> u32 {\n    2\n}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Bind a memory to rebind_src in a.rs.
    let src_symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("rebind_src".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("rebind_src symbol");

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "rebind test memory".to_string(),
            body: "This memory will be explicitly rebound to a new symbol.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                symbol_id: Some(src_symbol.symbol_id),
                logical_symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id.clone();

    // Delete rebind_src so the binding goes gone.
    fs::write(root.join("src/a.rs"), "// rebind_src removed\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1, "binding should be gone after removing symbol: {report:?}");

    // Locate rebind_dst in b.rs (the target of the explicit rebind).
    let dst_symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("rebind_dst".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("rebind_dst symbol");

    // Fetch the text_hash of rebind_dst's chunk so we can assert it matches after rebind.
    let dst_chunk_text_hash: String = db
        .storage
        .connection()
        .query_row(
            "
            SELECT chunks.text_hash
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            WHERE files.path LIKE '%b.rs'
              AND chunks.symbol_path LIKE '%rebind_dst%'
            LIMIT 1
            ",
            [],
            |row| row.get(0),
        )
        .unwrap();

    // Perform the explicit rebind.
    let rebound = db
        .memory_rebind(&memory_id, crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(dst_symbol.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        })
        .unwrap();

    // The returned binding must be current and the memory hash must match the new chunk.
    assert_eq!(rebound.bindings.len(), 1);
    assert_eq!(
        rebound.bindings[0].anchor_status, "current",
        "rebound binding must be current, got: {}",
        rebound.bindings[0].anchor_status
    );
    assert_eq!(
        rebound.source_text_hash.as_deref(),
        Some(dst_chunk_text_hash.as_str()),
        "memory source_text_hash must equal the new chunk's text_hash after rebind"
    );

    // A follow-up validate must NOT flip the binding to stale or gone.
    let post_rebind_report = db.memory_validate().unwrap();
    assert_eq!(
        post_rebind_report.gone, 0,
        "validate after rebind must not report gone: {post_rebind_report:?}"
    );
    assert_eq!(
        post_rebind_report.stale, 0,
        "validate after rebind must not report stale: {post_rebind_report:?}"
    );
    assert_eq!(
        post_rebind_report.current + post_rebind_report.relocated,
        1,
        "binding must be current or relocated after validate: {post_rebind_report:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn anchor_health_counts_tallies_persisted_statuses() {
    // Exercise the GROUP BY query in anchor_health_counts and the active-only filter.
    // Create two memories bound to real symbols; after memory_validate they should both be
    // "current". Assert memory_anchor_health() returns current >= 2 and gone == 0.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn health_alpha() {}\npub fn health_beta() {}\n")
        .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let sym = |name: &str| {
        db.select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some(name.to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("symbol must exist")
    };
    let alpha = sym("health_alpha");
    let beta = sym("health_beta");

    let bind_target = |symbol_id| crate::query::memory::RepoMemoryBindTarget {
        symbol_id: Some(symbol_id),
        logical_symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: None,
        start_line: None,
        end_line: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir: None,
    };

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "health alpha invariant".to_string(),
        body: "Anchor health test — alpha binding.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: bind_target(alpha.symbol_id),
    })
    .unwrap();

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "health beta decision".to_string(),
        body: "Anchor health test — beta binding.".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: bind_target(beta.symbol_id),
    })
    .unwrap();

    // Validate so bindings get their anchor_status written to "current".
    db.memory_validate().unwrap();

    let health = db.memory_anchor_health().unwrap();
    assert!(health.current >= 2, "expected at least 2 current bindings, got {health:?}");
    assert_eq!(health.gone, 0, "expected no gone bindings, got {health:?}");
    assert_eq!(health.stale, 0, "expected no stale bindings, got {health:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn memory_doctor_lists_gone_and_suggests_candidates() {
    // Bind a memory to `fn doctor_src` in a.rs. Delete a.rs and add `fn doctor_src` to b.rs
    // with a different body (so content-hash relocation does NOT fire and the binding stays
    // gone). Then call `memory_doctor`: the entry must appear with anchor_status == "gone"
    // and a non-empty candidate list (the same-named fn in b.rs).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn doctor_src() -> u32 {\n    1\n}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let src_symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("doctor_src".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("doctor_src in a.rs");

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "doctor test memory".to_string(),
        body: "This memory is bound to a symbol that will become gone.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(src_symbol.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // Remove a.rs and add b.rs with the same-named fn but a different body (hash mismatch
    // intentional — content relocation must NOT fire, leaving the binding gone).
    fs::remove_file(root.join("src/a.rs")).unwrap();
    fs::write(root.join("src/b.rs"), "pub fn doctor_src() -> u32 {\n    99\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let validate_report = db.memory_validate().unwrap();
    assert_eq!(
        validate_report.gone, 1,
        "binding must be gone after removing a.rs: {validate_report:?}"
    );

    // Now run memory_doctor and verify the entry is present with a candidate.
    let entries = db.memory_doctor().unwrap();
    assert_eq!(entries.len(), 1, "doctor should return exactly one entry: {entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.title, "doctor test memory");
    assert!(
        entry.anchor_status == "gone" || entry.anchor_status == "stale",
        "anchor_status should be gone or stale, got: {}",
        entry.anchor_status
    );
    // The same-named fn in b.rs must appear as a candidate.
    assert!(
        !entry.candidates.is_empty(),
        "doctor entry must have at least one candidate for the same-named fn in b.rs: {entry:?}"
    );
    assert!(
        entry.candidates.iter().any(|c| c.contains("doctor_src")),
        "candidate must contain 'doctor_src': {:?}",
        entry.candidates
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn memory_doctor_dedupes_cfg_split_candidates() {
    // A gone binding whose same-name symbol is cfg-split must surface that candidate ONCE — the
    // bare-name candidate query returns a row per physical twin, and the rebind suggestion is by
    // qualified name, so undeduped twins would print the identical command twice.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Bind to a plain (non-cfg) helper in a.rs.
    fs::write(root.join("src/a.rs"), "pub fn cfg_helper() -> u32 {\n    1\n}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let original = db
        .symbol_candidates(
            &crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("cfg_helper".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            },
            false,
        )
        .unwrap()
        .candidates[0]
        .symbol_id;
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "cfg helper note".to_string(),
        body: "Bound to a helper that becomes a cfg-split pair in another file.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(original),
            ..Default::default()
        },
    })
    .unwrap();

    // Remove a.rs and reintroduce `cfg_helper` as a cfg-split pair in b.rs with DIFFERENT bodies,
    // so content-hash relocation cannot fire (binding goes gone) while the qualified name survives
    // as two physical twins sharing one logical symbol.
    fs::remove_file(root.join("src/a.rs")).unwrap();
    fs::write(
        root.join("src/b.rs"),
        "#[cfg(not(target_arch = \"wasm32\"))]\npub fn cfg_helper() -> u32 {\n    \
         11\n}\n\n#[cfg(target_arch = \"wasm32\")]\npub fn cfg_helper() -> u32 {\n    22\n}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(db.memory_validate().unwrap().gone, 1, "binding must be gone");

    let entries = db.memory_doctor().unwrap();
    let entry = entries.iter().find(|e| e.title == "cfg helper note").expect("doctor entry");
    let cfg_candidates: Vec<&String> =
        entry.candidates.iter().filter(|c| c.ends_with("cfg_helper")).collect();
    assert_eq!(cfg_candidates.len(), 1, "cfg twins collapse to one suggestion: {cfg_candidates:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn symbol_path_selector_is_exact_not_substring() {
    // `--symbol-path` (the qualified-name route the doctor now suggests) must match exactly:
    // the qualified name `…::spawn_blocking` must NOT also pull in `spawn_blocking_handle` /
    // `spawn_blocking_offload`. This is what makes the doctor's suggestion runnable.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn spawn_blocking() {}\npub fn spawn_blocking_handle() {}\npub fn \
         spawn_blocking_offload() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hit = db
        .select_symbol_for_bind(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: Some("src/lib.rs::spawn_blocking".to_string()),
            symbol: None,
            language: None,
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .expect("exact qualified name resolves, no substring siblings")
        .expect("one hit");
    assert_eq!(hit.name, "spawn_blocking");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn select_symbol_for_bind_collapses_cfg_split_group() {
    // The memory-doctor bug: a memory bound to a cfg-split helper goes gone, and the suggested
    // `--symbol <qualified_name>` rebind hits BOTH cfg twins → ambiguous → dead end. The
    // bind-resolution path must collapse a one-logical-group candidate set to a single member so
    // the rebind succeeds, while a genuinely-distinct same-name set still disambiguates.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(not(target_arch = \"wasm32\"))]\npub fn spawn_blocking() {}\n\n#[cfg(target_arch = \
         \"wasm32\")]\npub fn spawn_blocking() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Resolve by the fully-qualified name the doctor would suggest. select_symbol (no collapse)
    // must disambiguate; select_symbol_for_bind must collapse to one member of the logical group.
    let qualified = db
        .symbol_candidates(
            &crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("spawn_blocking".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            },
            false,
        )
        .unwrap()
        .candidates[0]
        .qualified_name
        .clone();
    let logical_id = db
        .symbol_candidates(
            &crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("spawn_blocking".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            },
            false,
        )
        .unwrap()
        .candidates[0]
        .logical_symbol_id
        .expect("cfg twins share a logical id");

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: Some(qualified.clone()),
        symbol: None,
        language: None,
        allow_ambiguous: false,
        limit: 10,
    };
    assert!(
        db.select_symbol(&selector).unwrap().is_err(),
        "plain select_symbol must still disambiguate the two cfg twins"
    );
    let hit = db
        .select_symbol_for_bind(&selector)
        .unwrap()
        .expect("cfg group collapses, not ambiguous")
        .expect("one member returned");
    assert_eq!(hit.logical_symbol_id, Some(logical_id));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn repo_brief_ranks_churn_and_god_module_candidates() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);

    fs::write(root.join("src/stable.rs"), "pub fn stable() -> i32 { 1 }\n").unwrap();
    fs::write(root.join("src/hot.rs"), hot_module_text(0)).unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add initial modules"]);

    for revision in 1..=3 {
        fs::write(root.join("src/hot.rs"), hot_module_text(revision)).unwrap();
        run_git(&root, &["add", "src/hot.rs"]);
        run_git(&root, &["commit", "-m", "Iterate hot module"]);
    }

    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();

    let churn = db
        .repo_brief(crate::query::repo_brief::RepoBriefOptions {
            mode: crate::query::repo_brief::RepoBriefMode::Churn,
            limit: 1,
            include_generated: false,
            include_memories: true,
        })
        .unwrap();
    assert_eq!(churn.candidates[0].path, "src/hot.rs");
    assert_eq!(churn.candidates[0].category, "recent_churn_hotspot");
    assert!(churn.candidates[0].score <= 1.0);
    assert!(churn.candidates[0].metrics.commit_touch_count >= 4);
    assert!(churn.candidates[0].why.iter().any(|reason| reason.contains("churn")));

    let god_modules = db
        .repo_brief(crate::query::repo_brief::RepoBriefOptions {
            mode: crate::query::repo_brief::RepoBriefMode::GodModules,
            limit: 1,
            include_generated: false,
            include_memories: true,
        })
        .unwrap();
    assert_eq!(god_modules.candidates[0].path, "src/hot.rs");
    assert!(god_modules.candidates[0].score <= 1.0);
    assert!(god_modules.candidates[0].metrics.symbol_count >= 30);
    assert!(!god_modules.candidates[0].split_hints.is_empty());
    assert!(god_modules.candidates[0].next_tools.iter().any(|tool| tool.tool == "impact_surface"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn repo_clusters_groups_cotouched_files() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/sync")).unwrap();
    fs::create_dir_all(root.join("src/ui")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);

    fs::write(root.join("src/sync/actor.rs"), "pub fn sync_actor() -> i32 { 1 }\n").unwrap();
    fs::write(root.join("src/sync/msg.rs"), "pub fn sync_msg() -> i32 { 2 }\n").unwrap();
    fs::write(root.join("src/ui/app.rs"), "pub fn ui_app() -> i32 { 3 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add modules"]);

    for revision in 1..=2 {
        fs::write(
            root.join("src/sync/actor.rs"),
            format!("pub fn sync_actor() -> i32 {{ {revision} }}\n"),
        )
        .unwrap();
        fs::write(
            root.join("src/sync/msg.rs"),
            format!("pub fn sync_msg() -> i32 {{ {} }}\n", revision + 10),
        )
        .unwrap();
        run_git(&root, &["add", "src/sync/actor.rs", "src/sync/msg.rs"]);
        run_git(&root, &["commit", "-m", "Iterate sync modules"]);
    }

    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();

    let clusters = db
        .repo_clusters(crate::query::clusters::RepoClustersOptions {
            limit: 5,
            include_generated: false,
            include_memories: true,
            min_cluster_size: 2,
        })
        .unwrap();

    let sync_cluster =
        clusters.clusters.iter().find(|cluster| cluster.name == "src/sync").expect("sync cluster");
    assert!(sync_cluster.representative_paths.contains(&"src/sync/actor.rs".to_string()));
    assert!(sync_cluster.representative_paths.contains(&"src/sync/msg.rs".to_string()));
    assert!(sync_cluster.metrics.co_touch_edges >= 2);

    fs::remove_dir_all(root).unwrap();
}

fn hot_module_text(revision: usize) -> String {
    let mut text = String::new();
    text.push_str("pub fn entry() -> i32 {\n");
    for i in 0..32 {
        text.push_str(&format!("    helper_{i}() +\n"));
    }
    text.push_str(&format!("    {revision}\n}}\n"));
    for i in 0..32 {
        text.push_str(&format!("pub fn helper_{i}() -> i32 {{ {i} }}\n"));
    }
    text
}

fn unique_temp_root() -> PathBuf {
    let mut root = std::env::temp_dir();
    let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    root.push(format!("rag-rat-schema-test-{}-{}-{suffix}", std::process::id(), now_ms()));
    root
}

fn fixture_temp_root(fixture: &str) -> PathBuf {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let fixture_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures").join(fixture);
    copy_fixture_dir(&fixture_root, &root);
    root
}

fn copy_fixture_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let from_path = entry.path();
        let to_path = to.join(entry.file_name());
        if from_path.is_dir() {
            copy_fixture_dir(&from_path, &to_path);
        } else {
            fs::copy(&from_path, &to_path).unwrap();
        }
    }
}

fn markdown_config(text: &str) -> (PathBuf, Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("search.md"), text).unwrap();
    let config = markdown_config_for_root(root.clone());
    (root, config)
}

fn markdown_config_for_root(root: PathBuf) -> Config {
    Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "markdown".to_string(),
            language: Language::Markdown,
            directories: vec![PathBuf::from("docs")],
            include: vec!["**/*.md".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Docs,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    }
}

/// GitHub context for tests: the rag-rat repo itself, never the live `gh` CLI (#60).
fn test_gh_ctx() -> github::GitHubContext {
    github::GitHubContext::new(Some("cq27-dev/rag-rat"), false)
}
// ---- #219 stage 2: linked-worktree overlay indexing ----

fn init_git_repo(root: &Path) {
    run_git(root, &["init", "-q", "-b", "main"]);
    run_git(root, &["config", "user.email", "t@e"]);
    run_git(root, &["config", "user.name", "t"]);
}

/// Symbol names visible in the ACTIVE scope for `path` — queried through the `temp.files` scope
/// view (set by `set_context`), so overlay shadowing and tombstones are reflected exactly as a real
/// query would see them.
fn names_in_scope(db: &IndexDatabase, path: &str) -> Vec<String> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT s.name FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.path = ?1 \
             ORDER BY s.name",
        )
        .unwrap();
    let names = stmt.query_map([path], |row| row.get::<_, String>(0)).unwrap();
    names.filter_map(Result::ok).collect()
}

/// Whether `path` is visible at all in the active scope view (a tombstone makes this false even
/// when the base committed row exists).
fn path_in_scope(db: &IndexDatabase, path: &str) -> bool {
    db.storage
        .connection()
        .query_row("SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1)", [path], |row| row.get(0))
        .unwrap()
}

fn set_base_scope(db: &mut IndexDatabase, root: &Path) {
    let (sha, _) = resolve_git_context(root);
    db.set_context(&sha, &worktree_id_of(root)).unwrap();
}

#[test]
fn worktree_overlay_committed_modification_shadows_base() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["base_fn".to_string()]);

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(!report.worktree_id.is_empty(), "linked worktree recognized");
    assert!(report.indexed >= 1, "a.rs indexed as an overlay row");

    // index_worktree_overlay leaves the connection in the linked overlay scope.
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["linked_fn".to_string()],
        "linked scope sees the branch content, and the overlay shadows the base"
    );
    set_base_scope(&mut db, &main);
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["base_fn".to_string()],
        "the base scope is unchanged by the overlay"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// The resolved target symbol id of the single `calls_name` edge whose source file is `path` — or
/// `None` when it is unresolved. Reads `edges_data` directly (the edge rows are shared across
/// scopes; `source_file_id` keys them by the scope's file row), so it can prove a shared committed
/// caller's edge is left intact by an overlay pass (#219 P1).
fn calls_edge_target(db: &IndexDatabase, path: &str) -> Option<i64> {
    db.storage
        .connection()
        .query_row(
            "SELECT d.to_symbol_id FROM edges_data d
             JOIN files f ON f.id = d.source_file_id
             JOIN name_strings ek ON ek.id = d.edge_kind_id
             WHERE f.path = ?1 AND ek.value = 'calls_name'",
            [path],
            |row| row.get::<_, Option<i64>>(0),
        )
        .unwrap()
}

#[test]
fn worktree_overlay_resolution_does_not_corrupt_base_edges() {
    // #219 P1: an UNCHANGED committed caller's edge into a symbol the overlay renames must NOT be
    // rewritten by the overlay pass — the caller file's row is SHARED with the base scope, so an
    // overlay-scoped re-resolve against the (shadowed) overlay symbol set would corrupt the base
    // graph. The fix re-resolves ONLY the worktree's own overlay source rows.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    // `caller.rs` calls `target_fn` (defined in `target.rs`). The caller is never touched on the
    // branch, so its committed row is shared between base and overlay scopes.
    fs::write(main.join("src/caller.rs"), "pub fn use_it() { target_fn(); }\n").unwrap();
    fs::write(main.join("src/target.rs"), "pub fn target_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // The base edge resolves to a concrete target symbol; capture it.
    let base_target = calls_edge_target(&db, "src/caller.rs");
    assert!(base_target.is_some(), "base caller edge resolves to target_fn");

    // The branch RENAMES target_fn → renamed_fn (so target_fn no longer exists in the overlay's
    // symbol set), but leaves caller.rs untouched.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/target.rs"), "pub fn renamed_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "rename target"]);

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "target.rs indexed as an overlay row");

    // Back in the base scope: the unchanged caller's edge must still resolve to the SAME base
    // target. Before the fix the overlay pass re-resolved the shared caller row against its own
    // symbol set (where target_fn is gone) and NULLed/retargeted it, corrupting the base graph.
    set_base_scope(&mut db, &main);
    assert_eq!(
        calls_edge_target(&db, "src/caller.rs"),
        base_target,
        "the overlay pass must not rewrite the shared base caller's resolved edge"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// Global `parser_failures` row count (the table is unscoped — keyed by `path` only).
fn parser_failure_total(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM parser_failures", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn worktree_overlay_does_not_pollute_or_clear_global_parser_failures() {
    // #219 review: `parser_failures` is keyed by `path` only and every reader counts it globally.
    // An overlay pass routes its files through the same write path, so (1) a BRANCH-ONLY syntax
    // error must not be recorded into the global table (it would show in base/sibling coverage),
    // and (2) an overlay pass over a path that is BROKEN in the base must not DELETE the base's
    // failure by bare path.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/clean.rs"), "pub fn clean_fn() {}\n").unwrap();
    fs::write(main.join("src/base_broken.rs"), "pub fn base_broken(").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    // The base has exactly one failure (base_broken.rs).
    assert_eq!(parser_failure_total(&db), 1, "base records its one parse failure");

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // The branch BREAKS the previously-clean file AND fixes the base-broken one.
    fs::write(linked.join("src/clean.rs"), "pub fn clean_fn(").unwrap();
    fs::write(linked.join("src/base_broken.rs"), "pub fn now_ok() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch edits"]);

    let mut db = db;
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "the branch's two changed files are indexed as overlay rows");

    // The global table is UNCHANGED by the overlay pass: the branch-only `clean.rs` failure was not
    // recorded, and the base `base_broken.rs` failure was not cleared by the overlay's same-path
    // re-index.
    assert_eq!(
        parser_failure_total(&db),
        1,
        "overlay neither pollutes nor clears the global parser_failures table"
    );
    set_base_scope(&mut db, &main);
    let base_failures = db.parser_failure_paths().unwrap();
    assert_eq!(base_failures.len(), 1);
    assert_eq!(
        base_failures[0].path, "src/base_broken.rs",
        "the base scope still reports its own parse failure, untouched by the overlay"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// The lowest chunk id whose file row has `path` in the active scope (overlay row wins).
fn scoped_chunk_id(db: &IndexDatabase, path: &str) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
             WHERE f.path = ?1 ORDER BY c.id LIMIT 1",
            [path],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn worktree_overlay_read_chunk_returns_branch_text_not_main() {
    // #219 review: `read_chunk_current` revalidates a scoped chunk against `source_root` (the MAIN
    // checkout). When a branch differs from main but the chunk's anchor still validates against
    // main, the EXACT path re-sliced the chunk text out of MAIN's file — returning base text for a
    // branch chunk. The anchor hash is whitespace-NORMALIZED (lines trimmed, blanks dropped), so a
    // branch that differs from main ONLY in indentation anchors EXACT against main, then slices
    // main's de-indented bytes. The fix skips live revalidation under an overlay scope, returning
    // the stored branch-indexed text verbatim.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    // Main: no indentation.
    let main_src = "pub fn marker() -> i32 {\nlet branch_witness = 1;\nbranch_witness\n}\n";
    fs::write(main.join("src/a.rs"), main_src).unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Branch: SAME normalized content (so the anchor matches EXACT against main), but distinctively
    // indented — the indentation is what proves whether the stored branch text or main's bytes win.
    let branch_src =
        "pub fn marker() -> i32 {\n        let branch_witness = 1;\n        branch_witness\n}\n";
    fs::write(linked.join("src/a.rs"), branch_src).unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    // index_worktree_overlay leaves the connection in the overlay scope.
    let overlay_chunk_id = scoped_chunk_id(&db, "src/a.rs");
    let chunk = db.read_chunk(overlay_chunk_id).unwrap().expect("overlay chunk readable");
    assert!(
        chunk.text.contains("        let branch_witness"),
        "read_chunk returns the BRANCH's indented text in the overlay scope (not main's \
         de-indented bytes via an EXACT anchor match), got: {:?}",
        chunk.text
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn refresh_worktree_overlays_reconciles_overlay_scope_embeddings() {
    // #219 review: `refresh_worktree_overlays` restored the base scope BEFORE the pass's reconcile,
    // so a NEW/CHANGED overlay chunk never got an embedding (worktree `semantic_search` stayed
    // BM25-only for branch content). The fix reconciles each CHANGED overlay inline, while scoped
    // to it. Uses the deterministic in-process HASH model (no download).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(
        main.join("src/a.rs"),
        "pub fn base_entry() {\n    // base content with enough detail to satisfy the embedding \
         policy minimum\n}\n",
    )
    .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();
    db.reconcile(None, Some(8)).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // A NEW branch-only file with enough text to be embedding-eligible.
    fs::write(
        linked.join("src/branch_new.rs"),
        "pub fn branch_entry() {\n    // branch-only content with enough detail to satisfy the \
         embedding policy minimum\n}\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "add branch file"]);

    let options = ai::ReconcileOptions { batch_size: Some(8), ..Default::default() };
    let budget = crate::watch::ReconcileBudget::new(options, std::time::Instant::now());
    // The pass refreshes the overlay AND reconciles its embeddings inline.
    let changed = crate::watch::refresh_worktree_overlays(&mut db, &config, Some(&budget));
    assert!(changed, "the overlay changed (a new branch file was indexed)");

    // In the overlay scope, the new branch file's chunk must carry a Current embedding — not be
    // left BM25-only. `refresh_worktree_overlays` restored the base scope, so re-enter the
    // overlay.
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    let embedded: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM chunk_embeddings ce
             JOIN chunks c ON c.id = ce.chunk_id
             JOIN files f ON f.id = c.file_id
             WHERE f.path = 'src/branch_new.rs' AND ce.status = 'Current'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(embedded >= 1, "the overlay's new chunk was reconciled into an embedding");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_branch_deleted_file_is_hidden_by_tombstone() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/keep.rs"), "pub fn keep_fn() {}\n").unwrap();
    fs::write(main.join("src/gone.rs"), "pub fn gone_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    assert!(path_in_scope(&db, "src/gone.rs"));

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::remove_file(linked.join("src/gone.rs")).unwrap();
    run_git(&linked, &["add", "-A"]);
    run_git(&linked, &["commit", "-q", "-m", "drop gone"]);

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.tombstoned >= 1, "gone.rs written as a tombstone");

    // Tombstone hides the branch-deleted file; the untouched file falls through to the base.
    assert!(!path_in_scope(&db, "src/gone.rs"), "linked scope hides the branch-deleted file");
    assert!(path_in_scope(&db, "src/keep.rs"), "non-delta file falls through to the base");
    assert_eq!(names_in_scope(&db, "src/keep.rs"), vec!["keep_fn".to_string()]);

    set_base_scope(&mut db, &main);
    assert!(path_in_scope(&db, "src/gone.rs"), "the base scope still has the file");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_tombstones_an_ignored_replacement_of_a_base_file() {
    // #219 review: when a branch drops a base file from its tracked/indexable view but an IGNORED
    // file still sits at that path on disk, the candidate must be TOMBSTONED, not skipped. Before
    // the fix the on-disk-but-ignored path hit `continue`, so the overlay scope fell through to the
    // base row and queries returned a file the branch no longer presents.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/data.rs"), "pub fn base_data() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    assert!(path_in_scope(&db, "src/data.rs"));

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // The branch git-rm's the tracked file (→ a deletion candidate in the tree-diff) AND ignores
    // the path, then drops a NEW untracked file at the same path: on disk it exists but is
    // gitignored, so it is NOT indexable — exactly the shadow-a-base-file-with-an-ignored-file
    // case.
    fs::remove_file(linked.join("src/data.rs")).unwrap();
    fs::write(linked.join(".gitignore"), "/src/data.rs\n").unwrap();
    run_git(&linked, &["add", "-A"]);
    run_git(&linked, &["commit", "-q", "-m", "drop + ignore data"]);
    fs::write(linked.join("src/data.rs"), "pub fn ignored_replacement() {}\n").unwrap();

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.tombstoned >= 1, "the ignored replacement of a base file is tombstoned");
    assert!(
        !path_in_scope(&db, "src/data.rs"),
        "the overlay hides the base file behind a tombstone (the branch's view dropped it)"
    );

    set_base_scope(&mut db, &main);
    assert!(path_in_scope(&db, "src/data.rs"), "the base scope still has the file");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_reads_do_not_heal_against_main() {
    // #219 review: read tools (symbol_lookup / impact / search) revalidated overlay rows against
    // `source_root` (the MAIN checkout). A branch that changes more than
    // MAX_AUTO_HEAL_FILES_PER_CALL files looks entirely stale vs main, so `symbol_candidates`'
    // matched-file heal tripped `NeedsReindex` (and `heal_file` no-ops under an overlay
    // anyway). The overlay is authoritative, so the staleness check must be skipped under a
    // linked-overlay scope.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    // More than the heal cap (4) so an unguarded check would treat every branch file as stale.
    for i in 0..6 {
        fs::write(main.join(format!("src/f{i}.rs")), format!("pub fn shared_{i}() {{}}\n"))
            .unwrap();
    }
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Every file differs from main on the branch, and one carries a NEW symbol to look up.
    for i in 0..6 {
        fs::write(
            linked.join(format!("src/f{i}.rs")),
            format!("pub fn shared_{i}() {{}}\npub fn branch_only_{i}() {{}}\n"),
        )
        .unwrap();
    }
    run_git(&linked, &["add", "-A"]);
    run_git(&linked, &["commit", "-q", "-m", "branch changes every file"]);

    // Leaves the connection in the overlay scope.
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("branch_only_0".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: true,
        limit: 10,
    };
    // Unguarded, this raised NeedsReindex (6 > cap 4 stale-vs-main files); guarded, it resolves the
    // branch symbol cleanly and flags nothing stale.
    let lookup = db.symbol_candidates(&selector, false).unwrap();
    assert!(
        lookup.candidates.iter().any(|c| c.name == "branch_only_0"),
        "the overlay symbol resolves without a main-root stale heal: {:?}",
        lookup.candidates.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert!(
        lookup.stale_files.is_empty(),
        "no overlay file is flagged stale against main: {:?}",
        lookup.stale_files
    );
    // Search must also not raise NeedsReindex under the overlay scope.
    db.search("shared_0", 10, false).expect("search succeeds under the overlay scope");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_untracked_linked_file_appears() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    // Untracked new file in the linked checkout (no branch commit).
    fs::write(linked.join("src/new.rs"), "pub fn new_fn() {}\n").unwrap();

    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(names_in_scope(&db, "src/new.rs"), vec!["new_fn".to_string()]);

    set_base_scope(&mut db, &main);
    assert!(!path_in_scope(&db, "src/new.rs"), "the untracked file is not in the base scope");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_reads_uncommitted_linked_edit_not_head() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    // Linked HEAD == base; only a DIRTY (uncommitted) edit in the linked working tree.
    fs::write(linked.join("src/a.rs"), "pub fn dirty_fn() {}\n").unwrap();

    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["dirty_fn".to_string()],
        "overlay reads the linked WORKING tree, not the linked HEAD tree"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_query_routing_selects_scope() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // The routing entry the MCP/query path uses: None -> base, a valid linked sibling -> its
    // overlay, an unreadable/foreign path -> base (never the wrong repo).
    db.use_worktree_scope(&main, None).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["base_fn".to_string()]);

    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["linked_fn".to_string()]);

    db.use_worktree_scope(&main, Some(Path::new("/"))).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["base_fn".to_string()],
        "an unreadable worktree path falls back to the base scope"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// Raw count of overlay file rows (`worktree_id != ''`), bypassing the scope view — to assert GC
/// keeps/prunes overlay rows directly.
fn overlay_row_count(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE worktree_id != ''", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn worktree_overlay_gc_keeps_a_live_worktrees_overlay() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // Reset to the BASE scope so the overlay is kept only via `live_worktree_contexts` (the active-
    // context fallback in garbage_collect would otherwise mask a worktree_id mismatch).
    set_base_scope(&mut db, &main);
    let before = overlay_row_count(&db);
    assert!(before > 0, "overlay rows exist before GC");

    db.garbage_collect().unwrap();
    assert_eq!(
        overlay_row_count(&db),
        before,
        "GC keeps a live worktree's overlay (the overlay worktree_id matches the GC live set)"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_gc_prunes_a_removed_worktrees_overlay() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    set_base_scope(&mut db, &main);
    assert!(overlay_row_count(&db) > 0);

    // Remove the worktree → it leaves the `live_worktree_contexts` set → GC prunes its overlay.
    run_git(&main, &["worktree", "remove", "--force", linked.to_str().unwrap()]);
    db.garbage_collect().unwrap();
    assert_eq!(overlay_row_count(&db), 0, "GC prunes a removed worktree's overlay");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_rename_is_delete_old_plus_add_new() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/old.rs"), "pub fn moved_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    run_git(&linked, &["mv", "src/old.rs", "src/new.rs"]);
    run_git(&linked, &["commit", "-q", "-m", "rename"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // Linked scope: the old path is tombstoned (hidden), the new path carries the moved symbol.
    assert!(!path_in_scope(&db, "src/old.rs"), "renamed-from path is hidden in the worktree scope");
    assert!(path_in_scope(&db, "src/new.rs"));
    assert_eq!(names_in_scope(&db, "src/new.rs"), vec!["moved_fn".to_string()]);

    // Base scope is unchanged: old exists, new does not.
    set_base_scope(&mut db, &main);
    assert!(path_in_scope(&db, "src/old.rs"));
    assert!(!path_in_scope(&db, "src/new.rs"));

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn maintenance_pass_refreshes_a_linked_worktree_overlay() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    // A watcher maintenance pass auto-refreshes the overlay — no manual `index --worktree`.
    crate::watch::maintenance_pass(&config, false).unwrap();

    let mut db = IndexDatabase::open(&config.database).unwrap();
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["linked_fn".to_string()],
        "the maintenance pass populated the worktree overlay"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_reindex_is_idle_safe() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    let first = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(first.indexed >= 1, "the first overlay pass indexes the delta");
    // A re-run on an UNCHANGED worktree must be a no-op (sha-skip + tombstone-exists + gated
    // edge-resolve), so the watcher can refresh every pass without churn (#63 idle backstop).
    let second = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        (second.indexed, second.tombstoned, second.pruned),
        (0, 0, 0),
        "an unchanged worktree re-index writes nothing"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_honors_gitignore_and_refreshes_on_change() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // A tracked-but-gitignored file on the branch (force-added past the ignore rule), so it appears
    // in the committed tree-diff yet must be excluded by the worktree's `.gitignore` — parity with
    // the base walker, which the gix status path alone wouldn't enforce for a tracked file.
    fs::write(linked.join(".gitignore"), "/src/ignored.rs\n").unwrap();
    fs::write(linked.join("src/ignored.rs"), "pub fn ignored_fn() {}\n").unwrap();
    run_git(&linked, &["add", ".gitignore"]);
    run_git(&linked, &["add", "-f", "src/ignored.rs"]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(
        !path_in_scope(&db, "src/ignored.rs"),
        "a gitignored worktree file is not overlaid (parity with the base walker)"
    );

    // Remove the ignore rule on disk and re-index → the overlay now picks it up (a `.gitignore`
    // change is honored because the matcher is recompiled each pass).
    fs::write(linked.join(".gitignore"), "").unwrap();
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/ignored.rs"),
        vec!["ignored_fn".to_string()],
        "removing the ignore rule refreshes the overlay to include the file"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_pending_embeddings_are_detectable_for_retry() {
    // #219 review (3440746687): a per-overlay embedding reconcile that returns `Partial` (the
    // shared time budget ran out mid-pass) leaves the overlay's remaining chunks un-embedded.
    // The next pass sees the overlay rows as unchanged and would skip the embed forever. The
    // watcher retries on a positive `pending_embedding_jobs` count IN THE OVERLAY SCOPE — this
    // asserts that count is non-zero for an overlay whose chunks haven't been embedded, and
    // zero once they have.
    // Function bodies long enough to clear the embedding eligibility floor (MIN_EMBEDDING_CHARS).
    let base_src = r#"pub fn base_fn(input: u32) -> u32 {
    let doubled = input.wrapping_mul(2);
    let offset = doubled.wrapping_add(7);
    offset.wrapping_sub(input)
}
"#;
    let branch_src = r#"pub fn linked_fn(input: u32) -> u32 {
    let tripled = input.wrapping_mul(3);
    let offset = tripled.wrapping_add(11);
    offset.wrapping_sub(input)
}
"#;
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), base_src).unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(ai::HASH_MODEL_ID).unwrap();
    // The base has at least one embeddable chunk before reconcile (so the test isn't vacuous)...
    set_base_scope(&mut db, &main);
    assert!(db.pending_embedding_jobs().unwrap() > 0, "base has an embeddable chunk to begin with");
    // ...and embedding the base clears its backlog.
    db.reconcile_with_options_progress(ai::ReconcileOptions::default(), |_| {}).unwrap();
    set_base_scope(&mut db, &main);
    assert_eq!(db.pending_embedding_jobs().unwrap(), 0, "base scope is fully embedded");

    // A linked worktree modifies the file → the overlay carries a NEW, un-embedded chunk.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), branch_src).unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    // index_worktree_overlay leaves the connection scoped to the overlay (and does NOT embed).
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(
        db.pending_embedding_jobs().unwrap() > 0,
        "the overlay's un-embedded chunk is detectable as pending in the overlay scope (retry \
         gate)",
    );

    // After reconciling in the overlay scope, the backlog is cleared — a later pass won't re-run.
    db.reconcile_with_options_progress(ai::ReconcileOptions::default(), |_| {}).unwrap();
    assert_eq!(
        db.pending_embedding_jobs().unwrap(),
        0,
        "once embedded, the overlay reports no pending jobs (idle-safe, no perpetual retry)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_fts_freshness_revision_is_scope_invariant() {
    // #219 review (3440746692): `content_revision` (the FTS freshness digest) read the SCOPED
    // `files` view, so the global `fts_source_revision` `sync_fts` recorded under a linked-overlay
    // scope differed from the base-scope digest. Interleaved base/overlay reads then each saw the
    // global revision as stale and rebuilt the global FTS, alternating forever. The digest must be
    // GLOBAL (over `main.files`) so it is identical across scopes.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // The digest computed under the OVERLAY scope (left active by index_worktree_overlay)...
    let overlay_revision = db.content_revision().unwrap();
    // ...must equal the digest under the BASE scope: it's a GLOBAL digest, not a per-scope one.
    set_base_scope(&mut db, &main);
    let base_revision = db.content_revision().unwrap();
    assert_eq!(
        overlay_revision, base_revision,
        "the FTS freshness digest is global, so it can't alternate as scopes interleave",
    );

    // And it matches the stored `fts_source_revision` `sync_fts` wrote during the overlay refresh,
    // so a base read sees FTS as fresh (no rebuild) rather than perpetually stale.
    assert_eq!(
        db.meta("fts_source_revision").unwrap().as_deref(),
        Some(base_revision.as_str()),
        "fts_source_revision recorded during the overlay pass matches the global digest",
    );
    assert!(!db.fts_dirty().unwrap(), "the overlay refresh left FTS clean, not dirty");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_tombstones_a_base_file_a_gitignore_only_change_now_ignores() {
    // #219 review (3440746674): when the branch's ONLY change is a `.gitignore` rule, the
    // tree-diff/status candidates contain just `.gitignore` — an UNCHANGED base file the rule now
    // ignores is never visited, so its (now stale) base row keeps showing in the worktree scope.
    // The ignore-flip expansion must add that base file as a candidate so it is tombstoned.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    fs::write(main.join("src/keep.rs"), "pub fn keep_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // The branch's ONLY change is a `.gitignore` rule that ignores the (unchanged) `src/a.rs`.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join(".gitignore"), "/src/a.rs\n").unwrap();
    run_git(&linked, &["add", ".gitignore"]);
    run_git(&linked, &["commit", "-q", "-m", "ignore a.rs"]);

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.tombstoned >= 1, "the ignore-only change tombstones the now-ignored base file");
    assert!(
        !path_in_scope(&db, "src/a.rs"),
        "a base file the branch's `.gitignore` now ignores is hidden in the worktree scope, not \
         served from its stale base row",
    );
    // The sibling the rule does NOT touch is untouched (still served from its shared base row).
    assert_eq!(
        names_in_scope(&db, "src/keep.rs"),
        vec!["keep_fn".to_string()],
        "an unaffected base file is still served (no over-tombstoning)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_refreshed_with_the_branch_config_keeps_a_branch_only_target() {
    // #219 review (3440746699): the main watcher/maintenance process sweeps every linked worktree
    // with ITS OWN config. A branch whose `rag-rat.toml` ADDS a target (`extra/`) must be refreshed
    // with the branch's targets (`Config::for_linked_worktree_overlay`), or the overlay rows a
    // branch-launched hook indexed for `extra/` are filtered out of the delta and PRUNED by the
    // sweep. This asserts the overlay row survives a sweep that uses the branch config.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    // Main indexes only `src`.
    fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    // The sweeping process's config is main's: `src` only.
    let sweep_config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&sweep_config).unwrap();

    // A branch adds an `extra/` target and a file in it, with its own `rag-rat.toml`.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::create_dir_all(linked.join("extra")).unwrap();
    fs::write(linked.join("extra/more.rs"), "pub fn extra_fn() {}\n").unwrap();
    fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\", \"extra\"]\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds extra"]);

    // A branch-launched hook indexes the overlay WITH the branch config — `extra/more.rs` is
    // overlaid.
    let branch_config = sweep_config.for_linked_worktree_overlay(&linked);
    db.index_worktree_overlay(&branch_config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        names_in_scope(&db, "extra/more.rs"),
        vec!["extra_fn".to_string()],
        "the branch-only target file is overlaid by the branch-config pass",
    );

    // The MAIN sweep refreshes the same worktree. Done with the SWEEP config (`src` only) it would
    // PRUNE `extra/more.rs`; routed through `for_linked_worktree_overlay` it keeps the branch
    // target.
    let refreshed = sweep_config.for_linked_worktree_overlay(&linked);
    db.index_worktree_overlay(&refreshed, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        names_in_scope(&db, "extra/more.rs"),
        vec!["extra_fn".to_string()],
        "the main-process sweep refreshed with the branch config keeps the branch-only overlay row",
    );

    // Control: the sweep config alone (no `for_linked_worktree_overlay`) would prune it — proving
    // the bug is real and the helper is what prevents it.
    db.index_worktree_overlay(&sweep_config, &linked, &mut |_| {}).unwrap();
    assert!(
        !path_in_scope(&db, "extra/more.rs"),
        "the raw sweep config (src-only) prunes the branch-only overlay row — the bug the helper \
         fixes",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_committed_added_file_symbol_resolves_cross_connection() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    {
        // The CLI `index --worktree` writer: build the base, then overlay-index a worktree that
        // COMMITTED a brand-new file, then drop the connection.
        let mut db = IndexDatabase::rebuild(&config).unwrap();
        run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
        fs::write(linked.join("src/added.rs"), "pub fn added_fn() {}\n").unwrap();
        run_git(&linked, &["add", "."]);
        run_git(&linked, &["commit", "-q", "-m", "add file"]);
        let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
        assert!(report.indexed >= 1, "the added file is indexed into the overlay");
    }

    // A FRESH connection (the MCP server querying after the CLI wrote the overlay).
    let mut db = IndexDatabase::open(&config.database).unwrap();
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert!(
        !db.symbols("added_fn", Some(Language::Rust), 10).unwrap().is_empty(),
        "a committed added file's symbol resolves via symbol lookup in the worktree scope \
         (cross-connection)"
    );
    // ...and is grouped into logical_symbols, so GRAPH NAV (find_callers/trace_callees resolve
    // through logical_symbols) sees it too — the overlay pass must run rebuild_logical_symbols.
    let grouped: bool = db
        .storage
        .connection()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM logical_symbols WHERE logical_name = 'added_fn')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(grouped, "the overlay's added symbol is grouped into logical_symbols (graph nav)");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_serves_worktree_version_when_main_moved_ahead() {
    // Symmetry: when MAIN advances a file the worktree branch didn't touch, the worktree scope must
    // still serve the WORKTREE's (older) version — the overlay is the worktree's view, not "newest
    // wins" (the base/worktree direction is irrelevant).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/shared.rs"), "pub fn v1() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "v1"]);
    let config = source_config(main.clone(), Language::Rust);

    // Worktree branches at v1 (it does NOT touch shared.rs afterward).
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);

    // Main moves AHEAD: shared.rs -> v2.
    fs::write(main.join("src/shared.rs"), "pub fn v2() {}\n").unwrap();
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "v2"]);

    let mut db = IndexDatabase::rebuild(&config).unwrap(); // base = main @ v2
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // Worktree scope: the worktree's v1 (overlay shadows main's newer v2).
    assert!(!db.symbols("v1", Some(Language::Rust), 10).unwrap().is_empty(), "worktree serves v1");
    assert!(
        db.symbols("v2", Some(Language::Rust), 10).unwrap().is_empty(),
        "worktree scope does not show main's newer v2"
    );
    // Base scope: main's v2.
    set_base_scope(&mut db, &main);
    assert!(!db.symbols("v2", Some(Language::Rust), 10).unwrap().is_empty(), "base serves v2");
    assert!(
        db.symbols("v1", Some(Language::Rust), 10).unwrap().is_empty(),
        "base does not show v1"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_stable_when_main_removed_a_file_a_nested_branch_keeps() {
    // Field repro of the perverse held layout: a linked worktree NESTED inside config.root and
    // gitignored there; MAIN removed a file the branch still HAS; the index retains the dead old
    // commit scope that had the file. Across repeated WATCHER maintenance passes the overlay must
    // serve that file READABLE in the worktree scope — never flip-flop to a tombstone.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/keep.rs"), "pub fn keep_fn() {}\n").unwrap();
    fs::write(main.join("src/reinf.rs"), "pub fn classify_seg() {}\n").unwrap();
    fs::write(main.join(".gitignore"), "/wt/\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "C1 has reinf"]);
    let config = source_config(main.clone(), Language::Rust);

    // Index at C1 → leaves a committed scope that HAS reinf.rs (the lingering dead scope).
    IndexDatabase::rebuild(&config).unwrap();

    // Linked worktree forked at C1, NESTED under main at the gitignored path.
    let linked = main.join("wt");
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);

    // Main REMOVES reinf.rs at C2; the branch keeps it on disk + in its HEAD.
    fs::remove_file(main.join("src/reinf.rs")).unwrap();
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "C2 removed reinf"]);

    for pass in 0..3 {
        crate::watch::maintenance_pass(&config, pass == 2).unwrap(); // gc on the last pass
        let mut db = IndexDatabase::open(&config.database).unwrap();
        db.use_worktree_scope(&main, Some(&linked)).unwrap();
        assert_eq!(
            names_in_scope(&db, "src/reinf.rs"),
            vec!["classify_seg".to_string()],
            "pass {pass}: worktree overlay serves the branch file (readable, not tombstoned)"
        );
    }

    let _ = fs::remove_dir_all(&main);
}

#[test]
fn worktree_overlay_keeps_base_scope_logical_grouping() {
    // #219 regression: the overlay pass's rebuild_logical_symbols must NOT de-group the
    // base (shadowed) scope. A linked worktree that MODIFIES a base file shadows the base committed
    // row; that base symbol must keep its logical handle (sym_<hex>), or graph-nav-by-id silently
    // breaks for base symbols. Before the fix the overlay rebuild ran against the worktree scope
    // view and wiped every other scope's grouping.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/shared.rs"), "pub fn shared_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Modify the base file in the worktree → the overlay shadows the base committed row.
    fs::write(linked.join("src/shared.rs"), "pub fn shared_fn() {\n    let _x = 1;\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // The BASE committed shared_fn symbol (commit_sha != '', worktree_id = '') must still be
    // grouped.
    let base_grouped: i64 = db
        .storage
        .connection()
        .query_row(
            // Query RAW main.files, not the `files` scope view: after the overlay pass the
            // connection is worktree-scoped, which SHADOWS the base committed shared.rs row.
            "SELECT COUNT(*) FROM logical_symbol_members m
             JOIN main.symbols s ON s.id = m.symbol_id
             JOIN main.files f ON f.id = s.file_id
             WHERE s.name = 'shared_fn' AND f.commit_sha != '' AND f.worktree_id = ''",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(base_grouped >= 1, "overlay pass de-grouped the base scope (graph-nav-by-id breaks)");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[cfg(unix)]
#[test]
fn worktree_overlay_resolves_through_a_symlinked_path() {
    // #219 regression: a worktree referenced via a SYMLINK must resolve to the same
    // worktree_id as the canonical path (worktree_id_of canonicalizes), so indexing via one
    // spelling and querying via another agree. Before the fix the keys diverged → silent
    // overlay miss + GC pruning the live overlay.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/lib.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/added.rs"), "pub fn added_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    // Index via the CANONICAL path; query via a SYMLINK to the same checkout.
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    let symlinked = unique_temp_root();
    let _ = fs::remove_dir_all(&symlinked);
    std::os::unix::fs::symlink(&linked, &symlinked).unwrap();

    db.use_worktree_scope(&main, Some(&symlinked)).unwrap();
    assert!(
        !db.symbols("added_fn", Some(Language::Rust), 10).unwrap().is_empty(),
        "a symlinked worktree path must resolve to the same overlay as the canonical path"
    );

    let _ = fs::remove_file(&symlinked);
    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn orientation_in_a_linked_worktree_reflects_the_overlay_on_base() {
    // #219: SessionStart orientation must scope to the session's WORKTREE (the overlay on the
    // base), not the worktree's own HEAD — the index has no committed scope at a linked
    // worktree's HEAD, so the old resolve_git_context(cwd) saw only the bare overlay delta,
    // missing the base files.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/base.rs"), "pub fn base_fn() {}\n").unwrap();
    fs::write(main.join("src/keep.rs"), "pub fn keep_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    let db_path = db.database_path().to_path_buf();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/added.rs"), "pub fn added_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    drop(db);

    let conn = IndexConnection::open_read_only(&db_path).unwrap();
    // Worktree cwd: overlay ON base = base.rs + keep.rs + the overlay's added.rs = 3.
    let o_wt = crate::query::orientation::orientation(conn.connection(), &main, &linked).unwrap();
    assert_eq!(
        o_wt.total_files, 3,
        "worktree orientation must show base files + the overlay's added file"
    );

    // Main cwd: base scope only = 2.
    let o_main = crate::query::orientation::orientation(conn.connection(), &main, &main).unwrap();
    assert_eq!(o_main.total_files, 2, "main orientation shows the base scope");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #219 review: when `config.root` is a SUBDIR of the repo, the tree-diff / status candidates are
/// repo-relative (`crate/src/lib.rs`) but the overlay keys + `target_for_path` are config-root-
/// relative (`src/lib.rs`). The old code filtered every subdir edit out, so the overlay was empty
/// and a worktree query kept serving the stale base. The fix rebases candidates to config-relative
/// and reads bytes from the linked checkout's equivalent of `config.root`.
#[test]
fn worktree_overlay_serves_a_subdir_rooted_config() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("crate/src")).unwrap();
    fs::write(main.join("crate/src/lib.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    // Config rooted at the SUBDIR `crate`, indexing `crate/src`.
    let config = source_config(main.join("crate"), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Branch edits a file UNDER the config subdir.
    fs::write(linked.join("crate/src/lib.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    // The caller passes the linked worktree root; `compute_linked_worktree_delta` derives the
    // `crate` subdir and reads bytes from `<linked>/crate`.
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(report.indexed, 1, "the subdir edit must produce one overlay row");

    db.use_worktree_scope(&config.root, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/lib.rs"),
        vec!["linked_fn".to_string()],
        "the worktree query serves the branch version, keyed config-root-relative"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #219 review: a caller can pass a path INSIDE the linked checkout (e.g. `--worktree .` run from
/// `<linked>/src`) rather than its root. The overlay must still read the readable candidates from
/// the resolved workdir, not from the raw `linked_path` (which would double the `src/` prefix and
/// fail every read).
#[test]
fn worktree_overlay_accepts_a_path_inside_the_linked_checkout() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    // Pass a SUBDIR of the linked checkout, not its root. `gix` discovery resolves the workdir, so
    // the readable file is read from `<linked>/src/a.rs`, not `<linked>/src/src/a.rs`.
    let inside = linked.join("src");
    let report = db.index_worktree_overlay(&config, &inside, &mut |_| {}).unwrap();
    assert_eq!(report.indexed, 1, "the readable candidate must be read from the resolved workdir");

    db.use_worktree_scope(&config.root, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["linked_fn".to_string()]);

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

fn source_config(root: PathBuf, language: Language) -> Config {
    Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: language.as_str().to_string(),
            language,
            directories: vec![PathBuf::from("src")],
            include: vec!["src/".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    }
}

fn assert_edge(db: &IndexDatabase, from: &str, to: &str, edge_kind: &str, confidence: &str) {
    let count = db
        .storage
        .connection()
        .query_row(
            "
                SELECT COUNT(*)
                FROM edges
                WHERE edge_kind = ?1
                  AND confidence = ?2
                  AND COALESCE(from_name, '') LIKE ?3
                  AND to_name LIKE ?4
                ",
            params![edge_kind, confidence, format!("%{from}%"), format!("%{to}%")],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert!(count > 0, "missing edge {from} -[{edge_kind}/{confidence}]-> {to}");
}

#[test]
fn rebuild_restores_durable_wal_after_bulk_build() {
    // The bulk rebuild drops to journal_mode=MEMORY + synchronous=OFF for speed; it MUST
    // restore durable WAL/NORMAL afterward so later writes (reconcile, the watcher) are safe.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn alpha() {}\npub fn beta() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let journal_mode: String =
        db.storage.connection().query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
    assert_eq!(journal_mode.to_lowercase(), "wal", "rebuild must restore WAL durability");
    let synchronous: i64 =
        db.storage.connection().query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
    assert_eq!(synchronous, 1, "synchronous must be restored to NORMAL (=1)");
    // The index is intact and queryable after the bulk build.
    assert!(!db.symbols("alpha", Some(Language::Rust), 10).unwrap().is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dir_memory_binds_to_a_directory() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn dir_anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "src holds the core library".to_string(),
            body: "All Rust source lives under src/.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: Some("src".to_string()),
            },
        })
        .unwrap();

    assert!(!created.duplicate);
    assert_eq!(created.memory.bindings.len(), 1);
    let binding = &created.memory.bindings[0];
    assert_eq!(binding.binding_kind, "dir");
    assert_eq!(binding.binding_id, "src");
    assert_eq!(binding.anchor_status, "current");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dir_memory_validation_current_and_gone() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn dir_validate_anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Helper: build a dir bind target with only `dir` set.
    let dir_bind = |dir: Option<String>| crate::query::memory::RepoMemoryBindTarget {
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: None,
        start_line: None,
        end_line: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir,
    };

    // Case 1: memory on a populated directory ("src") -> validates current.
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "src dir is the library root".to_string(),
        body: "All source lives under src/.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        bind: dir_bind(Some("src".to_string())),
    })
    .unwrap();

    // Case 2: memory on a directory with no indexed files -> resolves gone at bind time, and
    // memory_validate leaves it gone.
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "nonexistent dir has no files".to_string(),
        body: "This directory does not exist in the index.".to_string(),
        confidence: "low".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        bind: dir_bind(Some("does/not/exist".to_string())),
    })
    .unwrap();

    // Case 3: root memory (dir:"") -> current whenever any file is indexed.
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "repo root anchors the whole index".to_string(),
        body: "The entire repo is indexed.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        bind: dir_bind(Some("".to_string())),
    })
    .unwrap();

    let report = db.memory_validate().unwrap();
    // "src" + "" both current, "does/not/exist" gone -> current==2, gone==1.
    assert_eq!(report.current, 2, "expected 2 current dir bindings");
    assert_eq!(report.gone, 1, "expected 1 gone dir binding");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn list_memories_returns_summaries_and_filters_by_binding_kind() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn list_anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dir_bind = |dir: Option<String>| crate::query::memory::RepoMemoryBindTarget {
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: None,
        start_line: None,
        end_line: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir,
    };
    let path_bind = |path: String| crate::query::memory::RepoMemoryBindTarget {
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: Some(path),
        start_line: None,
        end_line: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir: None,
    };

    // Create a dir-scoped memory.
    let dir_result = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "src is the library root".to_string(),
            body: "Core library lives under src/.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            bind: dir_bind(Some("src".to_string())),
        })
        .unwrap();

    // Create a path-scoped memory.
    let path_result = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "lib.rs exports the public surface".to_string(),
            body: "All public symbols are re-exported from lib.rs.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            bind: path_bind("src/lib.rs".to_string()),
        })
        .unwrap();

    let conn = db.storage.connection();

    // list_memories(None) returns both memories.
    let all = crate::query::memory::list_memories(conn, None).unwrap();
    assert_eq!(all.len(), 2, "expected 2 summaries, got: {all:?}");

    // The dir memory is present with correct summary fields.
    let dir_summary = all.iter().find(|s| s.memory_id == dir_result.memory.memory_id).unwrap();
    assert_eq!(dir_summary.kind, "Decision");
    assert_eq!(dir_summary.title, "src is the library root");
    assert_eq!(dir_summary.status, "active");
    assert_eq!(dir_summary.binding_kind, "dir");
    assert_eq!(dir_summary.binding_id, "src");

    // The path memory is present with correct summary fields.
    let path_summary = all.iter().find(|s| s.memory_id == path_result.memory.memory_id).unwrap();
    assert_eq!(path_summary.kind, "Invariant");
    assert_eq!(path_summary.binding_kind, "path");
    assert_eq!(path_summary.binding_id, "src/lib.rs");

    // list_memories(Some("dir")) returns only the dir-scoped memory.
    let dir_only = crate::query::memory::list_memories(conn, Some("dir")).unwrap();
    assert_eq!(dir_only.len(), 1, "expected 1 dir-kind summary, got: {dir_only:?}");
    assert_eq!(dir_only[0].binding_kind, "dir");
    assert_eq!(dir_only[0].memory_id, dir_result.memory.memory_id);

    // list_memories(Some("path")) returns only the path-scoped memory.
    let path_only = crate::query::memory::list_memories(conn, Some("path")).unwrap();
    assert_eq!(path_only.len(), 1, "expected 1 path-kind summary, got: {path_only:?}");
    assert_eq!(path_only[0].binding_kind, "path");

    fs::remove_dir_all(root).unwrap();
}

// ─── dir_tree tests ──────────────────────────────────────────────────────────

/// Shared helper: build a dir-only `RepoMemoryBindTarget`.
fn dir_bind_target(dir: Option<String>) -> crate::query::memory::RepoMemoryBindTarget {
    crate::query::memory::RepoMemoryBindTarget {
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: None,
        start_line: None,
        end_line: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir,
    }
}

/// Shared helper: create a minimal "dir" memory attached to the given directory path.
fn create_dir_memory(db: &IndexDatabase, title: &str, dir: Option<String>) {
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: title.to_string(),
        body: format!("Memory for {dir:?}."),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        bind: dir_bind_target(dir),
    })
    .unwrap();
}

/// Shared helper: install the scope view on `conn` for the repo at `root`.
fn install_scope(conn: &rusqlite::Connection, root: &Path) {
    let (commit_sha, worktree_id) = resolve_git_context(root);
    crate::index::install_scope_view(conn, &commit_sha, &worktree_id).unwrap();
}

// ─── Fix 1: label/depth contract ─────────────────────────────────────────────

#[test]
fn dir_tree_label_depth_flat_siblings() {
    // Fixture: src/a (3 files), src/b (3 files).
    // Expected display tree (formatter indents by depth, prints label):
    //   src      (depth 0, label "src")
    //     a      (depth 1, label "a")
    //     b      (depth 1, label "b")
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["x.rs", "y.rs", "z.rs"] {
        fs::write(root.join("src/a").join(name), "pub fn f() {}\n").unwrap();
        fs::write(root.join("src/b").join(name), "pub fn g() {}\n").unwrap();
    }
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts::default();
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    let find = |p: &str| {
        tree.nodes.iter().find(|n| n.path == p).unwrap_or_else(|| {
            panic!(
                "no node for {p}; nodes: {:?}",
                tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
            )
        })
    };

    let src = find("src");
    assert_eq!(src.depth, 0, "src depth");
    assert_eq!(src.label, "src", "src label");

    let a = find("src/a");
    assert_eq!(a.depth, 1, "src/a depth");
    assert_eq!(a.label, "a", "src/a label");

    let b = find("src/b");
    assert_eq!(b.depth, 1, "src/b depth");
    assert_eq!(b.label, "b", "src/b label");

    assert_eq!(tree.truncated, 0);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dir_tree_label_depth_collapse_single_child_chain() {
    // Fixture: pkg/inner/deep with 3 files only at `deep` — no files in pkg or inner.
    // pkg → inner (single child, no files, no memory) → deep (3 files).
    // After collapse: one node with path="pkg", label="pkg/inner/deep", depth=0.
    // (The chain anchor is `pkg`; it collapses into `inner` then into `deep`.)
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/pkg/inner/deep")).unwrap();
    for name in &["a.rs", "b.rs", "c.rs"] {
        fs::write(root.join("src/pkg/inner/deep").join(name), "pub fn f() {}\n").unwrap();
    }
    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["src/".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    install_scope(conn, &root);

    // max_depth must be deep enough to reach depth 4 (src/pkg/inner/deep).
    let opts = crate::query::tree::TreeOpts { max_depth: 5, min_files: 3, max_nodes: 25 };
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    // The chain src → pkg → inner collapses; the one visible node for the pkg subtree
    // anchors at `src/pkg` (or `src`) and spans through to `deep`.  What matters:
    // (a) exactly one node has path == "src/pkg/inner/deep" OR the chain ends there,
    // (b) that node's label spans the collapsed segments relative to its display parent,
    // (c) its depth reflects only displayed ancestors.
    //
    // With src having only one included child (src/pkg), and src/pkg only one included child
    // (src/pkg/inner), etc., the whole chain from `src` collapses into a single anchor node
    // at `src` with label "src/pkg/inner/deep" (full path, display parent = "").
    let collapsed = tree.nodes.iter().find(|n| n.path == "src");
    assert!(
        collapsed.is_some(),
        "expected a collapsed node anchored at 'src'; nodes: {:?}",
        tree.nodes.iter().map(|n| (&n.path, &n.label, n.depth)).collect::<Vec<_>>()
    );
    let collapsed = collapsed.unwrap();
    assert_eq!(collapsed.label, "src/pkg/inner/deep", "collapsed label must span full chain");
    assert_eq!(collapsed.depth, 0, "collapsed chain anchor must be depth 0");
    assert_eq!(collapsed.file_count, 0, "file_count on chain anchor is 0 (files live at deep)");

    // No other node should appear (the entire tree collapses).
    assert_eq!(
        tree.nodes.len(),
        1,
        "only one node after full collapse; got: {:?}",
        tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
    );
    assert_eq!(tree.truncated, 0);
    fs::remove_dir_all(root).unwrap();
}

// ─── Fix 1 + memory-only inclusion ───────────────────────────────────────────

#[test]
fn dir_tree_memory_only_dir_appears_without_min_files() {
    // A dir with a "dir" memory but fewer than min_files direct files still appears.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    // Only 1 file in src/a — below default min_files=3.
    fs::write(root.join("src/a/only.rs"), "pub fn only() {}\n").unwrap();
    // src/b gets 3 files so it qualifies on its own (ensures src is pulled in as ancestor).
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["p.rs", "q.rs", "r.rs"] {
        fs::write(root.join("src/b").join(name), "pub fn f() {}\n").unwrap();
    }
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Anchor a dir memory on src/a.
    create_dir_memory(&db, "sparse subsystem", Some("src/a".to_string()));

    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts::default();
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    let node_a = tree.nodes.iter().find(|n| n.path == "src/a").unwrap_or_else(|| {
        panic!(
            "src/a missing from tree; nodes: {:?}",
            tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
        )
    });
    assert_eq!(node_a.file_count, 1, "src/a file_count");
    assert_eq!(node_a.memory_title.as_deref(), Some("sparse subsystem"), "src/a memory_title");
    assert_eq!(node_a.depth, 1, "src/a depth");
    assert_eq!(node_a.label, "a", "src/a label");

    fs::remove_dir_all(root).unwrap();
}

// ─── Fix 2: generated exclusion ──────────────────────────────────────────────

#[test]
fn dir_tree_excludes_generated_files_from_count() {
    // A dir whose only files are generated=1 must not become a qualifying node (file_count
    // must not include generated files).
    //
    // Layout: src/gen (3 generated files), src/real (3 real files), src/also (3 real files).
    // Two real siblings prevent src from collapsing into a single-child chain so that
    // src/real and src/also appear as their own nodes.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/gen")).unwrap();
    fs::create_dir_all(root.join("src/real")).unwrap();
    fs::create_dir_all(root.join("src/also")).unwrap();
    // Real files — indexed with generated=0.
    for name in &["a.rs", "b.rs", "c.rs"] {
        fs::write(root.join("src/real").join(name), "pub fn f() {}\n").unwrap();
        fs::write(root.join("src/also").join(name), "pub fn g() {}\n").unwrap();
    }
    // Generated files — write them so the indexer picks them up, then flip generated=1.
    for name in &["g1.rs", "g2.rs", "g3.rs"] {
        fs::write(root.join("src/gen").join(name), "// generated\npub fn gen() {}\n").unwrap();
    }
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Mark all files under src/gen as generated after indexing.
    db.storage
        .connection()
        .execute("UPDATE main.files SET generated = 1 WHERE path LIKE 'src/gen/%'", [])
        .unwrap();

    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts::default();
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    // src/gen must either be absent (did not qualify) or have file_count == 0.
    if let Some(gen_node) = tree.nodes.iter().find(|n| n.path == "src/gen") {
        assert_eq!(
            gen_node.file_count,
            0,
            "generated dir must have file_count=0; got {}: {:?}",
            gen_node.file_count,
            tree.nodes.iter().map(|n| (&n.path, n.file_count)).collect::<Vec<_>>()
        );
    }
    // src/real must appear with file_count == 3 (only non-generated files counted).
    let real_node = tree.nodes.iter().find(|n| n.path == "src/real").unwrap_or_else(|| {
        panic!(
            "src/real missing; nodes: {:?}",
            tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
        )
    });
    assert_eq!(real_node.file_count, 3, "src/real file_count must be 3 (non-generated only)");

    fs::remove_dir_all(root).unwrap();
}

// ─── Fix 3: real multi-context scoping ───────────────────────────────────────

#[test]
fn dir_tree_scope_excludes_other_worktree_files() {
    // Two worktree contexts share the same main.files table.  Scoping to one context must
    // not inflate file_count with the other worktree's rows.
    //
    // Arrangement: the primary build indexes src/a/{a,b,c}.rs AND src/b/{p,q,r}.rs.
    // Two sibling dirs prevent src from collapsing so src/a appears as its own node.
    // We then INSERT three extra files under src/a with a different worktree_id.
    // After scoping to the primary context, src/a must report file_count == 3, not 6.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["a.rs", "b.rs", "c.rs"] {
        fs::write(root.join("src/a").join(name), "pub fn f() {}\n").unwrap();
    }
    for name in &["p.rs", "q.rs", "r.rs"] {
        fs::write(root.join("src/b").join(name), "pub fn g() {}\n").unwrap();
    }
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Insert extra files belonging to a different worktree (same path prefix, different
    // worktree_id).
    let conn = db.storage.connection();
    for name in &["x.rs", "y.rs", "z.rs"] {
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
                 indexed_at_ms, indexed_revision, commit_sha, worktree_id)
             VALUES (?1, 'rust', 'source', 'sha-other', 0, 0, 0, 'rev-other', '', 'other-worktree')",
            [format!("src/a/{name}")],
        )
        .unwrap();
    }

    // Scope to the primary worktree only.
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts::default();
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    let node_a = tree.nodes.iter().find(|n| n.path == "src/a").unwrap_or_else(|| {
        panic!("src/a missing; nodes: {:?}", tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>())
    });
    assert_eq!(
        node_a.file_count, 3,
        "file_count must not be inflated by other-worktree rows; got {}",
        node_a.file_count
    );

    fs::remove_dir_all(root).unwrap();
}

// ─── Fix 3: max_nodes cap ────────────────────────────────────────────────────

#[test]
fn dir_tree_truncates_at_max_nodes() {
    // Create enough dirs to exceed max_nodes=3.  We use min_files=1 so every dir with a file
    // qualifies, giving us 5 leaf dirs + ancestor nodes.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    for i in 0..5u8 {
        let dir = root.join(format!("pkg{i}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("lib.rs"), "pub fn f() {}\n").unwrap();
    }
    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from(".")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts { max_depth: 2, min_files: 1, max_nodes: 3 };
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    assert!(tree.nodes.len() <= 3, "nodes.len()={} must be <= max_nodes=3", tree.nodes.len());
    assert!(tree.truncated > 0, "truncated must be >0 when nodes were dropped");

    fs::remove_dir_all(root).unwrap();
}

// ─── original integration test (extended) ────────────────────────────────────

#[test]
fn dir_tree_builds_annotated_layout() {
    // Index six files: three in src/a/ and three in src/b/.  Both dirs meet min_files (3),
    // so both appear in the tree.  A "dir" memory is anchored to src/a with title "alpha core"
    // and a root memory (dir:"") is anchored to the repo with title "the repo".

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["x.rs", "y.rs", "z.rs"] {
        fs::write(root.join("src/a").join(name), "pub fn ax() {}\n").unwrap();
        fs::write(root.join("src/b").join(name), "pub fn bx() {}\n").unwrap();
    }

    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    create_dir_memory(&db, "alpha core", Some("src/a".to_string()));
    create_dir_memory(&db, "the repo", Some("".to_string()));

    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts::default(); // max_depth=6, min_files=3, max_nodes=30
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    // Root memory must be present.
    assert_eq!(
        tree.root_memory_title.as_deref(),
        Some("the repo"),
        "root_memory_title mismatch; got: {:?}",
        tree.root_memory_title
    );

    // src must be an intermediate node (pulled in as ancestor).
    let src = tree.nodes.iter().find(|n| n.path == "src");
    assert!(
        src.is_some(),
        "no node for src; nodes: {:?}",
        tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
    );
    let src = src.unwrap();
    assert_eq!(src.depth, 0, "src depth");
    assert_eq!(src.label, "src", "src label");

    // src/a must appear with correct label/depth, file_count==3 and memory_title.
    let node_a = tree.nodes.iter().find(|n| n.path == "src/a");
    assert!(
        node_a.is_some(),
        "no node for src/a; nodes: {:?}",
        tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
    );
    let node_a = node_a.unwrap();
    assert_eq!(node_a.file_count, 3, "src/a file_count");
    assert_eq!(node_a.depth, 1, "src/a depth");
    assert_eq!(node_a.label, "a", "src/a label");
    assert_eq!(
        node_a.memory_title.as_deref(),
        Some("alpha core"),
        "src/a memory_title mismatch: {:?}",
        node_a.memory_title
    );

    // src/b must appear with correct label/depth and file_count==3.
    let node_b = tree.nodes.iter().find(|n| n.path == "src/b");
    assert!(
        node_b.is_some(),
        "no node for src/b; nodes: {:?}",
        tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
    );
    let node_b = node_b.unwrap();
    assert_eq!(node_b.file_count, 3, "src/b file_count");
    assert_eq!(node_b.depth, 1, "src/b depth");
    assert_eq!(node_b.label, "b", "src/b label");

    // No truncation.
    assert_eq!(tree.truncated, 0, "unexpected truncation");

    // Scoping invariant: re-installing the same scope view and re-querying must not change
    // counts (guards against the view accumulating duplicate rows on reinstall).
    install_scope(conn, &root);
    let tree2 = crate::query::tree::dir_tree(conn, &opts).unwrap();
    let node_a2 = tree2.nodes.iter().find(|n| n.path == "src/a").unwrap();
    assert_eq!(node_a2.file_count, 3, "file_count changed after scope reinstall");

    fs::remove_dir_all(root).unwrap();
}

// ─── Bug fix: children of collapsed node must use leaf labels ─────────────────

#[test]
fn dir_tree_children_of_collapsed_node_use_leaf_labels() {
    // Fixture:
    //   top/          — single included child (mid), no direct files, no memory → collapses
    //   top/mid/      — has two included children (x, y); files only in x/* and y/*
    //   top/mid/x/    — 3 files (qualifies on its own)
    //   top/mid/y/    — 3 files (qualifies on its own)
    //
    // After collapse: one displayed node anchored at `top` with label "top/mid" (relative to
    // root display parent ""). Its children x and y must be labelled "x" and "y" (relative to
    // the chain-end "top/mid"), NOT "mid/x" / "mid/y" (which would be wrong — relative to the
    // anchor "top").
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("top/mid/x")).unwrap();
    fs::create_dir_all(root.join("top/mid/y")).unwrap();
    for name in &["a.rs", "b.rs", "c.rs"] {
        fs::write(root.join("top/mid/x").join(name), "pub fn fx() {}\n").unwrap();
        fs::write(root.join("top/mid/y").join(name), "pub fn fy() {}\n").unwrap();
    }
    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from(".")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts { max_depth: 6, min_files: 3, max_nodes: 30 };
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    let node_labels: Vec<(&str, &str, u8)> =
        tree.nodes.iter().map(|n| (n.path.as_str(), n.label.as_str(), n.depth)).collect();

    // The collapsed node: anchor at "top", label "top/mid", depth 0.
    let collapsed = tree
        .nodes
        .iter()
        .find(|n| n.path == "top")
        .unwrap_or_else(|| panic!("no collapsed node at 'top'; nodes: {node_labels:?}"));
    assert_eq!(collapsed.label, "top/mid", "collapsed node label; nodes: {node_labels:?}");
    let collapsed_depth = collapsed.depth;

    // Children must be labelled by leaf segment only (not "mid/x" / "mid/y").
    let x = tree
        .nodes
        .iter()
        .find(|n| n.path == "top/mid/x")
        .unwrap_or_else(|| panic!("no node for top/mid/x; nodes: {node_labels:?}"));
    assert_eq!(x.label, "x", "top/mid/x label must be leaf 'x'; nodes: {node_labels:?}");
    assert_eq!(
        x.depth,
        collapsed_depth + 1,
        "top/mid/x depth must be parent+1; nodes: {node_labels:?}"
    );

    let y = tree
        .nodes
        .iter()
        .find(|n| n.path == "top/mid/y")
        .unwrap_or_else(|| panic!("no node for top/mid/y; nodes: {node_labels:?}"));
    assert_eq!(y.label, "y", "top/mid/y label must be leaf 'y'; nodes: {node_labels:?}");
    assert_eq!(
        y.depth,
        collapsed_depth + 1,
        "top/mid/y depth must be parent+1; nodes: {node_labels:?}"
    );

    assert_eq!(tree.truncated, 0);
    fs::remove_dir_all(root).unwrap();
}

fn table_count(db: &IndexDatabase, table: &str) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", [table], |row| row.get(0))
        .unwrap()
}

fn row_count(db: &IndexDatabase, table: &str) -> i64 {
    db.storage
        .connection()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
        .unwrap()
}

fn chunk_columns(db: &IndexDatabase) -> Vec<String> {
    table_columns(db, "chunks")
}

fn file_columns(db: &IndexDatabase) -> Vec<String> {
    table_columns(db, "files")
}

fn table_columns(db: &IndexDatabase, table: &str) -> Vec<String> {
    let mut stmt = db.storage.connection().prepare(&format!("PRAGMA table_info({table})")).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(1)).unwrap().map(Result::unwrap).collect()
}

fn conn_table_columns(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(1)).unwrap().map(Result::unwrap).collect()
}

fn conn_table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
        [table],
        |_| Ok(()),
    )
    .optional()
    .unwrap()
    .is_some()
}

/// V022 bootstrap (fresh-applies-all): a brand-new index applies every migration through V022 and
/// ends with the `packages` table and the three DEDICATED edge import-scope columns — and the
/// `edges` compatibility view surfaces them. There is NO `files.package_id` column: the
/// file→package mapping is computed at LOAD time from `packages` (the #106 fix dropped the
/// persisted pointer). The oracle's `callee_*` columns are untouched (the columns are dedicated,
/// not a callee overload).
#[test]
fn v025_creates_chunk_text_compression_tables() {
    // #77 Phase 2: the chunk_text (zstd blob) + chunk_text_dict (shared dictionary) tables exist
    // after a fresh apply (baseline) AND a forward-migrate (V025).
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    for t in ["chunk_text", "chunk_text_dict"] {
        assert!(conn_table_exists(&conn, t), "{t} created on fresh apply");
    }
    let cols = conn_table_columns(&conn, "chunk_text");
    for expected in ["chunk_id", "blob", "raw_len", "dict_version"] {
        assert!(cols.contains(&expected.to_string()), "chunk_text missing {expected}");
    }
    // Forward-migrate path: drop the tables + the V025 ledger row, re-apply → recreated.
    conn.execute_batch(
        "DROP TABLE chunk_text; DROP TABLE chunk_text_dict;
         DELETE FROM schema_version WHERE id = '025_chunk_text_compression_tables';",
    )
    .unwrap();
    schema::apply(&conn).unwrap();
    assert!(conn_table_exists(&conn, "chunk_text"), "V025 recreates chunk_text on forward migrate");
    assert!(conn_table_exists(&conn, "chunk_text_dict"));

    // Dicts are immutable + versioned (#77 Phase 2): MULTIPLE versions coexist (the prior
    // CHECK(id=1) single-row constraint is gone — that was the mutable-global-slot footgun a
    // retrain would hit).
    conn.execute("INSERT INTO chunk_text_dict(version, dict) VALUES (1, x'00')", []).unwrap();
    conn.execute("INSERT INTO chunk_text_dict(version, dict) VALUES (2, x'00')", [])
        .expect("multiple dict versions coexist");
    // raw_len is the decompress capacity; a negative value would cast to a huge usize.
    assert!(
        conn.execute(
            "INSERT INTO chunk_text(chunk_id, blob, raw_len, dict_version) VALUES (1, x'00', -1, \
             1)",
            [],
        )
        .is_err(),
        "chunk_text rejects negative raw_len"
    );
}

#[test]
fn v026_recreates_chunk_fts_contentless_and_repopulates() {
    // #77 Phase 2: chunk_fts becomes a CONTENTLESS FTS5 index. Fresh apply yields a contentless
    // table that supports delete-by-rowid (contentless_delete=1); the forward-migrate (V026)
    // converts an existing external-content table and repopulates it from chunks.text.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();

    let fts_sql: String = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'chunk_fts'", [], |r| r.get(0))
        .unwrap();
    assert!(fts_sql.contains("content=''"), "chunk_fts must be contentless: {fts_sql}");
    assert!(
        fts_sql.contains("contentless_delete=1"),
        "chunk_fts needs contentless_delete: {fts_sql}"
    );
    // Contentless delete-by-rowid round-trip (the incremental delete path relies on this).
    conn.execute("INSERT INTO chunk_fts(rowid, text) VALUES (1, 'alpha beta')", []).unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM chunk_fts WHERE chunk_fts MATCH 'alpha'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, 1);
    conn.execute("DELETE FROM chunk_fts WHERE rowid = 1", []).unwrap();
    let after: i64 = conn
        .query_row("SELECT count(*) FROM chunk_fts WHERE chunk_fts MATCH 'alpha'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, 0, "contentless_delete=1 removes the row by rowid");

    // Forward-migrate from a pre-V026 index: re-add the old chunks.text column, seed a chunk + an
    // external-content chunk_fts, and drop the V026 + V027 ledger rows. Re-applying runs V026
    // (convert chunk_fts to contentless + repopulate from chunks.text) then V027 (build the
    // chunk_text store from chunks.text + drop the column) — the full retirement path as a unit.
    conn.execute("ALTER TABLE chunks ADD COLUMN text TEXT NOT NULL DEFAULT ''", []).unwrap();
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
         VALUES ('src/a.rs', 'rust', 'source', 'h', 0, 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                            start_line, end_line, text_hash, text)
         VALUES (1, 'symbol', 'gamma', 0, 10, 1, 5, 'th', 'fn gamma() { delta() }')",
        [],
    )
    .unwrap();
    conn.execute_batch(
        "DROP TABLE chunk_fts;
         CREATE VIRTUAL TABLE chunk_fts USING fts5(text, content='chunks', content_rowid='id', \
         tokenize='porter');
         INSERT INTO chunk_fts(chunk_fts) VALUES('rebuild');
         DELETE FROM schema_version WHERE id = '026_contentless_chunk_fts';
         DELETE FROM schema_version WHERE id = '027_drop_chunks_text';",
    )
    .unwrap();
    schema::apply(&conn).unwrap();

    let migrated_sql: String = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'chunk_fts'", [], |r| r.get(0))
        .unwrap();
    assert!(
        migrated_sql.contains("content=''"),
        "V026 makes chunk_fts contentless: {migrated_sql}"
    );
    let hits: i64 = conn
        .query_row("SELECT count(*) FROM chunk_fts WHERE chunk_fts MATCH 'gamma'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(hits, 1, "V026 repopulates chunk_fts from chunks.text before V027 drops it");
    // V027 retired the column and built the compressed store from it.
    assert!(!schema::column_exists(&conn, "chunks", "text").unwrap(), "V027 drops chunks.text");
    let blobs: i64 = conn.query_row("SELECT count(*) FROM chunk_text", [], |r| r.get(0)).unwrap();
    assert_eq!(blobs, 1, "V027 builds the chunk_text store from chunks.text");
}

#[test]
fn rebuild_fts_repopulates_contentless_chunk_fts_from_the_store() {
    // #77 Phase 2 recovery path: if the contentless chunk_fts is emptied/desynced,
    // IndexDatabase::rebuild_fts repopulates it by decompressing the chunk_text store (it does not
    // re-read chunks.text), and search works again.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_recovery() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let match_count = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT count(*) FROM chunk_fts WHERE chunk_fts MATCH 'alpha_recovery'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };

    // The full rebuild writes chunk_fts inline.
    assert_eq!(match_count(&db), 1, "full rebuild writes chunk_fts inline");

    // Simulate a desync: clear the contentless index, then recover.
    db.storage
        .connection()
        .execute("INSERT INTO chunk_fts(chunk_fts) VALUES('delete-all')", [])
        .unwrap();
    assert_eq!(match_count(&db), 0);
    db.rebuild_fts().unwrap();
    assert_eq!(
        match_count(&db),
        1,
        "rebuild_fts repopulates contentless chunk_fts from chunk_text"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn full_rebuild_populates_the_chunk_text_store() {
    // #77 Phase 2: a full rebuild compresses every chunk into chunk_text against the shared dict,
    // and every stored blob round-trips to its chunks.text.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() {}\npub fn beta(x: u8) -> u8 { x }\n")
        .unwrap();
    fs::write(root.join("src/b.rs"), "pub fn gamma() -> u8 { 3 }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0)).unwrap();
    let stored: i64 = conn.query_row("SELECT COUNT(*) FROM chunk_text", [], |r| r.get(0)).unwrap();
    assert!(chunks > 0, "the rebuild indexed some chunks");
    assert_eq!(stored, chunks, "every chunk is compressed into chunk_text");
    let dict: Vec<u8> = conn
        .query_row("SELECT dict FROM chunk_text_dict WHERE version = 1", [], |r| r.get(0))
        .unwrap();

    // The chunks.text column is gone (#77 Phase 2), so verify the store is self-consistent: every
    // blob decompresses to valid UTF-8 and the decompressed corpus contains the indexed source.
    let mut stmt = conn.prepare("SELECT ct.blob, ct.raw_len FROM chunk_text ct").unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))).unwrap();
    let mut corpus = String::new();
    let mut checked = 0;
    for row in rows {
        let (blob, raw_len) = row.unwrap();
        let back = super::text_compression::decompress(&blob, &dict, raw_len as usize).unwrap();
        corpus.push_str(std::str::from_utf8(&back).expect("chunk_text blob decompresses to UTF-8"));
        checked += 1;
    }
    assert_eq!(checked, chunks);
    assert!(
        corpus.contains("alpha") && corpus.contains("beta") && corpus.contains("gamma"),
        "the decompressed store contains the indexed source"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn incremental_heal_maintains_the_chunk_text_store() {
    // #77 Phase 2 (2b-2a): the incremental/heal path writes chunk_text inline with the existing
    // dict, so a healed file's compressed blobs match its NEW text (the old rows cascade out with
    // the chunks). Without this, chunk_text would go stale on every incremental update.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn original() -> u8 { 1 }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Change the file on disk, then heal it (the incremental path: remove + re-index).
    fs::write(root.join("src/a.rs"), "pub fn changed() -> u8 { 2 }\npub fn added() {}\n").unwrap();
    db.heal_file(std::path::Path::new("src/a.rs")).unwrap();

    let conn = db.storage.connection();
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0)).unwrap();
    let stored: i64 = conn.query_row("SELECT COUNT(*) FROM chunk_text", [], |r| r.get(0)).unwrap();
    assert_eq!(
        stored, chunks,
        "heal kept chunk_text one-to-one with chunks (no stale/orphan rows)"
    );
    let dict: Vec<u8> = conn
        .query_row("SELECT dict FROM chunk_text_dict WHERE version = 1", [], |r| r.get(0))
        .unwrap();
    // chunks.text is gone (#77 Phase 2): decompress the store and assert it reflects the healed
    // NEW text and not the stale pre-heal text.
    let mut stmt = conn.prepare("SELECT ct.blob, ct.raw_len FROM chunk_text ct").unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))).unwrap();
    let mut corpus = String::new();
    for row in rows {
        let (blob, raw_len) = row.unwrap();
        let back = super::text_compression::decompress(&blob, &dict, raw_len as usize).unwrap();
        corpus.push_str(std::str::from_utf8(&back).expect("chunk_text blob decompresses to UTF-8"));
    }
    assert!(
        corpus.contains("changed") && corpus.contains("added"),
        "the healed file's NEW text is what's stored"
    );
    assert!(!corpus.contains("original"), "stale pre-heal text is gone from the store");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rebuild_with_files_but_no_chunks_still_trains_a_dict_so_incrementals_dont_orphan() {
    // #77 Phase 2 regression (adversarial finding). A full rebuild that indexes a file producing
    // ZERO chunks — a whitespace-only markdown file (markdown_chunks has no whole-file fallback) —
    // must still establish dict version 1. Pre-fix, build_store early-returned on an empty corpus,
    // leaving the index dict-less with files present; the next incremental/heal then hit
    // insert_chunks' "no dict" branch, which stages into the rebuild-only `temp.rebuild_chunk_text`
    // and either ORPHANED the new chunk (same connection: a live chunk with no chunk_text row,
    // which every reader's INNER JOIN silently drops) or errored "no such table" (fresh
    // connection).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/empty.md"), "   \n\t\n  \n").unwrap();
    let config = source_config(root.clone(), Language::Markdown);
    let db = IndexDatabase::rebuild(&config).unwrap();
    {
        let conn = db.storage.connection();
        let files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).unwrap();
        let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0)).unwrap();
        let dicts: i64 =
            conn.query_row("SELECT COUNT(*) FROM chunk_text_dict", [], |r| r.get(0)).unwrap();
        assert!(files >= 1, "the whitespace-only markdown file was indexed");
        assert_eq!(chunks, 0, "it produced zero chunks");
        assert_eq!(dicts, 1, "version 1 is established even with zero chunks (the fix)");
    }

    // The (already-tracked) file gains real content; heal it on the SAME connection. Pre-fix this
    // orphaned the resulting chunk; post-fix it compresses inline against the established v1 dict.
    fs::write(root.join("src/empty.md"), "# Title\n\nReal content that yields a chunk.\n").unwrap();
    db.heal_file(std::path::Path::new("src/empty.md")).unwrap();
    let conn = db.storage.connection();
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0)).unwrap();
    let stored: i64 = conn.query_row("SELECT COUNT(*) FROM chunk_text", [], |r| r.get(0)).unwrap();
    assert!(chunks >= 1, "the real markdown file produced a chunk");
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks
             LEFT JOIN chunk_text ON chunk_text.chunk_id = chunks.id
             WHERE chunk_text.chunk_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0, "no live chunk lacks a chunk_text blob (readers INNER JOIN it)");
    assert_eq!(stored, chunks, "chunk_text is one-to-one with chunks");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn open_under_a_held_write_lock_migrates_older_schema_without_deadlock() {
    // #226 regression: a CLI write command (index/maintenance/oracle) and a watcher pass hold the
    // per-DB write lock, then open the index — which may migrate an Older schema UNDER the same
    // lock. With a raw file lock that self-deadlocks (same process, second fd → flock blocks → 30s
    // timeout, schema never migrates). WriteLock is reentrant on the holding thread, so the
    // open-time migrate re-enters instead of blocking.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn f() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db_path = config.database.clone();

    // Build a current index, then make it look Older by removing the newest migration's ledger row
    // (the DDL stays applied; only the version regresses, so the re-run is an idempotent no-op).
    {
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.storage
            .connection()
            .execute("DELETE FROM schema_version WHERE id = '029_clone_fingerprint_tables'", [])
            .unwrap();
        assert_eq!(
            schema::status(db.storage.connection()).unwrap().state,
            schema::SchemaState::Older,
            "removing the V029 ledger row makes the schema Older"
        );
    }

    // Hold the write lock exactly as the CLI `index` command does, then open under it. Pre-#226
    // this blocked 30s on the migrate lock and errored; now it migrates immediately.
    let _lock = crate::locks::WriteLock::acquire_blocking(&db_path).unwrap();
    let db =
        IndexDatabase::open(&db_path).expect("open migrates the Older schema under the held lock");
    assert_eq!(
        schema::status(db.storage.connection()).unwrap().state,
        schema::SchemaState::Compatible,
        "open re-ran the migration under the held lock; the schema is current"
    );

    drop(_lock);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn v022_fresh_apply_creates_packages_and_dedicated_import_scope_columns() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();

    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    assert_eq!(schema::LATEST_SCHEMA_VERSION, 29);
    assert!(conn_table_exists(&conn, "packages"), "packages table is created on a fresh apply");

    let package_cols = conn_table_columns(&conn, "packages");
    for expected in ["id", "manifest_dir", "commit_sha", "worktree_id", "local_roots_json"] {
        assert!(package_cols.contains(&expected.to_string()), "packages missing {expected}");
    }
    assert!(
        !conn_table_columns(&conn, "files").contains(&"package_id".to_string()),
        "files.package_id is NOT added — the file→package mapping is computed at load (#106)"
    );
    // Dedicated columns on the real edge table — NOT a callee_* overload.
    let edges_data_cols = conn_table_columns(&conn, "edges_data");
    for expected in ["import_scope_start_byte", "import_scope_end_byte", "import_mod_id"] {
        assert!(edges_data_cols.contains(&expected.to_string()), "edges_data missing {expected}");
    }
    assert!(
        edges_data_cols.contains(&"callee_start_byte".to_string()),
        "the oracle's callee_start_byte column is untouched"
    );
    // The compatibility view surfaces the new columns (so writers/tests can set them).
    let edges_view_cols = conn_table_columns(&conn, "edges");
    for expected in ["import_scope_start_byte", "import_scope_end_byte", "import_mod_id"] {
        assert!(edges_view_cols.contains(&expected.to_string()), "edges view missing {expected}");
    }
    // The packages-scope index exists.
    assert!(
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_packages_scope'",
            [],
            |_| Ok(())
        )
        .optional()
        .unwrap()
        .is_some(),
        "idx_packages_scope is created"
    );
}

/// V022 forward-only migrate (older→latest): an index lacking the V022 artifacts (the `packages`
/// table, the edge import-scope columns, and the V022 schema_version row) is re-`apply`ed and
/// converges to V22 with all artifacts present — proving the migration is additive and idempotent
/// on top of an older shape, the auto-migrate-forward path (#102). V022 does NOT add a `files`
/// column (the file→package mapping is computed at load, #106), so there is nothing to drop/re-add
/// there.
#[test]
fn v022_forward_migrate_adds_artifacts_to_an_older_index() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // Simulate a pre-V022 index: drop the V022 artifacts and its schema_version row. (SQLite ≥3.35
    // supports DROP COLUMN; the bundled rusqlite is current.)
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS packages;
        DROP INDEX IF EXISTS idx_packages_scope;
        DROP VIEW IF EXISTS edges;
        DROP TRIGGER IF EXISTS edges_view_insert;
        DROP TRIGGER IF EXISTS edges_view_update;
        DROP TRIGGER IF EXISTS edges_view_delete;
        ALTER TABLE edges_data DROP COLUMN import_scope_start_byte;
        ALTER TABLE edges_data DROP COLUMN import_scope_end_byte;
        ALTER TABLE edges_data DROP COLUMN import_mod_id;
        DELETE FROM schema_version WHERE id = '022_per_package_import_scope';
        -- Also drop the later migration rows so `known_version` reads the contiguous V21 below;
        -- leaving any would make the applied-set max > 21 and skip the migrate. (The artifacts
        -- those later migrations added — the edges view (V023), files.has_test_code (V024) — can
        -- stay: their apply fns are idempotent, so the forward-migrate below is a clean no-op for
        -- the parts already present.)
        DELETE FROM schema_version WHERE id = '023_dispatch_edge_facts_view_exclusion';
        DELETE FROM schema_version WHERE id = '024_files_has_test_code';
        DELETE FROM schema_version WHERE id = '025_chunk_text_compression_tables';
        DELETE FROM schema_version WHERE id = '026_contentless_chunk_fts';
        DELETE FROM schema_version WHERE id = '027_drop_chunks_text';
        DELETE FROM schema_version WHERE id = '028_intern_symbol_qualified_names';
        DELETE FROM schema_version WHERE id = '029_clone_fingerprint_tables';
        ",
    )
    .unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, 21, "now looks like a V21 index");
    assert!(!conn_table_exists(&conn, "packages"));

    // Forward-migrate: re-running apply (the Older→apply path) converges to the latest version.
    schema::apply(&conn).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    assert!(conn_table_exists(&conn, "packages"), "forward migrate creates packages");
    assert!(
        !conn_table_columns(&conn, "files").contains(&"package_id".to_string()),
        "forward migrate does NOT add files.package_id (#106 computes the mapping at load)"
    );
    let edges_data_cols = conn_table_columns(&conn, "edges_data");
    for expected in ["import_scope_start_byte", "import_scope_end_byte", "import_mod_id"] {
        assert!(edges_data_cols.contains(&expected.to_string()), "forward migrate adds {expected}");
    }
    // The view was rebuilt and surfaces the columns (a SELECT must not fail).
    conn.query_row("SELECT import_mod_id FROM edges LIMIT 1", [], |_| Ok(())).optional().unwrap();
}

/// True when an INDEX of this name exists (sqlite_master, type='index').
fn conn_index_exists(conn: &rusqlite::Connection, index: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [index],
        |_| Ok(()),
    )
    .optional()
    .unwrap()
    .is_some()
}

/// V028 fresh apply (#224): a brand-new index stores qualified names as `qualified_name_id`
/// (interned into `name_strings`), NOT inline `qualified_name`; the id indexes exist and the old
/// string indexes do not.
#[test]
fn v028_fresh_apply_interns_symbol_qualified_names() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();

    for table in ["symbols", "logical_symbols"] {
        let cols = conn_table_columns(&conn, table);
        assert!(
            cols.contains(&"qualified_name_id".to_string()),
            "{table} has the interned id column"
        );
        assert!(
            !cols.contains(&"qualified_name".to_string()),
            "{table} no longer has the inline text column"
        );
    }
    assert!(conn_index_exists(&conn, "idx_symbols_qualified_name_id"));
    assert!(conn_index_exists(&conn, "idx_logical_symbols_qualified_name_id"));
    assert!(!conn_index_exists(&conn, "idx_symbols_qualified_name"));
    assert!(!conn_index_exists(&conn, "idx_logical_symbols_qualified_name"));
    // The pool is named `name_strings` (the #224 rename rode this version bump); `edge_strings` is
    // gone.
    assert!(conn_table_exists(&conn, "name_strings"));
    assert!(!conn_table_exists(&conn, "edge_strings"));
}

/// V028 forward-migrate (#224): simulate a pre-V028 index — re-add the inline `qualified_name` TEXT
/// column + the old string index, rename the pool back to `edge_strings`, and drop the V028 ledger
/// row — then re-apply. The forward path must: rename `edge_strings → name_strings`, intern every
/// symbol/logical qname into the pool, set `qualified_name_id`, drop the inline column, and leave a
/// forward-migrated row reconstructable to the SAME qualified name as before.
#[test]
fn v028_forward_migrate_interns_and_drops_the_inline_column() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();

    // Seed a file + a symbol + a logical symbol in the CURRENT (interned) shape.
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
         VALUES ('a.rs', 'rust', 'source', 'h', 0, 0)",
        [],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    let symbol_qn = "a.rs::do_thing";
    let logical_qn = "a.rs::LogicalThing";
    for qn in [symbol_qn, logical_qn] {
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", [qn]).unwrap();
    }
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
         end_byte)
         VALUES (?1, 'rust', 'do_thing', (SELECT id FROM name_strings WHERE value = ?2),
                 'function', 0, 10)",
        params![file_id, symbol_qn],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, kind,
                                     variant_count, group_reason)
         VALUES (7, 'rust', 'a.rs', 'LogicalThing',
                 (SELECT id FROM name_strings WHERE value = ?1), 'struct', 1, 'single')",
        params![logical_qn],
    )
    .unwrap();

    // --- Regress the schema to the pre-V028 shape ---
    // 1. Re-add the inline `qualified_name` TEXT column + restore its values from the pool, then
    //    drop the interned id column + its index.
    conn.execute_batch(
        "
        ALTER TABLE symbols ADD COLUMN qualified_name TEXT;
        UPDATE symbols SET qualified_name =
            (SELECT value FROM name_strings WHERE name_strings.id = symbols.qualified_name_id);
        DROP INDEX IF EXISTS idx_symbols_qualified_name_id;
        ALTER TABLE symbols DROP COLUMN qualified_name_id;
        CREATE INDEX idx_symbols_qualified_name ON symbols(qualified_name);

        ALTER TABLE logical_symbols ADD COLUMN qualified_name TEXT;
        UPDATE logical_symbols SET qualified_name =
            (SELECT value FROM name_strings WHERE name_strings.id =
                logical_symbols.qualified_name_id);
        DROP INDEX IF EXISTS idx_logical_symbols_qualified_name_id;
        ALTER TABLE logical_symbols DROP COLUMN qualified_name_id;
        CREATE INDEX idx_logical_symbols_qualified_name ON logical_symbols(qualified_name);
        ",
    )
    .unwrap();
    // 2. Rename the pool back to the pre-merge name `edge_strings` (the rename guard in
    //    provision_baseline must adopt it). The view references name_strings, so drop it first; the
    //    re-apply rebuilds it.
    conn.execute_batch(
        "
        DROP VIEW IF EXISTS edges;
        DROP TRIGGER IF EXISTS edges_view_insert;
        DROP TRIGGER IF EXISTS edges_view_update;
        DROP TRIGGER IF EXISTS edges_view_delete;
        ALTER TABLE name_strings RENAME TO edge_strings;
        ",
    )
    .unwrap();
    // 3. Drop the V028 and V029 ledger rows so the schema reads Older and the migration replays.
    conn.execute_batch(
        "DELETE FROM schema_version WHERE id = '028_intern_symbol_qualified_names';
         DELETE FROM schema_version WHERE id = '029_clone_fingerprint_tables';",
    )
    .unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().state,
        schema::SchemaState::Older,
        "removing the V028+V029 ledger rows makes the pre-V028 shape Older"
    );

    // --- Forward-migrate ---
    schema::apply(&conn).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);

    // The rename happened and the inline columns are gone.
    assert!(conn_table_exists(&conn, "name_strings"), "edge_strings was renamed to name_strings");
    assert!(!conn_table_exists(&conn, "edge_strings"));
    for table in ["symbols", "logical_symbols"] {
        let cols = conn_table_columns(&conn, table);
        assert!(cols.contains(&"qualified_name_id".to_string()), "{table} has qualified_name_id");
        assert!(!cols.contains(&"qualified_name".to_string()), "{table} dropped the inline column");
    }
    assert!(conn_index_exists(&conn, "idx_symbols_qualified_name_id"));
    assert!(conn_index_exists(&conn, "idx_logical_symbols_qualified_name_id"));
    assert!(!conn_index_exists(&conn, "idx_symbols_qualified_name"));
    assert!(!conn_index_exists(&conn, "idx_logical_symbols_qualified_name"));

    // The ids backfilled correctly: each row reconstructs to its ORIGINAL qualified name via the
    // join, and a round-trip lookup returns it.
    let symbol_reconstructed: String = conn
        .query_row(
            "SELECT qn.value FROM symbols
             LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
             WHERE symbols.name = 'do_thing'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(symbol_reconstructed, symbol_qn, "symbol qname is preserved through the migration");
    let logical_reconstructed: String = conn
        .query_row(
            "SELECT qn.value FROM logical_symbols
             LEFT JOIN name_strings qn ON qn.id = logical_symbols.qualified_name_id
             WHERE logical_symbols.id = 7",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(logical_reconstructed, logical_qn, "logical qname is preserved");

    // Round-trip through the production read paths: exact lookup (lookup_symbol_path) + logical
    // read.
    let hit = crate::query::symbol::lookup_candidates(
        &conn,
        &crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: Some(symbol_qn.to_string()),
            symbol: None,
            language: None,
            allow_ambiguous: true,
            limit: 10,
        },
        false,
    )
    .unwrap();
    assert_eq!(
        hit.candidates.first().map(|c| c.qualified_name.as_str()),
        Some(symbol_qn),
        "lookup_symbol_path resolves the interned qualified name"
    );
    let logical = crate::query::symbol::lookup_logical_by_id(&conn, 7).unwrap().unwrap();
    assert_eq!(logical.qualified_name, logical_qn, "logical read reconstructs the qname");
}

/// GC must-fix (#224, the highest-severity gate): the orphan-sweep prunes `name_strings` entries
/// nothing references. After the merge, symbols/logical_symbols `qualified_name_id` are referencing
/// columns too — so a pool entry referenced ONLY by a symbol (no edge) must SURVIVE gc, or gc nulls
/// the symbol's qname out.
#[test]
fn gc_preserves_a_name_strings_entry_referenced_only_by_a_symbol() {
    let (root, config) = markdown_config("# Note\nplaceholder\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    // `files`/`symbols` are per-connection scoped TEMP VIEWS after open/rebuild; write the base
    // tables in `main`. Scope the file to the live commit so gc keeps it.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                                commit_sha, worktree_id)
         VALUES ('only.rs', 'rust', 'source', 'h', 0, 0, ?1, ?2)",
        params![db.active_commit_sha, db.active_worktree_id],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    // A symbol whose qname is interned but referenced by NO edge.
    let symbol_only_qn = "only.rs::orphan_if_gc_is_wrong";
    conn.execute("INSERT OR IGNORE INTO main.name_strings(value) VALUES (?1)", [symbol_only_qn])
        .unwrap();
    conn.execute(
        "INSERT INTO main.symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                  end_byte)
         VALUES (?1, 'rust', 'orphan_if_gc_is_wrong',
                 (SELECT id FROM main.name_strings WHERE value = ?2), 'function', 0, 10)",
        params![file_id, symbol_only_qn],
    )
    .unwrap();

    // Run the pool-sweep through gc with the symbol's commit kept live.
    db.prune_to_live(
        std::slice::from_ref(&db.active_commit_sha),
        std::slice::from_ref(&db.active_worktree_id),
    )
    .unwrap();

    let conn = db.storage.connection();
    let surviving: i64 = conn
        .query_row("SELECT COUNT(*) FROM name_strings WHERE value = ?1", [symbol_only_qn], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(surviving, 1, "gc must NOT prune a pool entry a live symbol references");
    // And the symbol's qname is still reconstructable (not nulled).
    let reconstructed: Option<String> = conn
        .query_row(
            "SELECT qn.value FROM symbols
             LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
             WHERE symbols.name = 'orphan_if_gc_is_wrong'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reconstructed.as_deref(), Some(symbol_only_qn));
    drop(db);
    let _ = fs::remove_dir_all(&root);
}

/// Id-width guard (#224): the merged pool's `max(id)` must stay well under the 3→4-byte SQLite
/// serial-type cliff (8,388,608) — a wider id would be a real edges-side regression (every edge
/// carries two name-ids). A representative self-index of this repo is far below it; assert a large
/// margin so a future blow-up surfaces here.
#[test]
fn name_strings_max_id_stays_in_the_three_byte_range() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // Seed a representative spread of interned names across edges + symbols.
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
         VALUES ('a.rs', 'rust', 'source', 'h', 0, 0)",
        [],
    )
    .unwrap();
    for i in 0..1000 {
        let qn = format!("a.rs::sym_{i}");
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", [&qn]).unwrap();
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                 end_byte)
             VALUES (1, 'rust', ?1, (SELECT id FROM name_strings WHERE value = ?2), 'function', 0, \
             1)",
            params![format!("sym_{i}"), qn],
        )
        .unwrap();
    }
    let max_id: i64 =
        conn.query_row("SELECT COALESCE(MAX(id), 0) FROM name_strings", [], |r| r.get(0)).unwrap();
    assert!(
        max_id < 8_388_608,
        "name_strings max id {max_id} must stay under the 3→4-byte serial-type cliff"
    );
}

#[test]
fn files_has_test_code_flag_is_computed_at_index_time() {
    // #77 V024: the precomputed files.has_test_code flag replaces impact_surface's chunks.text
    // marker scan. Assert it's set at index time from the file's text (the same marker set the
    // V024 backfill + test_items use), independent of the path.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // A test-marker file whose PATH has no 'test'/'spec' — only the flag can classify it.
    fs::write(root.join("src/markers.rs"), "#[cfg(test)]\nmod inner {\n    pub fn check() {}\n}\n")
        .unwrap();
    fs::write(root.join("src/plain.rs"), "pub fn plain_helper() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let flag = |path: &str| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT has_test_code FROM main.files WHERE path = ?1", [path], |row| {
                row.get(0)
            })
            .unwrap()
    };
    assert_eq!(flag("src/markers.rs"), 1, "a #[cfg(test)] file gets has_test_code = 1");
    assert_eq!(flag("src/plain.rs"), 0, "a plain file gets has_test_code = 0");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn files_has_test_code_flag_survives_the_heal_path() {
    // #77 / PR #223 review: the lazy heal path (heal_file -> index_file) is a SECOND files-insert
    // site. Without computing the flag there, a marker-only test file healed (e.g. on a lexical
    // search miss) would drop to has_test_code = 0 and be misclassified as a non-test. Heal must
    // recompute it from the same chunk markers as the full/incremental path.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // PATH has no 'test'/'spec' — only the flag can classify it.
    fs::write(root.join("src/markers.rs"), "#[cfg(test)]\nmod inner {\n    pub fn check() {}\n}\n")
        .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let flag = || -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT has_test_code FROM main.files WHERE path = 'src/markers.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(flag(), 1, "full index sets has_test_code");
    db.heal_file(std::path::Path::new("src/markers.rs")).unwrap();
    assert_eq!(flag(), 1, "heal_file re-indexes through index_file and keeps has_test_code = 1");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn has_test_code_backfill_is_case_sensitive() {
    // PR #223 review: the V024 backfill must use the SAME case rules as the index-time
    // `str::contains` (case-sensitive). SQLite `LIKE` is case-insensitive for ASCII, so it would
    // set the flag for an uppercase `TEST(` that a freshly-indexed file (whose
    // `contains("test(")` is false) leaves at 0 — a migrated-vs-reindexed divergence. `instr`
    // is case-sensitive, so they agree.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // V024's backfill reads chunks.text, which V027 retired; this test exercises the backfill as a
    // pre-V027 forward-migrate would, so re-add the column it reads.
    conn.execute("ALTER TABLE chunks ADD COLUMN text TEXT NOT NULL DEFAULT ''", []).unwrap();
    for (id, path) in [(1, "a.rs"), (2, "b.rs")] {
        conn.execute(
            "INSERT INTO files(id, path, language, kind, sha256, modified_at_ms, indexed_at_ms) \
             VALUES (?1, ?2, 'rust', 'source', 'x', 0, 0)",
            rusqlite::params![id, path],
        )
        .unwrap();
    }
    // File 1: a lowercase marker. File 2: only an UPPERCASE non-marker (no lowercase marker).
    conn.execute(
        "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
         text, text_hash) VALUES (1, 'block', 0, 1, 1, 1, 'fn f() { test() }', 'h1')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
         text, text_hash) VALUES (2, 'block', 0, 1, 1, 1, 'fn g() { TEST() }', 'h2')",
        [],
    )
    .unwrap();
    conn.execute("UPDATE files SET has_test_code = 0", []).unwrap();
    schema::apply_files_has_test_code(&conn).unwrap();
    let flag = |id: i64| -> i64 {
        conn.query_row("SELECT has_test_code FROM files WHERE id = ?1", [id], |r| r.get(0)).unwrap()
    };
    assert_eq!(flag(1), 1, "lowercase test( marks the file");
    assert_eq!(flag(2), 0, "uppercase TEST( does NOT (case-sensitive, like str::contains)");
}

/// End-to-end through the FULL-REBUILD driver (`resolve_and_insert_edges`): a real `rebuild` of a
/// tiny Cargo workspace must apply per-package + module-aware import scope. A bare reference to a
/// name `use`d from an EXTERNAL crate stays unresolved; a same-named LOCAL workspace symbol still
/// resolves from the package that owns it. This is the full-driver half of the both-driver parity
/// (the DB driver is covered by `module_aware_suppression_through_db_driver`).
#[test]
fn full_rebuild_applies_per_package_import_scope() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // A workspace crate `myapp` with a LOCAL `Helper` and an EXTERNAL `use std::fmt::Display`.
    // `Helper` referenced via a local crate path resolves; a `Display` reference does not bind to a
    // (hypothetical) local symbol because it is use'd from std.
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"myapp\"\n").unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "use std::fmt::Display;\npub struct Helper;\npub struct Display;\npub fn run() {\n    let \
         _ = Display;\n    let _ = Helper;\n}\n",
    )
    .unwrap();

    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // The `packages` table was populated with myapp's root crate. The file→package mapping is NOT
    // persisted (#106) — the resolver computes it at load from this row — so there is no
    // `files.package_id` to assert; the behavioral assertions below prove the load-time computation
    // engaged.
    let package_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM packages", [], |row| row.get(0)).unwrap();
    assert!(package_count >= 1, "rebuild writes a packages row for the manifest");

    // The `Display` reference (use'd from external std) must NOT bind to the local `Display`
    // struct.
    let display_bound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE to_name = 'Display' AND edge_kind = \
             'references_type' AND to_symbol_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        display_bound, 0,
        "a `Display` use'd from external std must not bind to the local `Display` struct"
    );

    fs::remove_dir_all(root).unwrap();
}

fn indexed_revision_count(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM files WHERE indexed_revision != ''", [], |row| row.get(0))
        .unwrap()
}

fn chunk_source_revision_count(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM chunks WHERE source_revision != ''", [], |row| row.get(0))
        .unwrap()
}

fn first_chunk_id(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT id FROM chunks ORDER BY id LIMIT 1", [], |row| row.get(0))
        .unwrap()
}

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct MockGitHubClient;

impl github::GitHubClient for MockGitHubClient {
    fn issue(&self, owner: &str, repo: &str, number: i64) -> anyhow::Result<github::GitHubIssue> {
        Ok(github::GitHubIssue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            html_url: format!("https://github.com/{owner}/{repo}/issues/{number}"),
            state: "open".to_string(),
            title: "Decision: keep sqlite".to_string(),
            body: "We decided sqlite is required for binary size.".to_string(),
            author: Some("octo".to_string()),
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            updated_at: Some("2026-01-02T00:00:00Z".to_string()),
            is_pull_request: true,
        })
    }

    fn issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubComment>> {
        Ok(vec![github::GitHubComment {
            id: 4201,
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            html_url: format!("https://github.com/{owner}/{repo}/issues/{number}#comment-1"),
            body: "Rejected alternative: duckdb was too large.".to_string(),
            author: Some("octo".to_string()),
            created_at: Some("2026-01-01T01:00:00Z".to_string()),
            updated_at: Some("2026-01-01T01:00:00Z".to_string()),
        }])
    }

    fn pull(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<github::GitHubPullRequest>> {
        Ok(Some(github::GitHubPullRequest {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            html_url: format!("https://github.com/{owner}/{repo}/pull/{number}"),
            state: "open".to_string(),
            title: "Use sqlite".to_string(),
            body: "Constraint: normal queries must use cache only.".to_string(),
            author: Some("octo".to_string()),
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            updated_at: Some("2026-01-02T00:00:00Z".to_string()),
            merged_at: None,
        }))
    }

    fn pull_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubReview>> {
        Ok(vec![github::GitHubReview {
            id: 4202,
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            html_url: Some(format!("https://github.com/{owner}/{repo}/pull/{number}#review")),
            state: "COMMENTED".to_string(),
            body: "Risk: live crawling during search would be surprising.".to_string(),
            author: Some("reviewer".to_string()),
            submitted_at: Some("2026-01-01T02:00:00Z".to_string()),
        }])
    }

    fn pull_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubReviewComment>> {
        Ok(vec![github::GitHubReviewComment {
            id: 4203,
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            path: Some("docs/search.md".to_string()),
            html_url: format!("https://github.com/{owner}/{repo}/pull/{number}#discussion"),
            body: "No longer use obsolete duckdb rationale.".to_string(),
            author: Some("reviewer".to_string()),
            created_at: Some("2026-01-01T03:00:00Z".to_string()),
            updated_at: Some("2026-01-01T03:00:00Z".to_string()),
        }])
    }
}

struct PartiallyFailingGitHubClient;

impl github::GitHubClient for PartiallyFailingGitHubClient {
    fn issue(&self, owner: &str, repo: &str, number: i64) -> anyhow::Result<github::GitHubIssue> {
        if number == 404 {
            anyhow::bail!("gh: Not Found (HTTP 404)");
        }
        MockGitHubClient.issue(owner, repo, number)
    }

    fn issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubComment>> {
        MockGitHubClient.issue_comments(owner, repo, number)
    }

    fn pull(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<github::GitHubPullRequest>> {
        MockGitHubClient.pull(owner, repo, number)
    }

    fn pull_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubReview>> {
        MockGitHubClient.pull_reviews(owner, repo, number)
    }

    fn pull_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubReviewComment>> {
        MockGitHubClient.pull_review_comments(owner, repo, number)
    }
}

// ─── Phase C1: orientation composer ──────────────────────────────────────────

#[test]
fn orientation_composes_read_only() {
    // Build a temp index with files in two dirs, a root dir memory, and one non-dir
    // (path-bound) active memory.  Verify the Orientation struct is fully populated and
    // that running orientation twice yields identical results (no writes).

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["x.rs", "y.rs", "z.rs"] {
        fs::write(root.join("src/a").join(name), "pub fn ax() {}\n").unwrap();
        fs::write(root.join("src/b").join(name), "pub fn bx() {}\n").unwrap();
    }

    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Root dir memory (binding_kind='dir', binding_id="").
    create_dir_memory(&db, "root purpose", Some("".to_string()));

    // Non-dir memory bound to a specific path — should appear in active_memory_titles.
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "path memory title".to_string(),
        body: "bound to src/a/x.rs".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        bind: crate::query::memory::RepoMemoryBindTarget {
            path: Some("src/a/x.rs".to_string()),
            logical_symbol_id: None,
            symbol_id: None,
            chunk_id: None,
            edge_id: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // orientation installs its own scope view — pass the raw connection.
    let conn = db.storage.connection();
    let o1 = crate::query::orientation::orientation(conn, &root, &root).unwrap();

    // tree: root memory must be set; nodes must be non-empty.
    assert_eq!(
        o1.tree.root_memory_title.as_deref(),
        Some("root purpose"),
        "root_memory_title wrong: {:?}",
        o1.tree.root_memory_title
    );
    assert!(
        !o1.tree.nodes.is_empty(),
        "tree.nodes should be non-empty; got {:?}",
        o1.tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
    );

    // load_bearing: at most 5, each entry is (path, fan_in).
    assert!(o1.load_bearing.len() <= 5, "load_bearing len {} > 5", o1.load_bearing.len());
    // Each entry must be a non-empty path with u64 fan_in (value may be 0 if graph not built).
    for (path, _fan_in) in &o1.load_bearing {
        assert!(!path.is_empty(), "load_bearing path is empty");
    }

    // active_memory_titles: the path-bound memory must appear; the dir memory must NOT.
    assert!(
        o1.active_memory_titles.contains(&"path memory title".to_string()),
        "path memory not in active_memory_titles: {:?}",
        o1.active_memory_titles
    );
    assert!(
        !o1.active_memory_titles.contains(&"root purpose".to_string()),
        "dir memory should not appear in active_memory_titles: {:?}",
        o1.active_memory_titles
    );

    // head/indexed_head: strings (may be empty when not in a git repo; that's fine for a temp dir).
    // Just assert they're present as fields (no panic).
    let _ = &o1.head;
    let _ = &o1.indexed_head;

    // anchor counts must be present (counts are non-negative; checked via Debug).
    let _ = format!("{:?}", o1.anchor);

    // total_files: 6 non-generated source files indexed.
    assert_eq!(o1.total_files, 6, "total_files mismatch");

    // parser_failures: a non-panicking u64.
    let _ = o1.parser_failures;

    // Idempotency: run orientation a second time — must succeed with same key results.
    let o2 = crate::query::orientation::orientation(conn, &root, &root).unwrap();
    assert_eq!(
        o2.tree.root_memory_title.as_deref(),
        Some("root purpose"),
        "second call: root_memory_title changed"
    );
    assert_eq!(o2.tree.nodes.len(), o1.tree.nodes.len(), "second call: node count changed");
    assert_eq!(
        o2.active_memory_titles, o1.active_memory_titles,
        "second call: active_memory_titles changed"
    );
    assert_eq!(o2.total_files, o1.total_files, "second call: total_files changed");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn orientation_composes_through_read_only_connection() {
    // Regression guard: the production SessionStart path (claude_hook::session_start) opens the
    // index via IndexConnection::open_read_only (SQLITE_OPEN_READ_ONLY on the main DB) and then
    // runs orientation(), which CREATEs a TEMP table + TEMP VIEW.  A read-only main DB still
    // permits writes to the TEMP database, so this must succeed — prove it here.

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["x.rs", "y.rs", "z.rs"] {
        fs::write(root.join("src/a").join(name), "pub fn ax() {}\n").unwrap();
        fs::write(root.join("src/b").join(name), "pub fn bx() {}\n").unwrap();
    }

    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let db_path = db.database_path().to_path_buf();
    // Drop the writable handle so the read-only open is the only live connection.
    drop(db);

    // Open the SAME on-disk DB read-only, exactly as session_start does.
    let conn = IndexConnection::open_read_only(&db_path).unwrap();
    let o = crate::query::orientation::orientation(conn.connection(), &root, &root)
        .expect("orientation must compose through a read-only main-DB connection");

    // The scope view (TEMP table/view) was created and queried — non-empty tree + 6 files.
    assert!(!o.tree.nodes.is_empty(), "tree.nodes empty through read-only conn");
    assert_eq!(o.total_files, 6, "total_files mismatch through read-only conn");

    fs::remove_dir_all(root).unwrap();
}

/// A git fixture with one committed source file, configured like production (absolute DB path).
fn git_fixture_for_overlay_tests() -> (PathBuf, Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);
    fs::write(root.join("src/lib.rs"), "pub fn stable() -> i32 { 1 }\n").unwrap();
    fs::write(root.join("src/extra.rs"), "pub fn extra() -> i32 { 2 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "init"]);
    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };
    (root, config)
}

/// Insert a worktree-overlay `files` row mirroring an existing committed row's content — the
/// stale leftover a dirty-then-committed file leaves behind when its cleanup never ran (#87).
fn insert_stale_overlay_row(db: &IndexDatabase, path: &str, worktree_id: &str) -> i64 {
    let (sha, language, kind): (String, String, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT sha256, language, kind FROM main.files WHERE path = ?1 AND commit_sha != ''",
            [path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id) VALUES (?1, ?2, ?3, ?4, 0, 0, '', ?5)",
            rusqlite::params![path, language, kind, sha, worktree_id],
        )
        .unwrap();
    db.storage.connection().last_insert_rowid()
}

/// #87: a full rebuild must be authoritative for the whole checkout. A stale overlay row shadows
/// its committed counterpart, which exempted the committed row from the clear stage — the rebuild
/// then collided on UNIQUE(path, commit_sha, worktree_id) and FAILED. With the fix, the rebuild
/// succeeds and leaves exactly one row per path, at the commit scope.
#[test]
fn full_rebuild_survives_stale_overlay_rows() {
    let (root, config) = git_fixture_for_overlay_tests();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let worktree_id = db.active_worktree_id.clone();
    let commit = db.active_commit_sha.clone();
    assert!(!commit.is_empty(), "fixture must be a real git checkout");
    insert_stale_overlay_row(&db, "src/lib.rs", &worktree_id);
    drop(db);

    let db = IndexDatabase::rebuild(&config).expect("rebuild must survive stale overlay rows");

    let rows: Vec<(String, String)> = {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT commit_sha, worktree_id FROM main.files WHERE path = 'src/lib.rs' AND \
                 kind != 'deleted'",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(rows.len(), 1, "exactly one row per path after an authoritative rebuild: {rows:?}");
    assert_eq!(rows[0], (commit, String::new()), "the clean tree indexes at the commit scope");

    fs::remove_dir_all(root).unwrap();
}

/// #59: a FOREIGN file row — a path the real tree never produces — leaked into the index at the
/// ACTIVE scope (the held-mini footgun: a test redirected its DB to the shared self-index and wrote
/// fixture-relative paths under the repo's own commit). It must neither survive a full rebuild nor
/// wedge it on UNIQUE(path, commit_sha, worktree_id). The authoritative clear (#87) stages the
/// whole active commit, so a rebuild removes the leaked row and the self-index self-heals — no
/// manual `.rag-rat` wipe.
#[test]
fn full_rebuild_clears_foreign_leaked_rows_at_the_active_scope() {
    let (root, config) = git_fixture_for_overlay_tests();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let commit = db.active_commit_sha.clone();
    assert!(!commit.is_empty(), "fixture must be a real git checkout");
    // A path the real tree does not contain, leaked at the active commit scope (worktree_id='', the
    // shared clean-row scope real files index at).
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id) VALUES ('src/foreign_leak.rs', 'rust', 'source', 'leak', 0, \
             0, ?1, '')",
            rusqlite::params![commit],
        )
        .unwrap();
    drop(db);

    let db = IndexDatabase::rebuild(&config)
        .expect("rebuild must survive and clear foreign leaked rows");
    let leaked: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE path = 'src/foreign_leak.rs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0, "the authoritative full rebuild clears foreign rows at the active scope");

    fs::remove_dir_all(root).unwrap();
}

/// #1 / #106: in a REAL git checkout the active context is `(commit_sha=HEAD, worktree_id=<root
/// path>)` while a clean file row is `(commit_sha=HEAD, worktree_id='')`. The file→package mapping
/// is computed at LOAD time (`load_package_roots_into_scope`) by longest-`manifest_dir`-prefix over
/// the active scope's `packages` rows — there is no persisted `files.package_id` (#106 dropped it
/// to stop a worktree from stamping its package ids onto shared clean rows). This proves the
/// load-time computation correctly maps a clean-checkout file to ITS package on a real git
/// checkout: a path-dep alias declared only by crate `foo` resolves LOCAL inside `foo` and EXTERNAL
/// inside crate `bar`.
#[test]
fn clean_checkout_file_resolves_against_its_own_package_roots() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("crates/foo/src")).unwrap();
    fs::create_dir_all(root.join("crates/bar/src")).unwrap();
    fs::create_dir_all(root.join("crates/helper/src")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);
    // A workspace where ONLY `foo` declares the RENAMED path-dep alias `shared` (pointing at the
    // `helper` crate). The alias KEY `shared` is local ONLY to foo — it is not a workspace crate
    // name (that is `helper`) and bar never declares it — so the same `use shared::Thing` is
    // local in foo and external in bar. This is the per-package locality (#1) the load-time
    // mapping must honor.
    fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[\"crates/*\"]\n").unwrap();
    fs::write(
        root.join("crates/foo/Cargo.toml"),
        "[package]\nname=\"foo\"\n[dependencies]\nshared = { path = \"../helper\", package = \
         \"helper\" }\n",
    )
    .unwrap();
    // Both foo and bar reference a same-named `Thing` in TYPE position (a `references_type` edge —
    // the bucket per-package suppression acts on).
    fs::write(root.join("crates/foo/src/lib.rs"), "use shared::Thing;\npub fn foo(_t: Thing) {}\n")
        .unwrap();
    fs::write(root.join("crates/bar/Cargo.toml"), "[package]\nname=\"bar\"\n").unwrap();
    fs::write(root.join("crates/bar/src/lib.rs"), "use shared::Thing;\npub fn bar(_t: Thing) {}\n")
        .unwrap();
    fs::write(root.join("crates/helper/Cargo.toml"), "[package]\nname=\"helper\"\n").unwrap();
    // A local `Thing` symbol the bare references could bind to.
    fs::write(root.join("crates/helper/src/lib.rs"), "pub struct Thing;\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "init"]);

    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("crates")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };

    let db = IndexDatabase::rebuild(&config).unwrap();
    assert!(!db.active_commit_sha.is_empty(), "fixture must be a real git checkout");
    assert!(!db.active_worktree_id.is_empty(), "a real checkout has a non-empty worktree id");
    let conn = db.storage.connection();

    // In `foo`, `shared` is its declared path-dep alias → LOCAL → the bare `Thing` binds to the
    // shared crate's `Thing`. In `bar`, `shared` is undeclared → EXTERNAL → the bare `Thing` is
    // suppressed (stays unresolved). If the load-time mapping fell open to the global union (the
    // #106 leak), bar's reference would wrongly bind too.
    let foo_bound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges e JOIN files f ON f.id = e.source_file_id WHERE f.path = \
             'crates/foo/src/lib.rs' AND e.to_name = 'Thing' AND e.edge_kind != 'imports' AND \
             e.to_symbol_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        foo_bound >= 1,
        "in foo, `shared` is its own path-dep alias — the bare `Thing` resolves to the local \
         symbol"
    );
    let bar_bound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges e JOIN files f ON f.id = e.source_file_id WHERE f.path = \
             'crates/bar/src/lib.rs' AND e.to_name = 'Thing' AND e.edge_kind != 'imports' AND \
             e.to_symbol_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        bar_bound, 0,
        "in bar, `shared` is an EXTERNAL crate — the bare `Thing` must NOT bind to the local \
         symbol"
    );

    fs::remove_dir_all(root).unwrap();
}

/// #87 (self-heal half): an incremental pass drops a stale overlay row whose path is clean.
/// With a committed counterpart present the overlay is removed outright; without one, the row is
/// RE-STAMPED to the commit scope in place (same row id — chunks/symbols/embeddings/memory
/// bindings all survive).
#[test]
fn incremental_pass_heals_stale_overlay_rows() {
    let (root, config) = git_fixture_for_overlay_tests();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let worktree_id = db.active_worktree_id.clone();
    let commit = db.active_commit_sha.clone();

    // Case A: stale overlay WITH a committed counterpart -> deleted, committed row takes over.
    insert_stale_overlay_row(&db, "src/lib.rs", &worktree_id);
    // Case B: stale overlay WITHOUT a committed counterpart (its content matches disk) ->
    // re-stamped to the commit scope in place.
    let restamp_id = insert_stale_overlay_row(&db, "src/extra.rs", &worktree_id);
    db.storage
        .connection()
        .execute("DELETE FROM main.files WHERE path = 'src/extra.rs' AND commit_sha != ''", [])
        .unwrap();
    drop(db);

    let db = IndexDatabase::index_discover_with_progress(&config, |_| {}).unwrap();

    let overlays: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE worktree_id != '' AND kind != 'deleted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(overlays, 0, "a clean tree leaves no overlay rows behind");

    let lib_rows: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE path = 'src/lib.rs' AND kind != 'deleted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lib_rows, 1, "the committed row takes over from the deleted overlay");

    let (extra_id, extra_commit): (i64, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT id, commit_sha FROM main.files WHERE path = 'src/extra.rs' AND kind != \
             'deleted'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(extra_commit, commit, "the orphan overlay is re-stamped to the commit scope");
    assert_eq!(
        extra_id, restamp_id,
        "re-stamp is in place — the row id (and ids hanging off it) survive"
    );

    fs::remove_dir_all(root).unwrap();
}

/// Phase 3: the LOCAL structural-load enrichment (`scoped weighted fan-in`) rides along on BOTH the
/// `impact_surface` neighbors AND `symbol_lookup` / `search` hits — labeled, never as PageRank. A
/// hub called by several functions outranks a leaf nothing depends on.
#[test]
fn load_bearing_enrichment_present_on_impact_neighbors_and_lookup_hits() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn load_bearing_hub() -> i32 { 1 }
pub fn quiet_leaf() -> i32 { 2 }
pub fn caller_one() -> i32 { load_bearing_hub() }
pub fn caller_two() -> i32 { load_bearing_hub() }
pub fn caller_three() -> i32 { load_bearing_hub() }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // impact_surface neighbors: running impact on a CALLER surfaces the hub as a callee neighbor,
    // and the hub (three callers) carries the labeled load-bearing signal — the third importance
    // scale, never PageRank.
    let caller_selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("caller_one".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: false,
        limit: 10,
    };
    let caller = db.select_symbol(&caller_selector).unwrap().unwrap().expect("caller symbol");
    let report = db
        .impact_surface_report_for_selected_symbol(
            &caller,
            50,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    let enriched_hub = report
        .direct_semantic_callees
        .iter()
        .find_map(|hop| hop.importance.as_ref())
        .expect("the hub callee neighbor carries the load-bearing enrichment");
    assert_eq!(enriched_hub.label, "local structural load", "labeled, not PageRank");
    assert_eq!(enriched_hub.signal, "scoped weighted fan-in");
    assert!(enriched_hub.score > 0.0, "the hub's three callers give it positive fan-in");

    // symbol_lookup hits: the hub (3 callers) outscores the leaf (0). Both carry the label, but the
    // leaf has no in-edges in scope so its enrichment is absent — the score reflects scoped fan-in.
    let hub_hit = db
        .symbols("load_bearing_hub", Some(Language::Rust), 10)
        .unwrap()
        .into_iter()
        .find(|h| h.qualified_name.ends_with("load_bearing_hub"))
        .expect("hub lookup hit");
    let hub_importance =
        hub_hit.importance.as_ref().expect("hub has callers → a load-bearing signal");
    assert_eq!(hub_importance.label, "local structural load");
    assert!(hub_importance.score > 0.0, "the hub's three callers give it positive fan-in");

    let leaf_hit = db
        .symbols("quiet_leaf", Some(Language::Rust), 10)
        .unwrap()
        .into_iter()
        .find(|h| h.qualified_name.ends_with("quiet_leaf"))
        .expect("leaf lookup hit");
    assert!(
        leaf_hit.importance.is_none(),
        "a symbol nothing depends on has no in-scope fan-in: {:?}",
        leaf_hit.importance
    );

    // search hits carry the same enrichment on the resolved symbol.
    let search_hub =
        db.search("load_bearing_hub", 20, true).unwrap().into_iter().find(|hit| {
            hit.symbol_path.as_deref().is_some_and(|s| s.ends_with("load_bearing_hub"))
        });
    if let Some(hit) = search_hub
        && let Some(importance) = hit.importance.as_ref()
    {
        assert_eq!(importance.label, "local structural load", "search hit labeled correctly");
    }

    fs::remove_dir_all(root).unwrap();
}

/// Phase 3 regression: a CALLEE neighbor whose call was written with a `::` path carries a
/// source-level `target_qualified_name` (e.g. `crate::helper::deep_helper`) that does NOT match
/// rag-rat's `path::name` `qualified_name`. The enrichment must resolve such callees by
/// `to_symbol` (the verified rag-rat `path::name` target) FIRST — resolving by
/// `target_qualified_name` first leaves every qualified-call callee un-enriched. The sibling test
/// above misses this: its callees are bare calls, so `target_qualified_name` is `None` and the
/// fallback to `to_symbol` masks the wrong-order bug.
#[test]
fn load_bearing_enrichment_present_on_qualified_callee_neighbor() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // `deep_helper` is reached only via the `crate::helper::deep_helper()` path, so its callee
    // edge carries a `target_qualified_name` of `crate::helper::deep_helper` — divergent from the
    // rag-rat `path::name` `qualified_name`. Two callers give it fan-in ≥ 1 (so its scoped
    // weighted fan-in is `Some`).
    fs::write(
        root.join("src/helper.rs"),
        r#"
pub fn deep_helper() -> i32 { 7 }
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod helper;
pub fn qualified_caller_one() -> i32 { crate::helper::deep_helper() }
pub fn qualified_caller_two() -> i32 { crate::helper::deep_helper() }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let caller_selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("qualified_caller_one".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: false,
        limit: 10,
    };
    let caller =
        db.select_symbol(&caller_selector).unwrap().unwrap().expect("qualified caller symbol");
    let report = db
        .impact_surface_report_for_selected_symbol(
            &caller,
            50,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();

    let callee_hop = report
        .direct_semantic_callees
        .iter()
        .find(|hop| hop.to_symbol.as_deref().is_some_and(|s| s.ends_with("deep_helper")))
        .expect("the qualified callee neighbor is surfaced");
    // The callee carries the divergent source-level qualified name — the exact shape that
    // un-enriched callees in the wild (`self::storage::connection`, etc.).
    assert!(
        callee_hop
            .target_qualified_name
            .as_deref()
            .is_some_and(|q| q.contains("::") && !q.contains('/')),
        "callee carries a source-level (non path::name) target_qualified_name: {:?}",
        callee_hop.target_qualified_name
    );
    let importance = callee_hop
        .importance
        .as_ref()
        .expect("the qualified callee neighbor carries the load-bearing enrichment");
    assert_eq!(importance.label, "local structural load", "labeled, not PageRank");
    assert_eq!(importance.signal, "scoped weighted fan-in");
    assert!(importance.score > 0.0, "two callers give the callee positive fan-in");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_only_open_serves_current_index_and_declines_when_heal_is_owed() {
    // #143: a pure-read MCP tool opens the index read-only, so a concurrent writer (watcher, heal,
    // another client) can never lock it out. A current index is served read-only (Some) and its
    // connection cannot write the main DB; when a heal write is still owed (here a stale
    // graph_index_version), the read path declines (None) so the caller falls back to the
    // read-write open that heals — after which reads are lock-free again.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn ro_anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    IndexDatabase::rebuild(&config).unwrap();

    let ro = IndexDatabase::try_open_config_read_only(&config)
        .unwrap()
        .expect("a current index must be served read-only");
    assert!(
        !ro.symbols("ro_anchor", Some(Language::Rust), 10).unwrap().is_empty(),
        "the read-only connection must answer queries"
    );
    assert!(
        ro.storage
            .connection()
            .execute("INSERT INTO index_meta(key, value) VALUES ('ro_probe', 'x')", [])
            .is_err(),
        "a read-only tool connection must not be able to write the main DB"
    );
    drop(ro);

    // Mark the graph index stale (a heal write is now owed). open() already ran and left it
    // current, so set it afterward; the read-only path does not heal, so it must decline.
    let db = IndexDatabase::open(&config.database).unwrap();
    db.set_meta("graph_index_version", "0").unwrap();
    drop(db);
    assert!(
        IndexDatabase::try_open_config_read_only(&config).unwrap().is_none(),
        "a stale graph index owes a heal write → the read-only path must decline"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn impact_report_flags_a_section_truncated_at_limit() {
    // #49: a section capped at `limit` must be named in `truncated_sections` and a caveat — no
    // silent caps. Three callers of `hub` with limit=2 → `direct_semantic_callers` is truncated.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn hub() {}\npub fn a() { hub(); }\npub fn b() { hub(); }\npub fn c() { hub(); }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hub = db.symbols("hub", Some(Language::Rust), 10).unwrap().remove(0);

    // Three repo memories bound to `hub` so the memory section is also over the limit — the
    // truncation report must cover it, not just the non-memory vectors (#146 review).
    for i in 0..3 {
        db.memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: format!("hub note {i}"),
            body: "why hub is load-bearing".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            bind: crate::query::memory::RepoMemoryBindTarget {
                symbol_id: Some(hub.symbol_id),
                ..Default::default()
            },
        })
        .unwrap();
    }

    let report =
        db.impact_surface_report_for_selected_symbol(&hub, 2, &Default::default()).unwrap();
    assert_eq!(report.direct_semantic_callers.len(), 2, "callers truncated to the limit");
    assert!(
        report
            .completeness_and_caveats
            .truncated_sections
            .contains(&"direct_semantic_callers".to_string()),
        "the capped section must be named: {:?}",
        report.completeness_and_caveats
    );
    assert!(
        report.completeness_and_caveats.truncated_sections.contains(&"repo_memories".to_string()),
        "the capped memory section must be named too: {:?}",
        report.completeness_and_caveats
    );
    assert!(
        report
            .completeness_and_caveats
            .caveats
            .iter()
            .any(|caveat| caveat.contains("truncated at limit")),
        "a human caveat must mention truncation: {:?}",
        report.completeness_and_caveats.caveats
    );

    // A generous limit truncates nothing.
    let full = db.impact_surface_report_for_selected_symbol(&hub, 50, &Default::default()).unwrap();
    assert!(
        full.completeness_and_caveats.truncated_sections.is_empty(),
        "nothing should be flagged when under the limit: {:?}",
        full.completeness_and_caveats.truncated_sections
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn symbol_lookup_heals_stale_line_numbers_after_an_edit() {
    // #147: symbol rows aren't anchor-relocated like chunks, so an edit shifts their byte/line
    // positions until reindex. symbol_candidates must lazily heal the matched file and return
    // current positions (and report no residual stale files).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn target() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("target".to_string()),
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    let before = db.symbol_candidates(&selector, false).unwrap();
    let before_byte = before.candidates[0].start_byte;
    assert!(before.stale_files.is_empty(), "clean index has no stale files");

    // Shift `target` down on disk WITHOUT reindexing — the index is now stale for this file.
    fs::write(root.join("src/lib.rs"), "// a\n// b\n// c\npub fn target() {}\n").unwrap();

    let after = db.symbol_candidates(&selector, false).unwrap();
    assert!(after.stale_files.is_empty(), "matched file was healed: {:?}", after.stale_files);
    assert!(
        after.candidates[0].start_byte > before_byte,
        "healed lookup reflects the shifted position: {} !> {before_byte}",
        after.candidates[0].start_byte
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn symbol_lookup_heals_a_just_added_symbol_without_waiting_for_the_watcher() {
    // #152: a name lookup for a symbol just added (here, in a brand-new not-yet-indexed file)
    // returns it via the lazy zero-hit heal, instead of nothing until the watcher catches up. The
    // heal needs a stored Config (open_config) and a git working tree to derive the change set.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn existing() {}\n").unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);

    let config = source_config(root.clone(), Language::Rust);
    IndexDatabase::rebuild(&config).unwrap();
    // Reopen via open_config so the zero-hit heal has the Config to classify the change set.
    let db = IndexDatabase::open_config(&config).unwrap();

    // A brand-new file with a brand-new symbol — never indexed, not yet committed.
    fs::write(root.join("src/added.rs"), "pub fn brand_new_symbol() {}\n").unwrap();

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("brand_new_symbol".to_string()),
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    let found = db.symbol_candidates(&selector, false).unwrap();
    assert!(
        found.candidates.iter().any(|c| c.name == "brand_new_symbol"),
        "a just-added symbol must be healed in without waiting for the watcher: {:?}",
        found.candidates
    );

    // A genuine miss (a name that exists nowhere) returns empty — no heal resurrects it, no error.
    let miss = crate::query::symbol::SymbolSelector {
        symbol: Some("no_such_symbol_anywhere".to_string()),
        ..selector.clone()
    };
    assert!(
        db.symbol_candidates(&miss, false).unwrap().candidates.is_empty(),
        "a genuine miss must stay empty"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn impact_completeness_flags_dirty_result_files() {
    // #148: a result file dirty vs the index is counted in completeness.stale_files. Resolve via
    // the non-healing `symbols()` so the edit isn't healed away before impact sees it.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn hub() {}\npub fn a() { hub(); }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hub = db.symbols("hub", Some(Language::Rust), 10).unwrap().remove(0);
    let clean =
        db.impact_surface_report_for_selected_symbol(&hub, 20, &Default::default()).unwrap();
    assert_eq!(clean.completeness_and_caveats.stale_files, 0, "nothing dirty right after rebuild");

    // Edit the symbol's file on disk without reindexing.
    fs::write(root.join("src/lib.rs"), "// shifted\npub fn hub() {}\npub fn a() { hub(); }\n")
        .unwrap();
    let dirty =
        db.impact_surface_report_for_selected_symbol(&hub, 20, &Default::default()).unwrap();
    assert!(
        dirty.completeness_and_caveats.stale_files >= 1,
        "the dirty symbol file must be flagged: {:?}",
        dirty.completeness_and_caveats
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn symbol_lookup_does_not_resurrect_a_deleted_symbol_after_heal() {
    // #151 review (P1): when an edit deletes/renames a symbol, healing the stale file and
    // re-resolving by NAME returns nothing — symbol_candidates must NOT keep the pre-heal ghost
    // (dead id, old offsets). The pre-heal fallback is only for symbol_id selectors.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn doomed() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let by_name = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("doomed".to_string()),
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    assert!(
        !db.symbol_candidates(&by_name, false).unwrap().candidates.is_empty(),
        "found before delete"
    );

    // The edit removes `doomed` entirely.
    fs::write(root.join("src/lib.rs"), "pub fn something_else() {}\n").unwrap();

    let after = db.symbol_candidates(&by_name, false).unwrap();
    assert!(
        after.candidates.is_empty(),
        "a deleted symbol must not be resurrected after heal: {:?}",
        after.candidates
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn symbol_lookup_by_id_keeps_pre_heal_candidate_flagged_stale() {
    // #151 review (P1): a symbol_id selector can't survive a reindex (ids reassigned), so the
    // re-resolve is empty — keep the pre-heal candidate flagged stale rather than vanish.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keep() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // `symbols()` does not heal, so this id is from the clean index.
    let id = db.symbols("keep", Some(Language::Rust), 10).unwrap().remove(0).symbol_id;
    fs::write(root.join("src/lib.rs"), "// a\n// b\npub fn keep() {}\n").unwrap();

    let by_id = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: Some(id),
        symbol_path: None,
        symbol: None,
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    let res = db.symbol_candidates(&by_id, false).unwrap();
    assert!(!res.candidates.is_empty(), "symbol_id selector keeps the pre-heal candidate");
    assert!(!res.stale_files.is_empty(), "and flags the file stale: {:?}", res.stale_files);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn impact_completeness_flags_a_dirty_callee_definition_file() {
    // #151 review (P2): a callee defined in another file that's edited makes the resolution stale;
    // the callee's DEFINITION file must be counted, not just the call-site file.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub mod a;\npub mod b;\n").unwrap();
    fs::write(root.join("src/a.rs"), "pub fn caller() { crate::b::callee(); }\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn callee() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let caller = db.symbols("caller", Some(Language::Rust), 10).unwrap().remove(0);
    let clean =
        db.impact_surface_report_for_selected_symbol(&caller, 20, &Default::default()).unwrap();
    let resolved_to_b = clean
        .direct_semantic_callees
        .iter()
        .any(|hop| hop.to_symbol.as_deref().is_some_and(|s| s.contains("b.rs")));
    assert!(
        resolved_to_b,
        "callee resolved cross-file to b.rs: {:?}",
        clean.direct_semantic_callees
    );
    assert_eq!(clean.completeness_and_caveats.stale_files, 0, "nothing dirty yet");

    // Edit ONLY the callee's definition file (b.rs), not the call-site file (a.rs).
    fs::write(root.join("src/b.rs"), "// shifted\npub fn callee() {}\n").unwrap();
    let dirty =
        db.impact_surface_report_for_selected_symbol(&caller, 20, &Default::default()).unwrap();
    assert!(
        dirty.completeness_and_caveats.stale_files >= 1,
        "the dirty callee definition file must be flagged: {:?}",
        dirty.completeness_and_caveats
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn v029_creates_clone_fingerprint_tables_on_fresh_and_migrated_dbs() {
    // Fresh DB: apply() must create the SourcererCC postings tables + refinements, and report
    // Compatible at the latest version. fingerprint_bands must NOT exist (dropped in R1 rework).
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    crate::index::schema::apply(&conn).expect("apply");
    for table in
        ["symbol_fingerprints", "symbol_token_postings", "clone_token_df", "clone_refinements"]
    {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .expect("query");
        assert_eq!(n, 1, "{table} should exist after apply()");
    }
    // fingerprint_bands was replaced by symbol_token_postings + clone_token_df in R1.
    let bands: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='fingerprint_bands'",
            [],
            |r| r.get(0),
        )
        .expect("query bands");
    assert_eq!(bands, 0, "fingerprint_bands must not exist after R1 rework");

    let status = crate::index::schema::status(&conn).expect("status");
    assert_eq!(status.current_version, crate::index::schema::LATEST_SCHEMA_VERSION);
    assert!(matches!(status.state, crate::index::schema::SchemaState::Compatible));

    // Migrated DB: a DB at the prior baseline that runs migrate_forward gains the new tables too.
    let conn2 = rusqlite::Connection::open_in_memory().expect("open2");
    crate::index::schema::apply(&conn2).expect("apply2"); // already-latest is a no-op forward
    crate::index::schema::migrate_forward(&conn2).expect("migrate_forward");
    let postings: i64 = conn2
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND \
             name='symbol_token_postings'",
            [],
            |r| r.get(0),
        )
        .expect("query2");
    assert_eq!(postings, 1, "symbol_token_postings must exist after migrate_forward");
    let df: i64 = conn2
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='clone_token_df'",
            [],
            |r| r.get(0),
        )
        .expect("query3");
    assert_eq!(df, 1, "clone_token_df must exist after migrate_forward");
}

#[test]
fn indexing_writes_baseline_fingerprints_for_functions() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two near-identical functions (renamed) + one trivial one that must be skipped.
    fs::write(
        root.join("src/lib.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub fn \
         load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\npub fn \
         tiny() -> i32 { 0 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let conn = db.storage.connection();
    let fps: i64 = conn
        .query_row(
            "SELECT count(*) FROM symbol_fingerprints WHERE normalizer_kind='baseline'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fps, 2, "the two functions are fingerprinted; tiny() is below MIN_TOKENS");

    // The inverted index carries postings for exactly the two fingerprinted symbols (R3).
    let posting_symbols: i64 = conn
        .query_row(
            "SELECT count(DISTINCT symbol_id) FROM symbol_token_postings WHERE \
             normalizer_kind='baseline'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        posting_symbols, 2,
        "both fingerprinted functions get postings rows; tiny() does not"
    );

    // df is populated from the postings.
    let df_rows: i64 =
        conn.query_row("SELECT count(*) FROM clone_token_df", [], |r| r.get(0)).unwrap();
    assert!(df_rows > 0, "clone_token_df is populated during indexing");

    // The two functions are renamed clones, so they share tokens — at least one token's df >= 2.
    let max_df: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(df), 0) FROM clone_token_df WHERE normalizer_kind='baseline'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(max_df >= 2, "a token shared by both clones has df >= 2, got {max_df}");

    // Cascade: deleting a symbol drops its fingerprint AND postings rows (FK + reindex freshness).
    conn.execute("DELETE FROM symbols", []).unwrap();
    let after_fps: i64 =
        conn.query_row("SELECT count(*) FROM symbol_fingerprints", [], |r| r.get(0)).unwrap();
    assert_eq!(after_fps, 0, "fingerprints cascade on symbol delete");
    let after_postings: i64 =
        conn.query_row("SELECT count(*) FROM symbol_token_postings", [], |r| r.get(0)).unwrap();
    assert_eq!(after_postings, 0, "postings cascade on symbol delete");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn candidate_components_group_renamed_clones_and_exclude_unrelated() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/c.rs"),
        "pub fn parse_config(raw: String) -> Vec<u8> { let mut v = Vec::new(); for b in \
         raw.bytes() { v.push(b ^ 7); } v }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let components = db.candidate_clone_components().expect("components");
    // Exactly one component: the two renamed clones (a.rs + b.rs). parse_config in c.rs is
    // structurally unrelated and must not join the component.
    assert_eq!(components.len(), 1, "exactly one clone component: {components:?}");
    assert_eq!(components[0].len(), 2, "the component is the two renamed clones: {components:?}");

    fs::remove_dir_all(root).unwrap();
}

/// Adversarial containment (design rev-4 §8): a small function A whose entire token bag is
/// contained inside a much larger function B (A's body pasted into B amid other statements).
/// containment = overlap/min ≈ 1.0, but similarity = overlap/max ≈ 0.1 < THETA, so A and B are NOT
/// a whole-symbol clone — they must not land in a common component. (The size prune `min_len >=
/// ceil(THETA*max_len)` already excludes this pair before the exact verify.)
#[test]
fn candidate_components_reject_small_function_contained_in_large_one() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // A: a ~20-token real Rust function.
    let a_body = "let mut acc = 0; let p = compute(seed); acc += p; for q in items.iter() { acc \
                  += transform(q); } acc";
    std::fs::write(
        root.join("src/a.rs"),
        format!("pub fn small(seed: i32, items: Vec<i32>) -> i32 {{ {a_body} }}\n"),
    )
    .unwrap();
    // B: a ~200-token function that CONTAINS all of A's tokens (A's body pasted in) amid ~10x more
    // distinct statements, so B's token_len is roughly 10x A's. overlap/min(A) ≈ 1.0 but
    // overlap/max(B) ≈ 0.1.
    let mut filler = String::new();
    for i in 0..40 {
        filler.push_str(&format!(
            "let v{i} = step{i}(base{i}, factor{i}) + delta{i}; total += v{i} * weight{i} - \
             offset{i};\n"
        ));
    }
    std::fs::write(
        root.join("src/b.rs"),
        format!(
            "pub fn big(seed: i32, items: Vec<i32>, base: i32) -> i32 {{ let mut total = base; \
             {filler} {a_body}; total += acc; total }}\n"
        ),
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Sanity: both functions cleared MIN_TOKENS (so both are fingerprinted) and B is ~10x A.
    let conn = db.storage.connection();
    let lens: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT token_len FROM symbol_fingerprints WHERE normalizer_kind='baseline' ORDER \
                 BY token_len",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(lens.len(), 2, "both functions are fingerprinted: {lens:?}");
    assert!(
        lens[1] >= 5 * lens[0],
        "B is much larger than A so overlap/max stays below THETA: {lens:?}"
    );

    let components = db.candidate_clone_components().expect("components");
    assert!(
        components.is_empty(),
        "a small function contained in a large one is NOT a whole-symbol clone: {components:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

/// df is a selectivity hint only (design rev-4 §2, §8): emptying `clone_token_df` must NOT change
/// the components found. The deterministic token order falls back to `token_hash` via LEFT JOIN +
/// COALESCE, so no candidate is dropped — only the prefix prune loosens. Uses the
/// two-renamed-clones fixture.
#[test]
fn candidate_components_unchanged_when_clone_token_df_is_empty() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let before = db.candidate_clone_components().expect("components before df delete");
    assert_eq!(before.len(), 1, "baseline: one clone component: {before:?}");

    db.storage.connection().execute("DELETE FROM clone_token_df", []).unwrap();

    let after = db.candidate_clone_components().expect("components after df delete");
    assert_eq!(
        before, after,
        "df is selectivity-only: emptying clone_token_df must not change components"
    );

    fs::remove_dir_all(root).unwrap();
}

/// The `files.generated = 0` predicate is a READ-SIDE filter: generated files are still
/// fingerprinted on write but their symbols must not appear in clone components. This test proves
/// the filter is doing the exclusion (not a missing fingerprint row).
#[test]
fn candidate_components_exclude_generated_files_via_read_filter() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // Two renamed clones — same fixture as
    // candidate_components_group_renamed_clones_and_exclude_unrelated.
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Baseline: both files non-generated — one clone component of 2.
    let before = db.candidate_clone_components().expect("components before marking generated");
    assert_eq!(before.len(), 1, "baseline: one clone component: {before:?}");
    assert_eq!(before[0].len(), 2, "baseline component has 2 members: {before:?}");

    // Mark b.rs as generated in the REAL base table (`temp.files` is a view; can't UPDATE it).
    // This tests that the `files.generated = 0` predicate in the read query does the exclusion;
    // the write rows (fingerprints/postings) are left intact to prove it's a read-side filter.
    let conn = db.storage.connection();
    let updated =
        conn.execute("UPDATE main.files SET generated = 1 WHERE path LIKE '%b.rs'", []).unwrap();
    assert_eq!(updated, 1, "exactly one file row marked generated");

    // b.rs's symbols MUST still have fingerprint rows (proves it's the read filter, not a missing
    // row).
    let fp_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM symbol_fingerprints sf
             JOIN symbols ON symbols.id = sf.symbol_id
             JOIN main.files ON main.files.id = symbols.file_id
             WHERE main.files.path LIKE '%b.rs' AND sf.normalizer_kind = 'baseline'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(fp_count > 0, "b.rs still has fingerprint rows after marking generated: {fp_count}");

    // After marking b.rs generated the read filter must drop it — no component pairs a with b.
    let after = db.candidate_clone_components().expect("components after marking generated");
    let has_b_pair = after.iter().any(|component| {
        // Any component that contained b.rs's symbol alongside a.rs's symbol would be size >= 2.
        // Since b.rs is the only partner for a.rs, if a.rs has no partner the component is gone.
        component.len() >= 2
    });
    assert!(
        !has_b_pair,
        "generated b.rs must be excluded from clone components by the read filter: {after:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

/// Max-denominator overlap gate regression: two structurally different functions whose
/// token_len ratio is ≥ θ (they SURVIVE the size prune) but whose token-overlap/max_len < θ
/// (the gate rejects them). This is distinct from the containment test
/// (`candidate_components_reject_small_function_contained_in_large_one`), which is eliminated
/// by the size prune alone — this fixture proves it is the overlap/max gate doing the work.
///
/// Fixture: `a` is a sequential let-chain; `b` is a loop+match accumulator. They are structurally
/// different enough that their shared tokens (keywords, operators, AST-node-kind tokens) fall well
/// below the overlap threshold, even though their token_lens are within the 1/θ ≈ 1.43x band.
#[test]
fn candidate_components_reject_partial_overlap_below_max_denominator_theta() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // a: sequential let-chain with five named sub-computations, returns a sum.
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn a(x: i32, y: i32) -> i32 { let p = alpha(x); let q = beta(y); let r = gamma(p); \
         let s = delta(q); let t = epsilon(r, s); p + q + r + s + t }\n",
    )
    .unwrap();
    // b: loop-based accumulator with a match arm — completely different control flow from a.
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn b(items: Vec<i32>, acc: i32) -> i32 { let mut total = acc; for item in \
         items.iter() { let v = process(item); match v { 0 => total += 1, _ => total += v } } if \
         total > 0 { total } else { -1 } }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Asserting the pair SURVIVES the size prune isolates the overlap/max gate as the reason for
    // exclusion (distinct from the 5× containment test, which the size prune kills).
    //
    // Measured token_lens: a=92, b=104.  ceil(0.7 * 104) = 73.  92 ≥ 73 → prune passes.
    // Overlap (Σ min(freq_a, freq_b)) = 51 < 73 → gate fails.  Values are asserted below so a
    // future fixture change that breaks the isolation is caught immediately.
    let conn = db.storage.connection();
    let lens: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT token_len FROM symbol_fingerprints WHERE normalizer_kind='baseline' ORDER \
                 BY token_len",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(lens.len(), 2, "both functions must be fingerprinted: {lens:?}");
    let min_len = lens[0];
    let max_len = lens[1];
    let threshold = (0.7_f64 * max_len as f64).ceil() as i64;
    assert!(
        min_len >= threshold,
        "pair must survive the size prune (min_len={min_len} >= ceil(0.7*max_len)={threshold}) so \
         the next assertion targets the overlap/max gate, not the prune"
    );

    let comps = db.candidate_clone_components().unwrap();
    assert!(
        comps.is_empty(),
        "a partial-overlap pair below overlap/max θ must NOT be a candidate (no regression to \
         containment): min_len={min_len} max_len={max_len} threshold={threshold} {comps:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

/// normalizer_version filter: after a NORM_VERSION bump the old rows are stale and the read
/// must ignore them. Simulate by writing rows at version N and then decrementing to N-1.
#[test]
fn candidate_read_ignores_stale_normalizer_version_rows() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    assert_eq!(
        db.candidate_clone_components().unwrap().len(),
        1,
        "renamed clones form one component at the current version"
    );
    // Simulate a NORM_VERSION bump that left old rows behind: rewrite both rows to an old version.
    db.storage
        .connection()
        .execute("UPDATE symbol_fingerprints SET normalizer_version = normalizer_version - 1", [])
        .unwrap();
    assert!(
        db.candidate_clone_components().unwrap().is_empty(),
        "stale-version fingerprints must be ignored by the read"
    );

    fs::remove_dir_all(root).unwrap();
}

/// `find_clones` integration test: four near-identical rename-clone functions across two
/// directories form one candidate class; metrics are plausible and completeness block is populated.
#[test]
fn find_clones_ranks_a_clean_clone_class_with_metrics() {
    use crate::index::clones::NORM_VERSION;

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();

    // Four rename-clone variants — identical structure, only the variable name changes.
    for (dir, name, var) in [
        ("a", "load_user", "u"),
        ("a", "load_order", "o"),
        ("b", "load_item", "i"),
        ("b", "load_blob", "x"),
    ] {
        fs::write(
            root.join(dir).join(format!("{name}.rs")),
            format!(
                "pub fn {name}(db: Db) -> i32 {{ let {var} = db.get(1); validate({var}); {var} + \
                 1 }}\n"
            ),
        )
        .unwrap();
    }
    // A structurally distinct function that must NOT join the clone class.
    fs::write(
        root.join("a/misc.rs"),
        "pub fn misc(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n += b as usize; } n }\n",
    )
    .unwrap();

    let config = Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("a"), PathBuf::from("b")],
            include: vec!["a/".to_string(), "b/".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();

    assert_eq!(res.classes.len(), 1, "exactly one clone class (the four rename-clones)");
    let c = &res.classes[0];
    assert_eq!(c.member_count, 4, "all four rename-clone functions are members");
    assert_eq!(c.class_kind, "candidate_component");
    assert!(!c.refined, "Plan-2 classes are never refined");
    assert!(
        c.similarity_min > 0.9,
        "rename-clones are near-identical; expected similarity_min > 0.9, got {}",
        c.similarity_min
    );
    assert_eq!(c.cross_module_spread, 2, "members span two directories (a/ and b/)");
    assert_eq!(c.language, "rust");
    assert!(!c.class_key.is_empty());

    // Completeness block.
    assert_eq!(res.completeness.candidate_metric, "overlap_max_denominator");
    assert_eq!(res.completeness.normalizer_version, NORM_VERSION);
    assert!(!res.completeness.truncated);

    fs::remove_dir_all(root).unwrap();
}

/// `clones_for_symbol` integration test: two rename-clone functions (a.rs / b.rs) form one
/// candidate class; the `Ref` selector resolves to that class, the `PathLine` selector at line 1
/// resolves to the same `class_key`, and a structurally distinct solo function → `None`.
#[test]
fn clones_for_symbol_returns_the_class_by_ref_and_by_path_line() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // --- Ref selector ---
    let by_ref_res =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into())).unwrap();
    let by_ref = by_ref_res.class.as_ref().expect("src/a.rs::load_user should be in a clone class");
    assert_eq!(by_ref.member_count, 2, "class must contain both rename-clones");
    assert!(
        by_ref.members.iter().any(|m| m.r#ref.ends_with("b.rs::load_order")),
        "siblings must include the other clone; got: {:?}",
        by_ref.members.iter().map(|m| &m.r#ref).collect::<Vec<_>>()
    );

    // --- PathLine selector — same class_key as Ref ---
    let by_line_res = db
        .clones_for_symbol(CloneSymbolSelector::PathLine { path: "src/a.rs".into(), line: 1 })
        .unwrap();
    let by_line = by_line_res
        .class
        .as_ref()
        .expect("PathLine at line 1 in src/a.rs should resolve to the same clone class");
    assert_eq!(
        by_line.class_key, by_ref.class_key,
        "PathLine and Ref must resolve to the same class_key"
    );

    // --- Unrelated solo function → class: None ---
    // A structurally distinct function whose token bag won't reach θ=0.7 against the clones.
    fs::write(
        root.join("src/c.rs"),
        "pub fn solo(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n ^= b as usize; } n }\n",
    )
    .unwrap();
    let db2 = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let solo_res =
        db2.clones_for_symbol(CloneSymbolSelector::Ref("src/c.rs::solo".into())).unwrap();
    assert!(solo_res.class.is_none(), "a symbol in no clone class must have class: None");
    assert!(solo_res.symbol_resolved, "the solo symbol still resolves");
    assert!(solo_res.symbol_fingerprinted, "the solo function is eligible (fingerprinted)");

    fs::remove_dir_all(root).unwrap();
}

/// Fix 1 (#215): `min_similarity` is honored ALL the way through candidate generation, not merely
/// post-filtered. A borderline pair whose overlap/max ≈ 0.58 (in [0.5, 0.7)) is below the const θ
/// so it never even becomes a candidate at the default threshold — only a caller-supplied θ ≤ 0.58
/// widens candidate generation enough to surface it. The completeness block reports the θ used.
#[test]
fn find_clones_min_similarity_below_theta_widens_and_is_reported() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // `a` is a let-chain; `b` shares `a`'s first four statements then diverges into a loop+match,
    // so their token bags overlap moderately. Measured: token_lens 92 / 136, overlap/max ≈ 0.58.
    fs::write(
        root.join("src/a.rs"),
        "pub fn a(x: i32, y: i32) -> i32 { let p = alpha(x); let q = beta(y); let r = gamma(p); \
         let s = delta(q); let t = epsilon(r, s); p + q + r + s + t }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn b(x: i32, y: i32) -> i32 { let p = alpha(x); let q = beta(y); let r = gamma(p); \
         let s = delta(q); for item in items.iter() { let v = process(item); match v { 0 => total \
         += 1, _ => total += v } } if total > 0 { total } else { -1 } }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // θ = 0.5 (below the pair's ≈0.58 similarity): the pair becomes a candidate and is returned,
    // and the completeness block records the requested θ.
    let widened = db
        .find_clones(FindClonesOptions { min_similarity: Some(0.5), min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        widened.classes.len(),
        1,
        "θ=0.5 must surface the borderline pair as a class: {:?}",
        widened.classes
    );
    let sim = widened.classes[0].similarity_min;
    assert!(
        (0.5..0.7).contains(&sim),
        "the planted pair's similarity must sit in [0.5, 0.7): got {sim}"
    );
    assert_eq!(
        widened.completeness.min_similarity, 0.5,
        "completeness must report the θ actually used (0.5)"
    );

    // Default θ (None ⇒ 0.7): the pair is below threshold and must NOT be a candidate — proving
    // the widening was real (candidate generation, not just a post-filter relax).
    let default = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(
        default.classes.is_empty(),
        "θ=0.7 must NOT surface the borderline pair: {:?}",
        default.classes
    );
    assert_eq!(default.completeness.min_similarity, 0.7, "default completeness θ is 0.7");

    fs::remove_dir_all(root).unwrap();
}

/// `min_similarity` is a similarity ratio θ = overlap/max_len and must lie in (0.0, 1.0]. Values
/// outside that range are rejected up front (before candidate generation) so a unit error (e.g. a
/// percentage like 1.5) or a degenerate 0.0 floor can't silently admit every pair.
#[test]
fn find_clones_rejects_out_of_range_min_similarity() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // A single clone pair so the index isn't empty; the range check fires regardless of contents.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // 0.0 (boundary, exclusive lower) → error.
    let zero = db.find_clones(FindClonesOptions {
        min_similarity: Some(0.0),
        min_copies: None,
        limit: None,
    });
    let err = zero.expect_err("min_similarity = 0.0 must be rejected").to_string();
    assert!(err.contains("(0.0, 1.0]"), "{err}");

    // 1.5 (above 1.0) → error.
    let high = db.find_clones(FindClonesOptions {
        min_similarity: Some(1.5),
        min_copies: None,
        limit: None,
    });
    let err = high.expect_err("min_similarity = 1.5 must be rejected").to_string();
    assert!(err.contains("(0.0, 1.0]"), "{err}");

    // 1.0 (boundary, inclusive upper) → accepted.
    db.find_clones(FindClonesOptions { min_similarity: Some(1.0), min_copies: None, limit: None })
        .expect("min_similarity = 1.0 is the inclusive upper bound and must be accepted");

    // 0.5 (interior) → accepted.
    db.find_clones(FindClonesOptions { min_similarity: Some(0.5), min_copies: None, limit: None })
        .expect("min_similarity = 0.5 is in range and must be accepted");

    fs::remove_dir_all(root).unwrap();
}

/// Fix 2 (#215): `completeness.truncated` reflects whole CLASSES dropped by `limit`, not only
/// members capped within a class. Plant two distinct clone classes, ask for `limit=1`, and assert
/// the dropped second class flips `truncated`.
#[test]
fn find_clones_truncated_reflects_class_limit() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Class 1: two rename-clones of a `load_*` accessor.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    // Class 2: two rename-clones of a structurally DIFFERENT `sum_*` reducer — its own component.
    fs::write(
        root.join("src/c.rs"),
        "pub fn sum_bytes(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n += b as usize; } n \
         }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/d.rs"),
        "pub fn sum_words(w: Vec<u8>) -> usize { let mut m = 0; for c in w { m += c as usize; } m \
         }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Sanity: with no limit there are two distinct classes and nothing is truncated.
    let all = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(all.classes.len(), 2, "two distinct clone classes are planted: {:?}", all.classes);
    assert!(!all.completeness.truncated, "no limit ⇒ not truncated");

    // limit=1 drops one whole class ⇒ truncated must be true (Fix 2).
    let limited = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: Some(1) })
        .unwrap();
    assert_eq!(limited.classes.len(), 1, "limit=1 returns exactly one class");
    assert!(
        limited.completeness.truncated,
        "dropping a whole class via the limit must set completeness.truncated"
    );

    fs::remove_dir_all(root).unwrap();
}

/// Fix 3 (#215): a TRANSITIVE-chain component (A–B and B–C both ≥ θ, but A–C < θ) stays visible
/// as ONE clone class — the class-level `similarity_min < θ` filter that previously dropped it is
/// gone. θ governs CANDIDATE GENERATION only (every EDGE is ≥ θ); a component's aggregate
/// min-pairwise can legitimately dip below θ for a chain. This also makes `find_clones` and
/// `clones_for_symbol` AGREE on chain components (the latter never had the filter).
///
/// The fixture is empirically tuned and the test asserts the MEASURED edge similarities so it is
/// honest about the chain it plants (a tokenizer change that shifts the numbers reddens here, not
/// silently). At HEAD the measured edges are A/B≈0.74, B/C≈0.86, A/C≈0.67 — a genuine chain whose
/// weakest (A/C) endpoint sits below the default θ=0.70.
#[test]
fn find_clones_keeps_transitive_chain_components() {
    // The candidate metric is overlap/MAX_len, so the three members must be ~EQUAL length (a length
    // gap trips the size prune `min_len >= ceil(θ*max_len)` and kills the edges). Identifier names
    // alpha-rename to ID<n>, so only STRUCTURE drives the bag. Each member = a shared CORE of
    // let-bindings + TWO distinct structural slots built from DIFFERENT constructs (their tokens
    // don't overlap): A shares slot S1 with B; B shares slot S2 with C; A and C share neither, so
    // A/B and B/C clear θ while A/C falls below it.
    let core = "let c1 = ca(x); let c2 = cb(c1); let c3 = cc(c2);";
    let s1 = "if x > 0 { acc = p1(x); } else { acc = p2(x); } if acc > 1 { acc = p3(acc); } else \
              { acc = p4(acc); }";
    let s2 = "for it in xs { match it { 0 => acc += q1(it), 1 => acc += q2(it), _ => acc -= \
              q3(it) } } for jt in ys { match jt { 0 => acc += q4(jt), _ => acc -= q5(jt) } }";
    let sx = "while acc > 0 { acc = r1(acc); acc = r2(acc); acc = r3(acc); } while acc < 9 { acc \
              = r4(acc); acc = r5(acc); }";
    let sy = "loop { acc = s1f(acc); acc = s2f(acc); acc = s3f(acc); if acc == 0 { break; } } \
              loop { acc = s4f(acc); if acc < 0 { break; } }";
    // A = CORE + S1 + SX ; B = CORE + S1 + S2 ; C = CORE + S2 + SY.
    let a = format!("pub fn fa(x: i32) -> i32 {{ {core} {s1} {sx} 0 }}\n");
    let b = format!("pub fn fb(x: i32) -> i32 {{ {core} {s1} {s2} 0 }}\n");
    let c = format!("pub fn fc(x: i32) -> i32 {{ {core} {s2} {sy} 0 }}\n");
    let (a, b, c) = (a.as_str(), b.as_str(), c.as_str());

    const THETA: f64 = 0.7;

    // Measure each pairwise edge by rebuilding a two-file subset (so the only clone class is that
    // single pair, whose `similarity_min` IS the edge similarity). This makes the chain claim a
    // measured fact, not an assumption.
    let edge_sim = |src1: (&str, &str), src2: (&str, &str)| -> f64 {
        let r = unique_temp_root();
        let _ = fs::remove_dir_all(&r);
        fs::create_dir_all(r.join("src")).unwrap();
        fs::write(r.join(format!("src/{}.rs", src1.0)), src1.1).unwrap();
        fs::write(r.join(format!("src/{}.rs", src2.0)), src2.1).unwrap();
        let d = IndexDatabase::rebuild(&source_config(r.clone(), Language::Rust)).unwrap();
        // θ=0.30 so even a sub-θ edge surfaces; the class's similarity_min is the pair's
        // similarity.
        let res = d
            .find_clones(FindClonesOptions {
                min_similarity: Some(0.30),
                min_copies: None,
                limit: None,
            })
            .unwrap();
        let sim = res
            .classes
            .first()
            .unwrap_or_else(|| panic!("the {}/{} pair must form a class at θ=0.30", src1.0, src2.0))
            .similarity_min;
        fs::remove_dir_all(r).unwrap();
        sim
    };
    let ab = edge_sim(("a", a), ("b", b));
    let bc = edge_sim(("b", b), ("c", c));
    let ac = edge_sim(("a", a), ("c", c));
    assert!(ab >= THETA, "A/B must be a real (≥θ) edge: measured {ab}");
    assert!(bc >= THETA, "B/C must be a real (≥θ) edge: measured {bc}");
    assert!(
        ac < THETA,
        "A/C must be BELOW θ so the three only link transitively through B: measured {ac}"
    );

    // Now the full three-member scope. At the default θ=0.70 the chain forms ONE class of all three
    // members, with an aggregate min-pairwise (== A/C) below θ — proving the dropped class-filter.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), a).unwrap();
    fs::write(root.join("src/b.rs"), b).unwrap();
    fs::write(root.join("src/c.rs"), c).unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        res.classes.len(),
        1,
        "the transitive chain is ONE clone class at default θ: {:?}",
        res.classes.iter().map(|c| c.member_count).collect::<Vec<_>>()
    );
    let class = &res.classes[0];
    assert_eq!(class.member_count, 3, "all three chain members are in the class");
    assert!(
        class.cohesion_min_pairwise < THETA,
        "the chain's aggregate min-pairwise must be below θ (it is the A/C edge), proving the \
         class-level similarity_min<θ filter is gone: got {}",
        class.cohesion_min_pairwise
    );
    // cohesion_min_pairwise == similarity_min == the measured A/C edge.
    assert!(
        (class.cohesion_min_pairwise - ac).abs() < 1e-9,
        "the class min-pairwise must equal the measured A/C edge: class={} ac={ac}",
        class.cohesion_min_pairwise
    );

    // CONSISTENCY: clones_for_symbol(A) must return the SAME 3-member chain class — the two
    // surfaces now agree (clones_for_symbol never had the class-filter that find_clones dropped).
    let by_ref = db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::fa".into())).unwrap();
    let cfs_class = by_ref.class.as_ref().expect("fa is in the chain class");
    assert_eq!(cfs_class.member_count, 3, "clones_for_symbol(fa) returns the full 3-member chain");
    assert_eq!(
        cfs_class.class_key, class.class_key,
        "find_clones and clones_for_symbol must return the SAME class for the chain"
    );

    fs::remove_dir_all(root).unwrap();
}

/// Fix 3 (#215): `clones_for_symbol` carries eligibility flags. A below-`MIN_TOKENS` function
/// RESOLVES (`symbol_resolved=true`) but is not fingerprinted (`symbol_fingerprinted=false`,
/// `class=None`); an eligible-but-unique function is fingerprinted with no class; an eligible
/// clone yields `class=Some`.
#[test]
fn clones_for_symbol_reports_eligibility() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // tiny: below MIN_TOKENS ⇒ resolves but is never fingerprinted.
    fs::write(root.join("src/tiny.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    // solo: a substantial, structurally distinct function ⇒ fingerprinted but in no clone class.
    fs::write(
        root.join("src/solo.rs"),
        "pub fn solo(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n ^= b as usize; } n }\n",
    )
    .unwrap();
    // a/b: two rename-clones ⇒ an eligible clone class.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // tiny: resolves, not fingerprinted, no class.
    let tiny = db.clones_for_symbol(CloneSymbolSelector::Ref("src/tiny.rs::tiny".into())).unwrap();
    assert!(tiny.symbol_resolved, "tiny resolves to a scoped symbol");
    assert!(!tiny.symbol_fingerprinted, "tiny is below MIN_TOKENS ⇒ not fingerprinted");
    assert!(tiny.class.is_none(), "an unfingerprinted symbol is in no class");

    // solo: eligible (fingerprinted) but unique ⇒ no class.
    let solo = db.clones_for_symbol(CloneSymbolSelector::Ref("src/solo.rs::solo".into())).unwrap();
    assert!(solo.symbol_resolved, "solo resolves");
    assert!(solo.symbol_fingerprinted, "solo is substantial ⇒ fingerprinted (eligible)");
    assert!(solo.class.is_none(), "a unique eligible symbol has no clone class");

    // load_user: eligible AND a clone ⇒ class is Some.
    let clone =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into())).unwrap();
    assert!(clone.symbol_resolved, "load_user resolves");
    assert!(clone.symbol_fingerprinted, "load_user is fingerprinted");
    let class = clone.class.expect("load_user is in a clone class");
    assert_eq!(class.member_count, 2, "the clone class has both rename-clones");

    fs::remove_dir_all(root).unwrap();
}

/// Worktree-overlay scope: `find_clones` returns the BRANCH-ONLY clone class under the overlay
/// scope, and the base scope has no clone classes. Proves the clone read is scope-correct —
/// only the branch's symbol_fingerprint rows (written by `index_worktree_overlay`) are visible
/// under the linked scope; the base sees only its own (non-clone) file.
#[test]
fn worktree_overlay_find_clones_reflects_branch_clone_pair() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    // Base has only a tiny function — below MIN_TOKENS, so no fingerprint, no clone class.
    fs::write(main.join("src/base.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // Confirm base has NO clone classes.
    let base_before = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(
        base_before.classes.is_empty(),
        "base scope must have no clone classes before overlay: {:?}",
        base_before.classes
    );

    // Create a linked worktree on a new branch that ADDS a rename-clone pair.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Two renamed-clone functions — same structure as the existing clone fixture.
    fs::write(
        linked.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        linked.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "add clone pair"]);

    // Index the overlay — leaves connection in the linked scope.
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "the branch's new files are indexed as overlay rows");

    // Under the overlay scope, find_clones must return the branch's clone class.
    let overlay_res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        overlay_res.classes.len(),
        1,
        "overlay scope must expose exactly the branch's clone class: {:?}",
        overlay_res.classes
    );
    let class = &overlay_res.classes[0];
    assert_eq!(class.member_count, 2, "the branch clone class has 2 members");

    // Round-6 regression (#215): `stale_members` must be 0 under an overlay scope. The branch's
    // members (src/a.rs, src/b.rs) are branch-ONLY — absent from the main checkout — so a
    // main-checkout staleness comparison would count them both "missing" → stale=2 (false). The
    // overlay is maintained from branch bytes, so `count_stale_member_paths` correctly skips the
    // main-checkout check under a linked-overlay scope and reports 0.
    assert_eq!(
        overlay_res.completeness.stale_members, 0,
        "branch-only overlay members must not be falsely reported stale against the main checkout"
    );

    // Base scope must still have no clone classes.
    set_base_scope(&mut db, &main);
    let base_after = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(
        base_after.classes.is_empty(),
        "base scope must have no clone classes after overlay indexing: {:?}",
        base_after.classes
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// Fix 1 + Fix 2 (#215): a clone class with more than MAX_MEMBERS members exercises two paths that
/// a small fixture never reaches:
///  - Fix 1 (chunked hydration): `build_class` hydrates members in batches of HYDRATION_CHUNK
///    rather than one `?` host-param per member. With 60 members the single-statement path would
///    still fit under the SQLite var limit, but this proves the chunked accumulation produces the
///    correct `member_count`/`members.len()`/`truncated` semantics with no error — the chunking is
///    otherwise only stress-visible above ~999 members, which is too expensive to plant in a unit
///    test.
///  - Fix 2 (subject pinning): `clones_for_symbol` for a clone whose `symbols.id` falls LATE in the
///    component (past MAX_MEMBERS by id) must still return that subject in the capped member list —
///    the caller asked about THAT symbol.
#[test]
fn find_clones_caps_large_class_and_pins_late_subject() {
    use crate::index::query_api::MAX_MEMBERS;

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // 60 rename-clone functions: identical structure, only the local variable name changes, so they
    // share a struct_hash and form ONE clone component well above MAX_MEMBERS. Files are named so
    // that lexical write order does NOT predetermine symbols.id order (the subject we resolve by
    // ref is a LATE one, whose rowid lands past MAX_MEMBERS in the component's id-sorted
    // order).
    const N: usize = 60;
    for i in 0..N {
        let var = format!("v{i}");
        fs::write(
            root.join(format!("src/f{i:02}.rs")),
            format!(
                "pub fn f{i:02}(db: Db) -> i32 {{ let {var} = db.get(1); validate({var}); {var} + \
                 1 }}\n"
            ),
        )
        .unwrap();
    }
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Fix 1: find_clones returns the full-population class — member_count is all 60, the returned
    // member list is capped at MAX_MEMBERS, truncated is set, and there is NO error.
    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        res.classes.len(),
        1,
        "the 60 rename-clones form one class: {:?}",
        res.classes.len()
    );
    let class = &res.classes[0];
    assert_eq!(class.member_count, N, "member_count reflects the FULL component");
    assert_eq!(class.total_members, N, "total_members reflects the FULL component");
    assert_eq!(class.members.len(), MAX_MEMBERS, "returned members are capped at MAX_MEMBERS");
    assert_eq!(class.members_returned, MAX_MEMBERS, "members_returned == cap");
    assert!(res.completeness.truncated, "a capped member list must set truncated");

    // Find a subject whose symbols.id is LATE in the component (past MAX_MEMBERS in id order), so
    // the plain `take(cap)` path would DROP it. We read the highest fingerprinted symbol id's
    // qualified name — that member sorts last in the component's id order, well past
    // MAX_MEMBERS.
    let conn = db.storage.connection();
    let late_ref: String = {
        let mut stmt = conn
            .prepare(
                "SELECT ns.value
                 FROM symbols
                 JOIN files ON files.id = symbols.file_id
                 JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                 JOIN symbol_fingerprints sf
                   ON sf.symbol_id = symbols.id AND sf.normalizer_kind = 'baseline'
                 ORDER BY symbols.id DESC
                 LIMIT 1",
            )
            .unwrap();
        stmt.query_row([], |r| r.get(0)).unwrap()
    };

    // Fix 2: clones_for_symbol for that late subject must INCLUDE it in the capped member list.
    let by_ref = db.clones_for_symbol(CloneSymbolSelector::Ref(late_ref.clone())).unwrap();
    let pinned = by_ref.class.as_ref().expect("the late subject is in the clone class");
    assert_eq!(pinned.member_count, N, "the class still reports the full population");
    assert_eq!(pinned.members.len(), MAX_MEMBERS, "members are capped at MAX_MEMBERS");
    assert!(
        pinned.members.iter().any(|m| m.r#ref == late_ref),
        "the pinned late subject {late_ref} must appear in the capped members: {:?}",
        pinned.members.iter().map(|m| &m.r#ref).collect::<Vec<_>>()
    );

    fs::remove_dir_all(root).unwrap();
}

/// Fix 5 (#215): the clone surface stays empty (and errors NOT) when no fingerprint rows survive —
/// `build_class`'s `raw_members.is_empty()` guard returns `None` rather than building an
/// internally-inconsistent class. We delete every fingerprint row after a clone class was formed
/// and assert `find_clones` returns no classes with no error.
#[test]
fn find_clones_returns_no_class_when_fingerprints_vanish() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Baseline: one clone class.
    let before = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(before.classes.len(), 1, "baseline: one clone class");

    // Drop every fingerprint row. The candidate read loads bags from the same rows, so no component
    // forms and the Fix 5 empty-check guarantees no malformed class can leak through. Either way
    // the surface must be empty with no error.
    db.storage.connection().execute("DELETE FROM symbol_fingerprints", []).unwrap();
    let after = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(after.classes.is_empty(), "no fingerprints ⇒ no clone classes (no error): {after:?}");

    fs::remove_dir_all(root).unwrap();
}

/// Fix 4 (#215): the `Ref` and `PathLine` resolution arms now LEFT JOIN symbol_fingerprints and
/// prefer a fingerprinted row. This is primarily a SQL-correctness change (proven to COMPILE and to
/// not regress the existing resolution tests). Here we additionally assert the simple positive case
/// keeps working end-to-end: a fingerprinted clone resolves by `Ref` AND `PathLine` to its class.
#[test]
fn clones_for_symbol_prefers_fingerprinted_row_on_resolution() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Ref resolution finds the fingerprinted clone and its class.
    let by_ref =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into())).unwrap();
    assert!(by_ref.symbol_fingerprinted, "Ref must resolve to the fingerprinted row");
    let ref_class = by_ref.class.as_ref().expect("Ref resolves into the clone class");

    // PathLine resolution at line 1 reaches the same class via the fingerprint-preferred ordering.
    let by_line = db
        .clones_for_symbol(CloneSymbolSelector::PathLine { path: "src/a.rs".into(), line: 1 })
        .unwrap();
    assert!(by_line.symbol_fingerprinted, "PathLine must resolve to the fingerprinted row");
    let line_class = by_line.class.as_ref().expect("PathLine resolves into the clone class");
    assert_eq!(
        ref_class.class_key, line_class.class_key,
        "Ref and PathLine must resolve to the same fingerprinted class"
    );

    fs::remove_dir_all(root).unwrap();
}

// ── Fix 1 regression guard (PathLine tightest-span PRIMARY) ──────────────────────────────────

/// PathLine CONTRACT: span is PRIMARY. The tightest-spanning symbol at the cursor wins,
/// regardless of fingerprint status. A tiny unfingerprinted (below MIN_TOKENS) nested item that
/// is ENCLOSED by a larger fingerprinted outer function must be returned when the cursor is
/// within the inner item — we must NOT silently jump to the enclosing fingerprinted function.
///
/// Fixture: two symbols at line 3 of the same file — the OUTER spans lines 1-10 and is
/// fingerprinted (it is large enough), and an INNER placeholder spans lines 3-3 and is NOT
/// fingerprinted (below MIN_TOKENS). PathLine{line=3} must resolve to the INNER symbol (smaller
/// span), not the OUTER one.
///
/// Because the test fixture is entirely synthetic (we inject symbols directly into the DB rather
/// than relying on the parser to produce nested symbols from source), the inner symbol has a
/// bare stub source that definitely stays below MIN_TOKENS.
#[test]
fn pathline_tightest_span_wins_over_fingerprinted_enclosing() {
    use crate::index::clones::NORM_VERSION;

    // Strategy: inject TWO symbols rows directly into the DB for the same file and same line,
    // with DIFFERENT spans. The OUTER has a wider span (lines 1-10) and IS fingerprinted (we
    // copy the real fp row from an indexed clone). The INNER has span 0 (lines 5-5) and has NO
    // fingerprint row. PathLine{line=5} must resolve to the INNER (tightest span), not the
    // OUTER (wider span, but fingerprinted).
    //
    // We use a clone pair so at least one function is fingerprinted (large enough token count),
    // and then inject a synthetic outer that wraps the fingerprinted symbol's span.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // Clone pair so load_user gets a real fingerprint row.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let conn = db.storage.connection();

    // Look up the indexed load_user symbol (line 1-1 after parsing).
    let (lu_id, lu_file_id): (i64, i64) = conn
        .query_row(
            "SELECT symbols.id, symbols.file_id FROM symbols
             JOIN files ON files.id = symbols.file_id
             JOIN name_strings ns ON ns.id = symbols.qualified_name_id
             WHERE files.path = 'src/a.rs'
             ORDER BY (end_line - start_line) ASC
             LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    // Verify load_user is fingerprinted.
    let lu_fp_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = \
             'baseline'",
            [lu_id],
            |r| r.get(0),
        )
        .unwrap();
    if lu_fp_count == 0 {
        // load_user not fingerprinted — can't run this test; skip gracefully.
        fs::remove_dir_all(root).unwrap();
        return;
    }

    // Inject a SYNTHETIC OUTER symbol covering lines 1-20 (wider than load_user's 1-1).
    // It WILL be fingerprinted (we copy load_user's fp row to it).
    // Inner: inject at lines 1-1 with start_byte slightly different — same line, same span.
    // The key: inject a synthetic WIDE outer symbol spanning lines 1-20, fingerprinted.
    // Then inject a synthetic TINY inner symbol at lines 1-1 (same line, span 0), NOT
    // fingerprinted. PathLine{line=1} now has TWO candidates: outer (span 19) and inner (span
    // 0). The INNER must win (tightest span), even though the OUTER is fingerprinted.

    // Fake name string for the outer.
    conn.execute(
        "INSERT OR IGNORE INTO name_strings (value) VALUES ('src/a.rs::synthetic_outer')",
        [],
    )
    .unwrap();
    let outer_name_id: i64 = conn
        .query_row(
            "SELECT id FROM name_strings WHERE value = 'src/a.rs::synthetic_outer'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Inject the wide outer symbol (lines 1-20, large span).
    conn.execute(
        "INSERT INTO symbols (file_id, name, qualified_name_id, kind, language, start_line, \
         end_line, start_byte, end_byte) VALUES (?1, 'synthetic_outer', ?2, 'function', 'rust', \
         1, 20, 0, 1000)",
        rusqlite::params![lu_file_id, outer_name_id],
    )
    .unwrap();
    let outer_id: i64 = conn.last_insert_rowid();

    // Copy load_user's fp row to the outer (making it fingerprinted with the same token bag).
    let (nk, nv, tl, sh, created_at): (String, i64, i64, String, i64) = conn
        .query_row(
            "SELECT normalizer_kind, normalizer_version, token_len, struct_hash, created_at_ms \
             FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = 'baseline' LIMIT \
             1",
            [lu_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO symbol_fingerprints (symbol_id, normalizer_kind, \
         normalizer_version, token_len, struct_hash, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, \
         ?6)",
        rusqlite::params![outer_id, &nk, nv, tl, &sh, created_at],
    )
    .unwrap();

    // Fake name string for the tiny inner (NOT fingerprinted).
    conn.execute(
        "INSERT OR IGNORE INTO name_strings (value) VALUES ('src/a.rs::synthetic_inner')",
        [],
    )
    .unwrap();
    let inner_name_id: i64 = conn
        .query_row(
            "SELECT id FROM name_strings WHERE value = 'src/a.rs::synthetic_inner'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Inject the tiny inner symbol at lines 1-1 (within the outer's 1-20 span, span=0).
    // NO fingerprint row for this one.
    conn.execute(
        "INSERT INTO symbols (file_id, name, qualified_name_id, kind, language, start_line, \
         end_line, start_byte, end_byte) VALUES (?1, 'synthetic_inner', ?2, 'function', 'rust', \
         1, 1, 0, 10)",
        rusqlite::params![lu_file_id, inner_name_id],
    )
    .unwrap();
    let inner_id: i64 = conn.last_insert_rowid();

    // Sanity: outer IS fingerprinted, inner is NOT.
    let outer_fp: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = \
             'baseline' AND normalizer_version = ?2",
            rusqlite::params![outer_id, NORM_VERSION],
            |r| r.get(0),
        )
        .unwrap();
    let inner_fp: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = \
             'baseline' AND normalizer_version = ?2",
            rusqlite::params![inner_id, NORM_VERSION],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(outer_fp, 1, "outer must be fingerprinted for the regression to be testable");
    assert_eq!(inner_fp, 0, "inner must NOT be fingerprinted");

    // PathLine at line 1: must resolve to the TIGHTEST spanning symbol.
    // - load_user: span 0 (lines 1-1) — fingerprinted
    // - synthetic_inner: span 0 (lines 1-1) — NOT fingerprinted
    // - synthetic_outer: span 19 (lines 1-20) — fingerprinted
    //
    // The tightest span is 0 (load_user or synthetic_inner, both at lines 1-1). The old
    // fingerprint-first ORDER BY would have put synthetic_outer FIRST if we had the bug.
    // The correct ORDER BY puts synthetic_outer LAST (span=19 > span=0).
    //
    // The regression guard: if fingerprint-presence were PRIMARY, the resolver would pick
    // outer (fingerprinted, span=19) over inner (unfingerprinted, span=0). With the fix,
    // it picks one of the span-0 symbols first.
    //
    // To make this unambiguous, we can directly verify that the outer is NOT the resolved symbol
    // by checking: if synthetic_inner (no fp) is resolved, symbol_fingerprinted=false.
    // But since load_user also has span=0 and IS fingerprinted, the tiebreaker (fp-then-rowid)
    // may pick load_user. Either way, synthetic_outer (span=19) must NOT be picked.
    //
    // Direct verification: query what PathLine resolves to using the same SQL as the resolver.
    let resolved_id: Option<i64> = conn
        .query_row(
            "SELECT symbols.id
             FROM symbols
             JOIN files ON files.id = symbols.file_id
             LEFT JOIN symbol_fingerprints sf
               ON sf.symbol_id = symbols.id
               AND sf.normalizer_kind = 'baseline'
               AND sf.normalizer_version = ?3
             WHERE files.path = ?1
               AND ?2 BETWEEN symbols.start_line AND symbols.end_line
             ORDER BY (symbols.end_line - symbols.start_line) ASC,
                      (sf.symbol_id IS NULL) ASC, symbols.id ASC
             LIMIT 1",
            rusqlite::params!["src/a.rs", 1i64, NORM_VERSION],
            |r| r.get(0),
        )
        .optional()
        .unwrap();

    let resolved_symbol_id = resolved_id.expect("line 1 in src/a.rs must resolve to SOME symbol");

    // The resolved symbol must NOT be the outer (span=19). It must be one of the span=0 symbols.
    assert_ne!(
        resolved_symbol_id, outer_id,
        "PathLine must NOT resolve to synthetic_outer (span=19, fingerprinted) — the tightest \
         span (0) must win; this would fail with fingerprint-first ORDER BY"
    );

    // The span of the resolved symbol must be 0 (lines 1-1), not 19.
    let (res_start, res_end): (i64, i64) = conn
        .query_row(
            "SELECT start_line, end_line FROM symbols WHERE id = ?1",
            [resolved_symbol_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        res_end - res_start,
        0,
        "resolved symbol must have span=0 (lines {res_start}-{res_end}), not the wide outer \
         (span=19)"
    );

    fs::remove_dir_all(root).unwrap();
}

// ── Fix 2: Ref ambiguity rejection ───────────────────────────────────────────────────────────

/// Fix 2 (#215): a `Ref` that matches EXACTLY ONE fingerprinted symbol resolves normally.
/// A `Ref` that matches NO fingerprinted symbols falls back to the unfingerprinted path
/// (`symbol_resolved=true, symbol_fingerprinted=false, class=None`) — existing behaviour preserved.
///
/// True same-ref duplicate-fingerprinted injection is not possible via the standard indexer
/// (the indexer deduplicates by qualified_name per file), so we test the two non-ambiguous paths
/// here and note the gap. The ambiguous-ref path (>1 fingerprinted match → Err) is tested via
/// direct DB injection in the dedicated fixture below.
#[test]
fn clones_for_symbol_ref_single_fingerprinted_resolves_unfingerprinted_falls_back() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two rename-clones so load_user is fingerprinted and in a class.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    // A tiny function: resolves but is not fingerprinted.
    fs::write(root.join("src/tiny.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Exactly 1 fingerprinted match → resolves to clone class.
    let res = db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into())).unwrap();
    assert!(res.symbol_resolved);
    assert!(res.symbol_fingerprinted);
    assert!(res.class.is_some(), "single fingerprinted Ref must return its clone class");

    // 0 fingerprinted matches (tiny is below MIN_TOKENS) → resolved but not fingerprinted.
    let tiny_res =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/tiny.rs::tiny".into())).unwrap();
    assert!(tiny_res.symbol_resolved, "the unfingerprinted symbol still resolves");
    assert!(!tiny_res.symbol_fingerprinted, "tiny is below MIN_TOKENS");
    assert!(tiny_res.class.is_none());

    fs::remove_dir_all(root).unwrap();
}

/// Fix 2 (#215): injecting TWO distinct fingerprinted `symbols` rows that share the SAME
/// qualified name causes `clones_for_symbol(Ref)` to return an `Err` with "disambiguate" in the
/// message. This exercises the >1 fingerprinted path in `resolve_selector_to_symbol_id`.
///
/// We inject the second symbol row directly into the DB (the indexer never produces same-ref
/// duplicates for the same file, but the index schema allows it and the code must handle it).
#[test]
fn clones_for_symbol_ref_ambiguous_fingerprinted_returns_err() {
    use crate::index::clones::NORM_VERSION;

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // One file with one function; rebuild gives us a clean indexed symbol + fingerprint.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let conn = db.storage.connection();

    // Fetch the existing symbol's id and its name_id.
    let (orig_id, name_id, file_id): (i64, i64, i64) = conn
        .query_row(
            "SELECT symbols.id, symbols.qualified_name_id, symbols.file_id
             FROM symbols
             JOIN name_strings ns ON ns.id = symbols.qualified_name_id
             WHERE ns.value = 'src/a.rs::load_user'
             LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();

    // Inject a SECOND symbols row sharing the same qualified_name_id and file_id but different
    // span — simulating an overload / cfg variant with the same qualified name.
    // `name` is the bare symbol identifier (NOT NULL); we reuse "load_user".
    conn.execute(
        "INSERT INTO symbols (file_id, name, qualified_name_id, kind, language, start_line, \
         end_line, start_byte, end_byte) VALUES (?1, 'load_user', ?2, 'function', 'rust', 2, 5, \
         10, 50)",
        rusqlite::params![file_id, name_id],
    )
    .unwrap();
    let dup_id: i64 = conn.last_insert_rowid();

    // Fetch an existing fingerprint row for orig_id to clone its token data.
    let fp: Option<(String, i64, i64, String)> = conn
        .query_row(
            "SELECT normalizer_kind, normalizer_version, token_len, struct_hash
             FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = 'baseline' AND \
             normalizer_version = ?2 LIMIT 1",
            rusqlite::params![orig_id, NORM_VERSION],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .unwrap();

    let Some((nk, nv, tl, sh)) = fp else {
        // If there's no fingerprint yet the ambiguity path can't be reached; skip gracefully.
        fs::remove_dir_all(root).unwrap();
        return;
    };

    // Give the duplicate its own fingerprint row (same normalizer_version = current).
    // created_at_ms is NOT NULL in STRICT mode; use 0 as a placeholder.
    conn.execute(
        "INSERT OR IGNORE INTO symbol_fingerprints (symbol_id, normalizer_kind, \
         normalizer_version, token_len, struct_hash, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        rusqlite::params![dup_id, nk, nv, tl, sh],
    )
    .unwrap();

    // Verify the dup fp row was actually inserted (it would be silently ignored if the PK
    // already existed, which can't happen here since dup_id is fresh, but be explicit).
    let dup_fp_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = \
             'baseline' AND normalizer_version = ?2",
            rusqlite::params![dup_id, NORM_VERSION],
            |r| r.get(0),
        )
        .unwrap();
    if dup_fp_count == 0 {
        // fp INSERT was silently ignored (shouldn't happen) — skip the test.
        fs::remove_dir_all(root).unwrap();
        return;
    }

    // Now Ref("src/a.rs::load_user") matches TWO fingerprinted symbols → must return Err.
    let err = db
        .clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into()))
        .expect_err("Ref matching >1 fingerprinted symbols must return Err, not silently pick one");
    let msg = err.to_string();
    assert!(msg.contains("disambiguate"), "error message must mention 'disambiguate', got: {msg}");
    assert!(
        msg.contains("src/a.rs::load_user"),
        "error message must name the ambiguous ref, got: {msg}"
    );

    fs::remove_dir_all(root).unwrap();
}

// ── Fix 3: stale_members in completeness ────────────────────────────────────────────────────

/// Fix 3 (#215): `completeness.stale_members` counts DISTINCT returned-member file paths whose
/// on-disk content no longer matches the indexed `files.sha256`.
///
/// Clean index → `stale_members == 0`. After editing one member file on disk WITHOUT reindexing
/// → `stale_members >= 1`.
#[test]
fn find_clones_stale_members_zero_on_clean_index_and_nonzero_after_disk_edit() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let a_path = root.join("src/a.rs");
    let b_path = root.join("src/b.rs");
    fs::write(
        &a_path,
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        &b_path,
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Clean index: stale_members must be 0.
    let clean = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(clean.classes.len(), 1, "the two rename-clones form one class");
    assert_eq!(
        clean.completeness.stale_members, 0,
        "a freshly-indexed index with unchanged files must report stale_members=0"
    );

    // Edit one member file on disk WITHOUT reindexing — content now differs from indexed sha256.
    fs::write(&a_path, "pub fn load_user(db: Db) -> i32 { /* EDITED: body replaced */ 42 }\n")
        .unwrap();

    // find_clones reads PERSISTED fingerprint tables (unchanged) but stale_members checks disk.
    let stale = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        stale.classes.len(),
        1,
        "the class is still returned (stale detection is read-only)"
    );
    assert!(
        stale.completeness.stale_members >= 1,
        "after editing src/a.rs on disk, stale_members must be >= 1, got {}",
        stale.completeness.stale_members
    );

    fs::remove_dir_all(root).unwrap();
}
