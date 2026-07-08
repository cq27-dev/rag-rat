use super::*;

#[test]
fn rebuild_bootstraps_sqlite_schema_for_empty_target_root() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();

    let config = Config {
        repo_id_override: None,
        database_key_pinned: true,
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
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        // This test deliberately rebuilds an EMPTY target root (no `.md` files) to verify schema
        // bootstrap; that is the sanctioned `--allow-empty` path now that the core refuses a
        // first-time-empty registration by default (#427).
        allow_empty: true,
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
}

#[test]
fn migrate_preserves_github_papertrail_cache() {
    // Whole-table `row_count` cache-total checks are single-repo by nature: they assert the
    // papertrail cache survived a schema migration. The poison sibling's V041-scoped github rows
    // would inflate the unscoped totals (production github reads ARE scoped — the multi_repo_scope
    // leak matrix proves it), so opt this cache-total test out of the harness.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
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

    let _ = fs::remove_dir_all(root);
}

#[test]
fn full_rebuild_preserves_github_papertrail_cache() {
    // Whole-table `row_count` cache-total checks are single-repo by nature: they assert the
    // papertrail cache survived a full rebuild. The poison sibling's V041-scoped github rows would
    // inflate the unscoped totals (production github reads ARE scoped — the multi_repo_scope leak
    // matrix proves it), so opt this cache-total test out of the harness.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
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

    let _ = fs::remove_dir_all(root);
}

#[test]
fn full_rebuild_preserves_installed_model_manifest() {
    let (root, mut config) = markdown_config("alpha token with enough detail for embeddings\n");
    // Select the hash embedder (the model this test installs) so config and the install agree — a
    // fresh index seeds the CONFIGURED model (#394), so a mismatched default would be re-seeded.
    config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    let before = db.llm_status().unwrap();
    assert_eq!(before.embedding.model_id, HASH_MODEL_ID);
    assert!(before.embedding.installed);
    drop(db);

    let db = IndexDatabase::rebuild(&config).unwrap();

    let after = db.llm_status().unwrap();
    assert_eq!(after.embedding.model_id, HASH_MODEL_ID);
    assert!(after.embedding.installed);
    assert_eq!(after.embedding.state, "Ready");

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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
    // #484: the refusal must carry the remedy — on a shared global DB one upgraded agent migrates
    // the schema and every older-binary process hits this, so a bare refusal reads as breakage.
    assert!(err.contains("upgrade rag-rat"), "{err}");
    assert!(err.contains("restart"), "{err}");
    // The fleet hot-upgrade re-execs armed MCP servers only on Linux; elsewhere the message must
    // say running servers stay stale until their sessions restart.
    if cfg!(target_os = "linux") {
        assert!(!err.contains("hot-upgrade"), "{err}");
    } else {
        assert!(err.contains("do not hot-upgrade"), "{err}");
    }

    let _ = fs::remove_dir_all(root);
}

/// #498: the forward-migrate REPLAY path (an existing versioned ledger — every open-time
/// auto-migrate) must never touch the `__dirty__` marker. The marker choreography ran in
/// autocommit around a baseline replay that is a pure idempotent no-op on a versioned DB, so any
/// failure between the stamp and the clear — the live incident was a `SQLITE_BUSY` from an
/// ordinary writer, which the GLOBAL schema lock deliberately does not serialize against —
/// stranded a durable marker on a healthy DB, and every subsequent open refused with "dirty or
/// partial schema migration detected" until a manual `index --full`.
#[test]
fn forward_migrate_replay_never_touches_the_dirty_marker() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    truncate_schema_to(&conn, 50);

    // Audit every write that targets the marker: the stamp (the INSERT half of `INSERT OR
    // REPLACE` fires the insert trigger) and the clear (DELETE).
    conn.execute_batch(
        "
        CREATE TABLE dirty_marker_audit(op TEXT NOT NULL);
        CREATE TRIGGER audit_dirty_stamp AFTER INSERT ON schema_version
        WHEN NEW.id = '__dirty__'
        BEGIN INSERT INTO dirty_marker_audit(op) VALUES ('stamp'); END;
        CREATE TRIGGER audit_dirty_clear BEFORE DELETE ON schema_version
        WHEN OLD.id = '__dirty__'
        BEGIN INSERT INTO dirty_marker_audit(op) VALUES ('clear'); END;
        ",
    )
    .unwrap();

    schema::migrate_forward(&conn).unwrap();

    let marker_writes: i64 =
        conn.query_row("SELECT COUNT(*) FROM dirty_marker_audit", [], |row| row.get(0)).unwrap();
    assert_eq!(
        marker_writes, 0,
        "a forward migrate over an existing ledger must not stamp or clear the dirty marker — the \
         stamp..clear window is what a concurrent writer's SQLITE_BUSY strands (#498)"
    );
    let status = schema::status(&conn).unwrap();
    assert_eq!(status.state, schema::SchemaState::Compatible);
    assert_eq!(status.current_version, schema::LATEST_SCHEMA_VERSION);
}

