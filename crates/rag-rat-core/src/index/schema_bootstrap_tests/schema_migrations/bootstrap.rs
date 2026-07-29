use super::*;

#[test]
fn rebuild_bootstraps_sqlite_schema_for_empty_target_root() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();

    let config_root = rag_rat_base::test_scratch::canonical_config_root(root.to_path_buf());
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
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
    let sync_cursor_columns = table_columns(&db, "papertrail_sync_cursor");
    assert!(sync_cursor_columns.contains(&"high_mark_at".to_string()));
    assert!(sync_cursor_columns.contains(&"backfill_done".to_string()));
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

    let _ = fs::remove_dir_all(&root);
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

    let _ = fs::remove_dir_all(&root);
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

    let _ = fs::remove_dir_all(&root);
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

    let _ = fs::remove_dir_all(&root);
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

    let _ = fs::remove_dir_all(&root);
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

    let _ = fs::remove_dir_all(&root);
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

    let _ = fs::remove_dir_all(&root);
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

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn migrate_preserves_the_papertrail_cache() {
    // Whole-table `row_count` cache-total checks are single-repo by nature: they assert the
    // papertrail cache survived a schema migration. The poison sibling's scoped papertrail rows
    // would inflate the unscoped totals (production papertrail reads ARE scoped — the
    // multi_repo_scope leak matrix proves it), so opt this cache-total test out of the harness.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let (root, config) =
        markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    sync_from_refs_blocking(
        db.storage.connection(),
        &root,
        Some(&MockGitHubClient),
        false,
        &test_gh_ctx(),
    )
    .unwrap();
    // The mock change request stores ONE item + 4 unified comments; the mirror holds one row per
    // base row (no issue-shadow duplication).
    assert_eq!(row_count(&db, "papertrail_refs"), 1);
    assert_eq!(row_count(&db, "papertrail_items"), 1);
    assert_eq!(row_count(&db, "papertrail_comments"), 4);
    assert_eq!(row_count(&db, "papertrail_fts"), 5);
    db.storage
        .connection()
        .execute("DELETE FROM schema_version WHERE id = ?1", ["010_symbol_facts"])
        .unwrap();
    drop(db);

    let migrated = IndexDatabase::migrate(&config.database).unwrap();
    assert_eq!(migrated.state, schema::SchemaState::Compatible);
    let db = IndexDatabase::open(&config.database).unwrap();
    assert_eq!(row_count(&db, "papertrail_refs"), 1);
    assert_eq!(row_count(&db, "papertrail_items"), 1);
    assert_eq!(row_count(&db, "papertrail_comments"), 4);
    assert_eq!(row_count(&db, "papertrail_fts"), 5);
    let hits = db.papertrail_issue_search("sqlite", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item_key, "42");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn full_rebuild_preserves_the_papertrail_cache() {
    // Whole-table `row_count` cache-total checks are single-repo by nature: they assert the
    // papertrail cache survived a full rebuild. The poison sibling's scoped papertrail rows would
    // inflate the unscoped totals (production papertrail reads ARE scoped — the multi_repo_scope
    // leak matrix proves it), so opt this cache-total test out of the harness.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let (root, config) =
        markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    sync_from_refs_blocking(
        db.storage.connection(),
        &root,
        Some(&MockGitHubClient),
        false,
        &test_gh_ctx(),
    )
    .unwrap();
    assert_eq!(row_count(&db, "papertrail_items"), 1);
    assert_eq!(row_count(&db, "papertrail_fts"), 5);
    drop(db);

    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_eq!(row_count(&db, "papertrail_refs"), 1);
    assert_eq!(row_count(&db, "papertrail_items"), 1);
    assert_eq!(row_count(&db, "papertrail_comments"), 4);
    assert_eq!(row_count(&db, "papertrail_fts"), 5);
    let hits = db.papertrail_issue_search("sqlite", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item_key, "42");

    let _ = fs::remove_dir_all(&root);
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

    let _ = fs::remove_dir_all(&root);
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
    rag_rat_db::chunk_text_store::seed_chunk_text(
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

    let _ = fs::remove_dir_all(&root);
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
    // #585: it also names this binary's schema ceiling. (This fixture never recorded provenance —
    // no index_meta — so the "who migrated" clause is correctly absent; the defensive note yields
    // "".)
    assert!(err.contains(&format!("schema v{}", schema::LATEST_SCHEMA_VERSION)), "{err}");
    // The fleet hot-upgrade re-execs armed MCP servers only on Linux; elsewhere the message must
    // say running servers stay stale until their sessions restart.
    if cfg!(target_os = "linux") {
        assert!(!err.contains("hot-upgrade"), "{err}");
    } else {
        assert!(err.contains("do not hot-upgrade"), "{err}");
    }

    let _ = fs::remove_dir_all(&root);
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();

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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    truncate_schema_to(&conn, 50);

    conn.execute_batch(
        "
        CREATE TRIGGER fail_051_record BEFORE INSERT ON schema_version
        WHEN NEW.id = '051_clone_df_epoch'
        BEGIN SELECT RAISE(ABORT, 'injected mid-replay failure'); END;
        ",
    )
    .unwrap();
    assert!(
        schema::migrate_forward(&conn, &crate::index::migration_hooks()).is_err(),
        "the injected failure fails the migrate"
    );

    let status = schema::status(&conn).unwrap();
    assert_eq!(
        status.state,
        schema::SchemaState::Older,
        "a mid-replay failure must leave the schema Older (retryable), not Dirty: {}",
        status.message
    );

    // With the failure gone, the next migrate completes the pending step exactly once.
    conn.execute_batch("DROP TRIGGER fail_051_record;").unwrap();
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

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

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();

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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO schema_version(id, applied_at_ms, checksum, description)
         VALUES ('__dirty__', 1, '', 'partial migration in progress')",
        [],
    )
    .unwrap();
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Dirty);

    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    let status = schema::status(&conn).unwrap();
    assert_eq!(
        status.state,
        schema::SchemaState::Compatible,
        "a full apply must clear a stranded dirty marker: {}",
        status.message
    );
}

