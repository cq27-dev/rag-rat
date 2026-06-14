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
    assert!(logical_columns.contains(&"qualified_name".to_string()));
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
    // index predates shared tables (e.g. edge_strings) that a later migration INSERTs into.
    // Simulate by dropping the edges view + edge_strings and EVERY post-019 ledger row (so the
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
         DROP TABLE IF EXISTS edge_strings;
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
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'edge_strings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);
    assert_eq!(edge_strings_exists, 1, "baseline did not reprovision edge_strings before replay");

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
                    text, text_hash, source_revision, anchor_version, normalized_hash,
                    start_boundary_hash, end_boundary_hash, start_context_hash, end_context_hash,
                    context_radius, embedding_policy, embedding_priority
                )
                VALUES (?1, 'symbol', 'other_context', 0, 12, 1, 1, 'other context', 'other-text',
                    'other-sha', 1, '', '', '', '', '', 2, 'Embed', 1)
                RETURNING id
                ",
            [other_file_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    db.storage
        .connection()
        .execute(
            "
                INSERT INTO main.symbols(
                    file_id, language, name, qualified_name, kind, start_byte, end_byte, \
             signature, docs
                )
                VALUES (?1, 'rust', 'other_context', 'other_context', 'function', 0, 12, NULL, \
             NULL)
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
    let lookup = db.symbol_candidates(&selector).unwrap();
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
    let relookup = db.symbol_candidates(&selector).unwrap();
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
        .symbol_candidates(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("spawn_blocking".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: true,
            limit: 10,
        })
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
    assert_eq!(impact.repo_memories.direct.len(), 1);
    assert_eq!(impact.completeness_and_caveats.memory_status.active, 1);
    assert_eq!(impact.completeness_and_caveats.memory_status.stale, 0);

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
    assert!(impact.repo_memories.direct.is_empty());
    assert_eq!(impact.repo_memories.path_crossed.len(), 1);
    assert_eq!(impact.repo_memories.path_crossed[0].memory_id, edge_memory.memory.memory_id);
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

    assert!(
        report
            .repo_memories
            .call_path_crossed
            .iter()
            .any(|memory| memory.title == "a -> b -> c is the hot path"),
        "call-path memory should surface in impact_surface(b); got call_path_crossed = {:?}",
        report.repo_memories.call_path_crossed.iter().map(|m| &m.title).collect::<Vec<_>>()
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
        .symbol_candidates(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: true,
            limit: 10,
        })
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
        .symbol_candidates(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("cfg_helper".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: true,
            limit: 10,
        })
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
        .symbol_candidates(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("spawn_blocking".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: true,
            limit: 10,
        })
        .unwrap()
        .candidates[0]
        .qualified_name
        .clone();
    let logical_id = db
        .symbol_candidates(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("spawn_blocking".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: true,
            limit: 10,
        })
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
fn v022_fresh_apply_creates_packages_and_dedicated_import_scope_columns() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();

    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    assert_eq!(schema::LATEST_SCHEMA_VERSION, 22);
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
        ",
    )
    .unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, 21, "now looks like a V21 index");
    assert!(!conn_table_exists(&conn, "packages"));

    // Forward-migrate: re-running apply (the Older→apply path) converges to V22.
    schema::apply(&conn).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, 22);
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
    let o1 = crate::query::orientation::orientation(conn, &root).unwrap();

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
    let o2 = crate::query::orientation::orientation(conn, &root).unwrap();
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
    let o = crate::query::orientation::orientation(conn.connection(), &root)
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
    let before = db.symbol_candidates(&selector).unwrap();
    let before_byte = before.candidates[0].start_byte;
    assert!(before.stale_files.is_empty(), "clean index has no stale files");

    // Shift `target` down on disk WITHOUT reindexing — the index is now stale for this file.
    fs::write(root.join("src/lib.rs"), "// a\n// b\n// c\npub fn target() {}\n").unwrap();

    let after = db.symbol_candidates(&selector).unwrap();
    assert!(after.stale_files.is_empty(), "matched file was healed: {:?}", after.stale_files);
    assert!(
        after.candidates[0].start_byte > before_byte,
        "healed lookup reflects the shifted position: {} !> {before_byte}",
        after.candidates[0].start_byte
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