/// #498: a failure while REPLAYING a pending step must leave the schema `Older` — retryable on
/// the next open — never `Dirty` (which refuses every open until a manual rebuild). The injected
/// abort at the step's ledger record stands in for any mid-replay failure; the live one was a
/// `SQLITE_BUSY` during the V050→V051 step.
#[test]
fn forward_migrate_step_failure_leaves_a_retryable_older_schema_not_dirty() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    truncate_schema_to(&conn, 50);

    conn.execute_batch(
        "
        CREATE TRIGGER fail_051_record BEFORE INSERT ON schema_version
        WHEN NEW.id = '051_clone_df_epoch'
        BEGIN SELECT RAISE(ABORT, 'injected mid-replay failure'); END;
        ",
    )
    .unwrap();
    assert!(schema::migrate_forward(&conn).is_err(), "the injected failure fails the migrate");

    let status = schema::status(&conn).unwrap();
    assert_eq!(
        status.state,
        schema::SchemaState::Older,
        "a mid-replay failure must leave the schema Older (retryable), not Dirty: {}",
        status.message
    );

    // With the failure gone, the next migrate completes the pending step exactly once.
    conn.execute_batch("DROP TRIGGER fail_051_record;").unwrap();
    schema::migrate_forward(&conn).unwrap();
    let status = schema::status(&conn).unwrap();
    assert_eq!(status.state, schema::SchemaState::Compatible);
    assert_eq!(status.current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '051_clone_df_epoch'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the retried step is recorded exactly once");
}

/// #498 loser discipline: a forward migrate that finds NOTHING owed (001 and every additive step
/// already recorded — the loser of a migration race re-entering after the winner finished) must
/// leave the ledger completely untouched: no dirty stamp, no 001 re-record, no writes at all.
/// Every avoided write is one fewer `SQLITE_BUSY` hazard against concurrent ordinary writers.
#[test]
fn forward_migrate_when_nothing_is_owed_leaves_the_ledger_untouched() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();

    conn.execute_batch(
        "
        CREATE TABLE ledger_write_audit(op TEXT NOT NULL, id TEXT NOT NULL);
        CREATE TRIGGER audit_ledger_insert AFTER INSERT ON schema_version
        BEGIN INSERT INTO ledger_write_audit(op, id) VALUES ('insert', NEW.id); END;
        CREATE TRIGGER audit_ledger_update AFTER UPDATE ON schema_version
        BEGIN INSERT INTO ledger_write_audit(op, id) VALUES ('update', NEW.id); END;
        CREATE TRIGGER audit_ledger_delete AFTER DELETE ON schema_version
        BEGIN INSERT INTO ledger_write_audit(op, id) VALUES ('delete', OLD.id); END;
        ",
    )
    .unwrap();

    schema::migrate_forward(&conn).unwrap();

    let mut stmt = conn.prepare("SELECT op, id FROM ledger_write_audit").unwrap();
    let writes: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        writes.is_empty(),
        "a forward migrate with nothing owed must not write the ledger, got: {writes:?}"
    );
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Compatible);
}

/// #498: `rag-rat index --full` ([`schema::apply`]) is the sanctioned recovery the Dirty refusal
/// names, so a successful apply must CLEAR a stranded `__dirty__` marker: apply re-runs every
/// idempotent step, so a marker that survives it is provably stale. Without the clear, the
/// remedy would leave the DB refusing every open forever. (The marker can be stranded by an
/// older binary's failed mid-replay migrate; new binaries no longer stamp it on replay.)
#[test]
fn apply_clears_a_stranded_dirty_marker() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO schema_version(id, applied_at_ms, checksum, description)
         VALUES ('__dirty__', 1, '', 'partial migration in progress')",
        [],
    )
    .unwrap();
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Dirty);

    schema::apply(&conn).unwrap();

    let status = schema::status(&conn).unwrap();
    assert_eq!(
        status.state,
        schema::SchemaState::Compatible,
        "a full apply must clear a stranded dirty marker: {}",
        status.message
    );
}