/// #501 review: the baseline replay must be DATA-PRESERVING, not just convergent — the pre-ladder
/// prototype conversion ran `DROP TABLE IF EXISTS chunk_summaries` unconditionally, so every
/// forward migrate wiped the current summaries (re-derivable only at model cost) on its way to
/// recreating the table empty. Destructive legacy conversions may fire only when the LEGACY shape
/// is actually present.
#[test]
fn forward_migrate_replay_preserves_chunk_summaries() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // Seed a bare summary row (FK off — the parent chunk chain is irrelevant to what this test
    // pins: the replay must not touch the rows).
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         INSERT INTO chunk_summaries(chunk_id, model_id, prompt_version, input_hash, text_hash,
                                     summary, status)
         VALUES (1, 'model', 'v1', 'ih', 'th', 'a summary worth keeping', 'Current');",
    )
    .unwrap();
    truncate_schema_to(&conn, 50);

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();

    let kept: i64 =
        conn.query_row("SELECT COUNT(*) FROM chunk_summaries", [], |row| row.get(0)).unwrap();
    assert_eq!(kept, 1, "a baseline replay must not wipe current-shape chunk summaries");
}

/// #501 review companion: the legacy-prototype conversion still fires where it should — a
/// pre-ladder DB whose `chunk_summaries` is the ORIGINAL single-summary shape (chunk_id PK, no
/// `prompt_version`) gets it replaced with the current shape, and the prototype `embeddings`
/// table is removed.
#[test]
fn baseline_replay_still_converts_the_legacy_prototype_ai_tables() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "
        CREATE TABLE embeddings(chunk_id INTEGER PRIMARY KEY, vector BLOB);
        CREATE TABLE chunk_summaries(
            chunk_id INTEGER PRIMARY KEY,
            model_id TEXT NOT NULL,
            source_text_hash TEXT NOT NULL,
            summary TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            last_error TEXT
        );
        ",
    )
    .unwrap();

    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    assert!(!conn_table_exists(&conn, "embeddings"), "the prototype embeddings table is dropped");
    assert!(
        conn_table_columns(&conn, "chunk_summaries").contains(&"prompt_version".to_string()),
        "the legacy chunk_summaries shape is replaced with the current one"
    );
}

/// #498: a failed FIRST-EVER provision (001 never recorded) keeps the crash-detectability
/// contract — the DB reads Dirty and the marker row records the failure, so a torn fresh
/// bootstrap refuses to serve until `index --full` re-runs it.
#[test]
fn a_failed_first_provision_reads_dirty_with_the_failure_recorded() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    // Poison the bootstrap: a pre-existing wrong-shape `files` table no-ops the baseline's
    // `CREATE TABLE IF NOT EXISTS files` and then fails its index build (`no such column`).
    conn.execute_batch("CREATE TABLE files(id INTEGER PRIMARY KEY);").unwrap();

    assert!(
        schema::apply(&conn, &crate::index::migration_hooks()).is_err(),
        "the poisoned baseline fails the first provision"
    );

    let status = schema::status(&conn).unwrap();
    assert_eq!(status.state, schema::SchemaState::Dirty);
    let description: String = conn
        .query_row("SELECT description FROM schema_version WHERE id = '__dirty__'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        description.contains("partial migration failed"),
        "the marker records the failure: {description}"
    );
}

/// #498 review: a Dirty state can also come from a CHECKSUM-MISMATCHED baseline row (not just a
/// stranded marker), and `index --full` must recover that too — apply re-records every additive
/// step's checksum unconditionally, and the baseline provision must treat a stale 001 checksum
/// as "provision owed" so the 001 row is refreshed as well.
#[test]
fn apply_recovers_a_baseline_checksum_mismatch() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "UPDATE schema_version SET checksum = 'sha256:corrupted'
         WHERE id = '001_sqlite_storage_baseline'",
        [],
    )
    .unwrap();
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Dirty);

    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

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
        schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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

    let _ = fs::remove_dir_all(&root);
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

    let _ = fs::remove_dir_all(&root);
}