/// #498 review: a Dirty state can also come from a CHECKSUM-MISMATCHED baseline row (not just a
/// stranded marker), and `index --full` must recover that too — apply re-records every additive
/// step's checksum unconditionally, and the baseline provision must treat a stale 001 checksum
/// as "provision owed" so the 001 row is refreshed as well.
#[test]
fn apply_recovers_a_baseline_checksum_mismatch() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn.execute(
        "UPDATE schema_version SET checksum = 'sha256:corrupted'
         WHERE id = '001_sqlite_storage_baseline'",
        [],
    )
    .unwrap();
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Dirty);

    schema::apply(&conn).unwrap();

    let status = schema::status(&conn).unwrap();
    assert_eq!(
        status.state,
        schema::SchemaState::Compatible,
        "a full apply must refresh a checksum-mismatched baseline row: {}",
        status.message
    );
}

/// #498: the forward-migrate variant of the double-checked schema race
/// ([`concurrent_create_or_migrate_applies_the_schema_exactly_once`] covers the fresh-CREATE
/// side): two upgraded processes racing one pending step over a shared V(N-1) DB. Both opens must
/// succeed — the winner migrates under the GLOBAL schema lock, the loser re-checks under it and
/// no-ops — and the ledger must hold exactly one row per migration with no stranded dirty marker.
/// Before #498 the loser's UNLOCKED status probe could catch the winner's transient `__dirty__`
/// stamp and refuse the open outright instead of waiting on the lock.
#[test]
fn concurrent_forward_migrate_applies_the_pending_step_exactly_once() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let db_path = root.join("index.sqlite");

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        schema::apply(&conn).unwrap();
        truncate_schema_to(&conn, 50);
        // Make the pending step real work again, as on a live V050 DB.
        conn.execute_batch("DROP TABLE IF EXISTS clone_df_epoch;").unwrap();
    }

    let a = db_path.clone();
    let b = db_path.clone();
    let t1 = std::thread::spawn(move || IndexDatabase::open(&a).map(|_| ()));
    let t2 = std::thread::spawn(move || IndexDatabase::open(&b).map(|_| ()));
    t1.join().unwrap().expect("one concurrent opener migrates forward");
    t2.join().unwrap().expect("the other re-checks under the schema lock and no-ops");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let status = schema::status(&conn).unwrap();
    assert_eq!(status.state, schema::SchemaState::Compatible);
    assert_eq!(status.current_version, schema::LATEST_SCHEMA_VERSION);
    assert_eq!(
        status.migrations.len(),
        schema::LATEST_SCHEMA_VERSION as usize,
        "exactly one schema_version row per migration and no stranded dirty marker"
    );

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
}

/// V046 (dream v2 pass 0): fresh `schema::apply` creates the `memory_reality` / `memory_summaries`
/// sibling tables, both STRICT + repo_id-scoped with the documented PKs. The absolute schema-tip
/// pin lives on the newest migration's test; this one uses only symbolic latest checks.
#[test]
fn migration_046_creates_the_verification_tables_on_fresh_apply() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    let table_sql = |table: &str| -> String {
        conn.query_row("SELECT sql FROM sqlite_master WHERE name = ?1", [table], |r| r.get(0))
            .unwrap()
    };
    let pk_cols = |table: &str| -> Vec<String> {
        conn.prepare(&format!(
            "SELECT name FROM pragma_table_info('{table}') WHERE pk > 0 ORDER BY pk"
        ))
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    for table in ["memory_reality", "memory_summaries"] {
        assert!(conn_table_exists(&conn, table), "{table} created on fresh apply");
        assert!(table_sql(table).contains("STRICT"), "{table} is STRICT");
        assert!(
            conn_table_columns(&conn, table).contains(&"repo_id".to_string()),
            "{table} carries repo_id"
        );
    }
    assert_eq!(
        pk_cols("memory_reality"),
        vec!["repo_id".to_string(), "memory_id".to_string()],
        "memory_reality is keyed (repo_id, memory_id)"
    );
    assert_eq!(
        pk_cols("memory_summaries"),
        vec!["repo_id".to_string(), "memory_id".to_string(), "content_hash".to_string()],
        "memory_summaries is keyed (repo_id, memory_id, content_hash) so a body edit \
         self-invalidates"
    );

    // Re-apply is a no-op (the memory_reality existence sentinel short-circuits).
    schema::apply(&conn).unwrap();
    assert!(conn_table_exists(&conn, "memory_reality"), "tables survive a re-apply");
}

/// V047: fresh `schema::apply` creates `memory_model_failures`, the dream model-failure sibling
/// table. Carries the absolute schema-tip pin now that V047 is newest.
#[test]
fn migration_047_creates_the_model_failure_table_on_fresh_apply() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // The absolute-tip pin moved to `migration_048_*` (V048 is the tip now); this test keeps the
    // symbolic "schema at LATEST after apply" check and its V047 table coverage.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    let table_sql = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'memory_model_failures'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap();
    assert!(table_sql.contains("STRICT"), "memory_model_failures is STRICT");
    for column in [
        "memory_id",
        "repo_id",
        "pass",
        "content_hash",
        "checked_inputs_hash",
        "model_id",
        "prompt_version",
        "reason",
        "failed_at_ms",
        "attempts",
    ] {
        assert!(
            conn_table_columns(&conn, "memory_model_failures").contains(&column.to_string()),
            "memory_model_failures carries {column}"
        );
    }
    let pk_cols: Vec<String> = conn
        .prepare(
            "SELECT name FROM pragma_table_info('memory_model_failures') WHERE pk > 0 ORDER BY pk",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        pk_cols,
        vec!["repo_id".to_string(), "memory_id".to_string(), "pass".to_string()],
        "one current failure row per repo/memory/pass"
    );

    schema::apply(&conn).unwrap();
    assert!(conn_table_exists(&conn, "memory_model_failures"), "failure table survives a re-apply");
}

#[test]
fn migration_048_adds_the_memory_payload_json_column() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // Symbolic freshness check — the absolute tip pin lives on the NEWEST migration's test
    // (migration_050_*), per the ladder convention.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    assert!(
        conn_table_columns(&conn, "repo_memories").contains(&"payload_json".to_string()),
        "repo_memories carries the payload_json column (#465)"
    );
    // Additive + nullable: a re-apply is idempotent and the column survives.
    schema::apply(&conn).unwrap();
    assert!(
        conn_table_columns(&conn, "repo_memories").contains(&"payload_json".to_string()),
        "payload_json survives a re-apply"
    );
}

#[test]
fn migration_049_adds_the_repo_node_edges_table() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // Symbolic freshness check — the absolute tip pin lives on the NEWEST migration's test
    // (migration_050_*), per the ladder convention.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    let table_sql = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'repo_node_edges'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap();
    assert!(table_sql.contains("STRICT"), "repo_node_edges is STRICT");
    for column in [
        "edge_key",
        "repo_id",
        "source_node_id",
        "relation",
        "target_repo_id",
        "target_kind",
        "target_anchor",
        "target_node_id",
        "target_logical_symbol_id",
        "anchor_status",
        "created_at_ms",
    ] {
        assert!(
            conn_table_columns(&conn, "repo_node_edges").contains(&column.to_string()),
            "repo_node_edges carries {column}"
        );
    }
    let pk_cols: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('repo_node_edges') WHERE pk > 0 ORDER BY pk")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(pk_cols, vec!["edge_key".to_string()], "edge_key is the stable PK");

    // Idempotent re-apply.
    schema::apply(&conn).unwrap();
    assert!(conn_table_exists(&conn, "repo_node_edges"), "edge table survives a re-apply");
}

#[test]
fn migration_050_adds_the_postings_path_index_and_delta_counter() {
    let index_exists = |conn: &rusqlite::Connection| -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = \
             'idx_clone_subblock_postings_path'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    };
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // Symbolic freshness check (the absolute tip pin lives on the NEWEST migration's test —
    // migration_051 — per the ladder convention).
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    assert!(
        index_exists(&conn),
        "the delta pass deletes a changed file's postings by (build_generation, path); the PK \
         leads with token_hash, so that delete needs this index (#473)"
    );
    assert!(
        conn_table_columns(&conn, "clone_graph_generations")
            .contains(&"delta_files_applied".to_string()),
        "generations carry the delta-drift counter that schedules the next full rebuild (#473)"
    );
    // Additive + defaulted: a re-apply is idempotent and both survive — and a POST-freeze
    // generation's postings are NOT invalidated by a re-apply (the pre-freeze invalidation below
    // is gated on the delta column being freshly added).
    conn.execute(
        "INSERT INTO clone_graph_generations
            (generation, status, theta_floor, normalizer_kind, normalizer_version,
             source_revision, started_at_ms, postings_written)
         VALUES (1, 'Complete', 0.7, 'baseline', 3, 'rev', 0, 1)",
        [],
    )
    .unwrap();
    schema::apply(&conn).unwrap();
    assert!(index_exists(&conn), "the postings path index survives a re-apply");
    assert!(
        conn_table_columns(&conn, "clone_graph_generations")
            .contains(&"delta_files_applied".to_string()),
        "delta_files_applied survives a re-apply"
    );
    let postings_written: i64 = conn
        .query_row(
            "SELECT postings_written FROM clone_graph_generations WHERE generation = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(postings_written, 1, "a re-apply must not invalidate an already-frozen graph");

    // PRE-freeze upgrade (#477 review): a generation from before the df epoch freeze may carry
    // postings ordered by an older df than the current table (incremental bumps moved df without
    // invalidating). The first V050 run — recognized by the delta column being absent — must
    // clear `postings_written` so those generations take one full rebuild instead of being
    // treated as delta-ready.
    conn.execute_batch("ALTER TABLE clone_graph_generations DROP COLUMN delta_files_applied;")
        .unwrap();
    schema::apply_clone_delta_maintenance(&conn).unwrap();
    let postings_written: i64 = conn
        .query_row(
            "SELECT postings_written FROM clone_graph_generations WHERE generation = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(postings_written, 0, "a pre-freeze generation is forced through a full rebuild");
}

#[test]
fn migration_051_adds_clone_df_epoch_and_backfills_existing_generations() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // V051 is the schema tip — the absolute pin (migration_050's drops to the symbolic
    // `current_version == LATEST` check when this lands).
    assert_eq!(schema::LATEST_SCHEMA_VERSION, 51, "V051 is the schema tip");
    assert_eq!(schema::status(&conn).unwrap().current_version, 51, "schema at LATEST after apply");
    assert!(
        conn_table_exists(&conn, "clone_df_epoch"),
        "the per-generation df snapshot table exists (#479)"
    );

    // BACKFILL (the V050→V051 upgrade bridge): a pre-epoch-table DB holds generations whose
    // postings are ordered by the CURRENT clone_token_df (the #473 freeze guarantees df has not
    // moved since any servable generation's build), so snapshotting current df per generation is
    // exact. Seed a V050-shaped state, re-run the applier in isolation (deferred-absence
    // pattern), and assert the snapshot: matching (repo_id, normalizer_kind) rows only.
    conn.execute(
        "INSERT INTO clone_graph_generations
            (generation, status, theta_floor, normalizer_kind, normalizer_version,
             source_revision, started_at_ms, postings_written, repo_id)
         VALUES (7, 'Complete', 0.7, 'baseline', 3, 'rev', 0, 1, 'r1')",
        [],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO clone_token_df(repo_id, normalizer_kind, token_hash, df)
         VALUES ('r1', 'baseline', 101, 3), ('r1', 'baseline', 102, 9);
         -- Different repo / different normalizer kind: must NOT enter generation 7's epoch.
         INSERT INTO clone_token_df(repo_id, normalizer_kind, token_hash, df)
         VALUES ('r2', 'baseline', 103, 5), ('r1', 'other', 104, 2);",
    )
    .unwrap();
    conn.execute_batch("DROP TABLE clone_df_epoch;").unwrap();
    schema::apply_clone_df_epoch(&conn).unwrap();
    let epoch_rows = |conn: &rusqlite::Connection| -> Vec<(i64, i64, i64)> {
        conn.prepare(
            "SELECT build_generation, token_hash, df FROM clone_df_epoch
             ORDER BY build_generation, token_hash",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    assert_eq!(
        epoch_rows(&conn),
        vec![(7, 101, 3), (7, 102, 9)],
        "backfill snapshots exactly the generation's own (repo_id, normalizer_kind) df rows"
    );

    // Idempotence: a re-apply must not re-snapshot a generation that already has epoch rows —
    // post-V051, current df moves on incremental passes, so a re-run folding NEW df rows into an
    // existing epoch would corrupt the frozen order.
    conn.execute(
        "INSERT INTO clone_token_df(repo_id, normalizer_kind, token_hash, df)
         VALUES ('r1', 'baseline', 105, 1)",
        [],
    )
    .unwrap();
    schema::apply_clone_df_epoch(&conn).unwrap();
    assert_eq!(
        epoch_rows(&conn),
        vec![(7, 101, 3), (7, 102, 9)],
        "a generation with epoch rows is never re-backfilled"
    );

    // FK CASCADE: the epoch rows die with their generation row (same lifecycle as
    // clone_edges / clone_subblock_postings).
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         DELETE FROM clone_graph_generations WHERE generation = 7;",
    )
    .unwrap();
    assert_eq!(
        epoch_rows(&conn),
        Vec::<(i64, i64, i64)>::new(),
        "epoch rows CASCADE with the generation"
    );
}

#[test]
fn migration_047_deferred_absence_and_reconverges_from_torn_state() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE memory_model_failures(leftover INTEGER);").unwrap();
    assert!(
        !conn_table_columns(&conn, "memory_model_failures").contains(&"reason".to_string()),
        "torn table lacks the sentinel column"
    );

    schema::apply_memory_model_failures_table(&conn).unwrap();

    assert!(
        conn_table_columns(&conn, "memory_model_failures").contains(&"reason".to_string()),
        "V047 drops the torn scratch table and creates the real shape"
    );
    schema::apply_memory_model_failures_table(&conn).expect("replay is a no-op");
    conn.execute(
        "INSERT INTO memory_model_failures(memory_id, repo_id, pass, content_hash, model_id, \
         prompt_version, reason, failed_at_ms) VALUES \
         ('m','r','verify','h','model','prompt','fabricated_evidence',0)",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO memory_model_failures(memory_id, repo_id, pass, content_hash, model_id, \
             prompt_version, reason, failed_at_ms) VALUES \
             ('m','r','verify','h2','model','prompt','malformed_verdict',1)",
            [],
        )
        .is_err(),
        "PK(repo_id, memory_id, pass) rejects a second current failure for the same pass"
    );
    conn.execute(
        "INSERT INTO memory_model_failures(memory_id, repo_id, pass, content_hash, model_id, \
         prompt_version, reason, failed_at_ms) VALUES \
         ('m','r','compact','h','model','prompt','summary_guard_rejected',0)",
        [],
    )
    .expect("verify and compact failures are distinct pass rows");
}

/// V046 in ISOLATION against a bare connection: the sibling tables are ABSENT before the migration
/// runs (the deferred-absence assertion anchored to the migration DDL, NOT the full ladder — the
/// documented breakage class), it re-converges from a torn `memory_summaries` scratch table,
/// replays as a no-op, and its keys hold (a duplicate reality row violates the PK; a new
/// content_hash is a new summary row).
#[test]
fn migration_046_deferred_absence_and_reconverges_from_torn_state() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    // A crashed prior V046 pass could leave a partial `memory_summaries`; the migration's leading
    // DROP re-converges from it.
    conn.execute_batch("CREATE TABLE memory_summaries(leftover INTEGER);").unwrap();
    // Deferred-absence, anchored to the migration DDL in isolation (never against the full ladder,
    // whose end state always has the table): the sentinel table is absent before V046 runs.
    assert!(!conn_table_exists(&conn, "memory_reality"), "memory_reality absent before V046 runs");

    schema::apply_memory_verification_tables(&conn).unwrap();

    assert!(conn_table_exists(&conn, "memory_reality"), "V046 creates memory_reality");
    assert!(conn_table_exists(&conn, "memory_summaries"), "V046 creates memory_summaries");
    assert!(
        conn_table_columns(&conn, "memory_summaries").contains(&"repo_id".to_string()),
        "the torn scratch table was dropped and recreated with the real shape"
    );
    // Replay short-circuits on the sentinel.
    schema::apply_memory_verification_tables(&conn).expect("replay is a no-op");

    // memory_reality PK (repo_id, memory_id): one row per memory; a duplicate is rejected.
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_at_ms) VALUES \
         ('m','r','h',0)",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_at_ms) VALUES \
             ('m','r','h2',1)",
            [],
        )
        .is_err(),
        "PK(repo_id, memory_id) rejects a second reality row for the same memory"
    );
    // memory_summaries admits a second content_hash for the same memory (the self-invalidation
    // shape).
    conn.execute(
        "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, generated_at_ms) \
         VALUES ('m','r','h','s',0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, generated_at_ms) \
         VALUES ('m','r','h2','s2',0)",
        [],
    )
    .expect("a new content_hash is a distinct summary row");
}
