use super::*;

#[test]
fn rebuild_bootstraps_sqlite_schema_for_empty_target_root() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();

    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
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

    let _ = fs::remove_dir_all(root);
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert!(conn_table_exists(&conn, "memory_reality"), "tables survive a re-apply");
}

/// V047: fresh `schema::apply` creates `memory_model_failures`, the dream model-failure sibling
/// table. Carries the absolute schema-tip pin now that V047 is newest.
#[test]
fn migration_047_creates_the_model_failure_table_on_fresh_apply() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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

    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert!(conn_table_exists(&conn, "memory_model_failures"), "failure table survives a re-apply");
}

#[test]
fn migration_048_adds_the_memory_payload_json_column() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert!(
        conn_table_columns(&conn, "repo_memories").contains(&"payload_json".to_string()),
        "payload_json survives a re-apply"
    );
}

#[test]
fn migration_049_adds_the_repo_node_edges_table() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // V052 now holds the absolute tip pin (migration_052's test); this drops to the symbolic
    // `current_version == LATEST` freshness check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
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
fn migration_052_adds_oplog_storage_tables() {
    const OPLOG_TABLES: [&str; 4] =
        ["oplog_entries", "oplog_projected_nodes", "oplog_projected_edges", "oplog_meta"];
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // V053 now holds the absolute tip pin (migration_053's test); this drops to the symbolic
    // `current_version == LATEST` freshness check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    for table in OPLOG_TABLES {
        assert!(conn_table_exists(&conn, table), "V052 creates {table}");
    }

    // Deferred-absence in ISOLATION: drop the tables and re-run the applier alone (never against
    // the full ladder's prior state). It recreates them, and a second run is a no-op (CREATE …
    // IF NOT EXISTS), matching the replay-write-free discipline.
    conn.execute_batch(
        "DROP TABLE oplog_entries;
         DROP TABLE oplog_projected_nodes;
         DROP TABLE oplog_projected_edges;
         DROP TABLE oplog_meta;",
    )
    .unwrap();
    assert!(!conn_table_exists(&conn, "oplog_entries"), "dropped before the isolated apply");
    schema::apply_oplog_storage(&conn).unwrap();
    schema::apply_oplog_storage(&conn).expect("replay is a no-op");
    for table in OPLOG_TABLES {
        assert!(conn_table_exists(&conn, table), "the isolated applier recreates {table}");
    }
}

#[test]
fn migration_053_scopes_the_oplog_by_stream_and_adds_fork_evidence() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // V054 now holds the absolute tip pin (migration_054's test); this drops to the symbolic
    // `current_version == LATEST` freshness check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    // The rebuilt tables carry the stream dimension; the quarantine table exists.
    for (table, column) in [
        ("oplog_entries", "stream_id"),
        ("oplog_projected_nodes", "stream_id"),
        ("oplog_projected_edges", "stream_id"),
        ("oplog_fork_evidence", "conflicting_entry_hash"),
    ] {
        assert!(
            conn_table_columns(&conn, table).contains(&column.to_string()),
            "V053 gives {table} its {column} column"
        );
    }

    // One chain slot per (stream, device, lamport): the same (device, lamport) is legal on two
    // DIFFERENT streams, and an equivocation within one stream trips the UNIQUE tripwire.
    conn.execute_batch(
        "INSERT INTO oplog_entries VALUES (x'01', x'aa', x'dd', 1, NULL, x'00', 0);
         INSERT INTO oplog_entries VALUES (x'02', x'bb', x'dd', 1, NULL, x'00', 0);",
    )
    .expect("the same (device, lamport) slot on two streams is two distinct chains");
    assert!(
        conn.execute(
            "INSERT INTO oplog_entries VALUES (x'03', x'aa', x'dd', 1, NULL, x'00', 0)",
            [],
        )
        .is_err(),
        "UNIQUE(stream_id, device_fingerprint, lamport) rejects a same-stream slot collision"
    );

    // Deferred-absence in ISOLATION: reduce the tables to the V052 shape (no stream_id, no
    // quarantine) and re-run the applier alone — it rebuilds the stream-scoped shape, and a
    // replay reconverges (the rebuild is safe precisely because the log is un-wired and empty;
    // see the applier's invariant comment).
    conn.execute_batch(
        "DROP TABLE oplog_entries;
         DROP TABLE oplog_projected_nodes;
         DROP TABLE oplog_projected_edges;
         DROP TABLE oplog_fork_evidence;
         CREATE TABLE oplog_entries(entry_hash BLOB PRIMARY KEY) STRICT;",
    )
    .unwrap();
    schema::apply_oplog_stream_scoping(&conn).unwrap();
    schema::apply_oplog_stream_scoping(&conn).expect("replay reconverges");
    for table in
        ["oplog_entries", "oplog_projected_nodes", "oplog_projected_edges", "oplog_fork_evidence"]
    {
        assert!(
            conn_table_columns(&conn, table).contains(&"stream_id".to_string()),
            "the isolated applier rebuilds {table} stream-scoped"
        );
    }
    assert!(conn_table_exists(&conn, "oplog_meta"), "oplog_meta is left untouched");
}

#[test]
fn migration_054_adds_the_single_row_device_identity_table() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // The absolute-tip pin moved to `migration_055_*` (V055 is the tip now); this drops to the
    // symbolic check, per the ladder convention.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    // The identity table exists with its full column set.
    for column in ["seed", "public_key", "fingerprint", "created_at_ms"] {
        assert!(
            conn_table_columns(&conn, "oplog_device_identity").contains(&column.to_string()),
            "V054 gives oplog_device_identity its {column} column"
        );
    }

    // `CHECK (id = 0)` + the primary key make it a strict single-row table: id 0 inserts once; a
    // non-zero id is refused by the CHECK; a second id-0 insert is refused by the PK. (Columns are
    // named because V058 added the nullable x25519 columns — a positional 5-value insert no longer
    // matches the 7-column table.)
    conn.execute(
        "INSERT INTO oplog_device_identity(id, seed, public_key, fingerprint, created_at_ms)
         VALUES (0, x'00', x'11', x'22', 0)",
        [],
    )
    .expect("the sole id=0 identity row inserts");
    assert!(
        conn.execute(
            "INSERT INTO oplog_device_identity(id, seed, public_key, fingerprint, created_at_ms)
             VALUES (1, x'00', x'11', x'22', 0)",
            [],
        )
        .is_err(),
        "CHECK (id = 0) rejects a second, non-zero identity"
    );
    assert!(
        conn.execute(
            "INSERT INTO oplog_device_identity(id, seed, public_key, fingerprint, created_at_ms)
             VALUES (0, x'99', x'88', x'77', 1)",
            [],
        )
        .is_err(),
        "the id=0 primary key rejects a second identity"
    );

    // Deferred-absence in ISOLATION: drop the table and re-run the applier alone (never against the
    // full ladder's end state). It recreates the table, and a replay is a no-op (CREATE … IF NOT
    // EXISTS).
    conn.execute_batch("DROP TABLE oplog_device_identity;").unwrap();
    assert!(
        !conn_table_exists(&conn, "oplog_device_identity"),
        "dropped before the isolated apply"
    );
    schema::apply_oplog_device_identity(&conn).unwrap();
    schema::apply_oplog_device_identity(&conn).expect("replay is a no-op");
    assert!(
        conn_table_columns(&conn, "oplog_device_identity").contains(&"seed".to_string()),
        "the isolated applier recreates the table"
    );
}

#[test]
fn migration_055_adds_the_binding_downgrade_marker_column() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // The absolute tip pin lives with the newest migration's test (`migration_057_*` now); this
    // drops to the symbolic `current_version == LATEST` freshness check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    assert!(
        conn_table_columns(&conn, "repo_memory_bindings")
            .contains(&"downgrade_pending_at_ms".to_string()),
        "repo_memory_bindings carries the downgrade hysteresis marker (#492)"
    );
    // Additive + nullable: a re-apply is idempotent and the column survives.
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert!(
        conn_table_columns(&conn, "repo_memory_bindings")
            .contains(&"downgrade_pending_at_ms".to_string()),
        "downgrade_pending_at_ms survives a re-apply"
    );
    // A forward migrate over a ledger truncated below V055 replays the step and lands the
    // column (the standard lagging-index path).
    truncate_schema_to(&conn, 54);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip"
    );
}

#[test]
fn migration_056_adds_the_git_change_couplings_table() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // The absolute tip pin lives with the newest migration's test (`migration_057_*` now); this
    // drops to the symbolic `current_version == LATEST` freshness check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    assert!(
        conn_table_exists(&conn, "git_change_couplings"),
        "V056 creates the git_change_couplings table"
    );

    // STRICT + composite (repo_id, path_a, path_b) PK: a duplicate pair is rejected.
    conn.execute(
        "INSERT INTO git_change_couplings(repo_id, path_a, path_b, co_change_count, \
         path_a_change_count, path_b_change_count, window_commit_count, last_co_change_at_s, \
         computed_at_ms) VALUES ('r', 'a.rs', 'b.rs', 2, 3, 4, 10, 100, 1)",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO git_change_couplings(repo_id, path_a, path_b, co_change_count, \
             path_a_change_count, path_b_change_count, window_commit_count, last_co_change_at_s, \
             computed_at_ms) VALUES ('r', 'a.rs', 'b.rs', 9, 9, 9, 9, 9, 9)",
            [],
        )
        .is_err(),
        "the composite PK rejects a duplicate (repo_id, path_a, path_b)"
    );

    // Deferred-absence in ISOLATION: drop + re-run the applier alone; it recreates, replay is a
    // no-op.
    conn.execute_batch("DROP TABLE git_change_couplings;").unwrap();
    assert!(!conn_table_exists(&conn, "git_change_couplings"), "dropped before the isolated apply");
    schema::apply_git_change_couplings(&conn).unwrap();
    schema::apply_git_change_couplings(&conn).expect("replay is a no-op");
    assert!(
        conn_table_exists(&conn, "git_change_couplings"),
        "the isolated applier recreates the table"
    );

    // A forward migrate over a ledger truncated below V056 replays the step and reaches the tip.
    truncate_schema_to(&conn, 55);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip"
    );
}

#[test]
fn migration_057_adds_the_external_symbols_table() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // The absolute tip pin moved to `migration_058_*` (V058 is the tip now); this drops to the
    // symbolic `current_version == LATEST` check, per the ladder convention.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    // The external-symbol contract table exists with its full column set.
    for column in [
        "repo_id",
        "tool",
        "tool_version",
        "commit_sha",
        "worktree_id",
        "moniker",
        "kind",
        "display_name",
        "signature_text",
        "signature_language",
        "documentation",
        "deprecated",
        "computed_at_ms",
    ] {
        assert!(
            conn_table_columns(&conn, "external_symbols").contains(&column.to_string()),
            "V057 gives external_symbols its {column} column"
        );
    }

    // PK `(repo_id, tool, commit_sha, worktree_id, moniker)`: a second row with the same key is
    // rejected even when the payload differs; the SAME moniker under a DIFFERENT checkout inserts
    // (the multi-worktree isolation), as does a distinct moniker.
    let insert = "INSERT INTO external_symbols(repo_id, tool, tool_version, commit_sha, \
                  worktree_id, moniker, kind, display_name, signature_text, signature_language, \
                  documentation, deprecated, computed_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, \
                  ?8, ?9, ?10, ?11, ?12, ?13)";
    conn.execute(insert, rusqlite::params![
        "r",
        "rust-analyzer",
        "1.0",
        "sha1",
        "",
        "crate a 1.0 mod/get().",
        "Function",
        "get",
        "fn get()",
        "rust",
        "docs",
        0,
        123_i64
    ])
    .expect("first external-symbol row inserts");
    assert!(
        conn.execute(insert, rusqlite::params![
            "r",
            "rust-analyzer",
            "2.0",
            "sha1",
            "",
            "crate a 1.0 mod/get().",
            "Method",
            "get",
            "fn get(x)",
            "rust",
            "other",
            1,
            456_i64
        ])
        .is_err(),
        "the (repo_id, tool, commit_sha, worktree_id, moniker) primary key rejects a duplicate"
    );
    conn.execute(insert, rusqlite::params![
        "r",
        "rust-analyzer",
        "1.0",
        "sha2",
        "",
        "crate a 1.0 mod/get().",
        "Function",
        "get",
        "fn get()",
        "rust",
        "docs",
        0,
        123_i64
    ])
    .expect("the same moniker under a different checkout (commit_sha) inserts — worktree-isolated");
    conn.execute(insert, rusqlite::params![
        "r",
        "rust-analyzer",
        "1.0",
        "sha1",
        "",
        "crate a 1.0 mod/other().",
        "Function",
        "other",
        "fn other()",
        "rust",
        "docs",
        0,
        123_i64
    ])
    .expect("a distinct moniker inserts");

    // Deferred-absence in ISOLATION: drop the table and re-run the applier alone (never against the
    // full ladder's end state). It recreates the table, and a replay is a no-op (CREATE … IF NOT
    // EXISTS).
    conn.execute_batch("DROP TABLE external_symbols;").unwrap();
    assert!(!conn_table_exists(&conn, "external_symbols"), "dropped before the isolated apply");
    schema::apply_external_symbols(&conn).unwrap();
    schema::apply_external_symbols(&conn).expect("replay is a no-op");
    assert!(
        conn_table_columns(&conn, "external_symbols").contains(&"moniker".to_string()),
        "the isolated applier recreates the table"
    );

    // A forward migrate over a ledger truncated below V057 replays the step and lands the table.
    conn.execute_batch("DROP TABLE external_symbols;").unwrap();
    truncate_schema_to(&conn, 56);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip"
    );
    assert!(
        conn_table_exists(&conn, "external_symbols"),
        "the forward migrate re-creates external_symbols"
    );
}

#[test]
fn migration_058_adds_the_oplog_device_x25519_columns() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // V058 is no longer the tip — symbolic tip check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    // The identity table gains the X25519 encryption columns (sync phase C, §5).
    for column in ["x25519_secret", "x25519_public"] {
        assert!(
            conn_table_columns(&conn, "oplog_device_identity").contains(&column.to_string()),
            "V058 gives oplog_device_identity its {column} column"
        );
    }

    // Deferred-absence in ISOLATION: build the table from the V054 DDL ALONE (never the full
    // ladder's end state) — the x25519 columns are absent — then the V058 applier adds them, and a
    // replay is an idempotent no-op (add_column_if_missing).
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_oplog_device_identity(&isolated).unwrap();
    for column in ["x25519_secret", "x25519_public"] {
        assert!(
            !conn_table_columns(&isolated, "oplog_device_identity").contains(&column.to_string()),
            "the V054 table alone lacks the {column} column"
        );
    }
    schema::apply_oplog_device_x25519(&isolated).unwrap();
    schema::apply_oplog_device_x25519(&isolated).expect("replay is a no-op");
    for column in ["x25519_secret", "x25519_public"] {
        assert!(
            conn_table_columns(&isolated, "oplog_device_identity").contains(&column.to_string()),
            "the isolated V058 applier adds the {column} column"
        );
    }

    // A forward migrate over a ledger truncated below V058 replays the step and keeps the columns.
    truncate_schema_to(&conn, 57);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip"
    );
    for column in ["x25519_secret", "x25519_public"] {
        assert!(
            conn_table_columns(&conn, "oplog_device_identity").contains(&column.to_string()),
            "the forward migrate keeps the {column} column"
        );
    }
}

#[test]
fn migration_059_creates_the_account_candidate_dag() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // V059 is no longer the tip (the absolute pin moved to the V060 papertrail test).
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    // The candidate-DAG tables + indexes exist (sync phase C, §16.1).
    for table in ["account_entries", "account_entry_status", "account_pre_verify"] {
        assert!(conn_table_exists(&conn, table), "V059 creates {table}");
    }
    assert!(conn_index_exists(&conn, "account_entries_chain"), "V059 creates the chain index");
    assert!(
        conn_index_exists(&conn, "account_accepted_slot"),
        "V059 creates the accepted-slot partial unique index (I10a)"
    );
    assert!(
        conn_index_exists(&conn, "account_pre_verify_account"),
        "V059 creates the pre-verify claimed_account_id index"
    );
    let pre_verify_columns = conn_table_columns(&conn, "account_pre_verify");
    assert!(pre_verify_columns.contains(&"signed_hash".to_string()));
    assert!(pre_verify_columns.contains(&"entry_hash".to_string()));

    // Deferred-absence in ISOLATION: a bare DB lacks the tables until the V059 applier runs, and a
    // replay is an idempotent no-op (every statement is CREATE ... IF NOT EXISTS).
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    assert!(!conn_table_exists(&isolated, "account_entries"), "bare DB lacks account_entries");
    schema::apply_account_candidate_dag(&isolated).unwrap();
    schema::apply_account_candidate_dag(&isolated).expect("replay is a no-op");
    for table in ["account_entries", "account_entry_status", "account_pre_verify"] {
        assert!(conn_table_exists(&isolated, table), "the isolated V059 applier creates {table}");
    }
    assert!(conn_index_exists(&isolated, "account_accepted_slot"), "and the partial unique index");

    // A forward migrate over a ledger truncated below V059 replays the step and keeps the tables.
    truncate_schema_to(&conn, 58);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip"
    );
    assert!(
        conn_table_exists(&conn, "account_entries"),
        "the forward migrate keeps account_entries"
    );
}

#[test]
fn migration_064_creates_account_authority_shadow_tables() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    for table in [
        "account_auth_state",
        "account_roster_history",
        "account_owner_incarnations",
        "account_stream_ownership",
        "account_stream_grants",
        "account_stream_grant_cuts",
    ] {
        assert!(conn_table_exists(&conn, table), "V064 creates {table}");
    }
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_account_authority_projection(&isolated).unwrap();
    schema::apply_account_authority_projection(&isolated).expect("V064 replay is idempotent");
    assert!(conn_table_exists(&isolated, "account_auth_state"));
    let seq_type: String = isolated
        .query_row(
            "SELECT type FROM pragma_table_info('account_stream_grant_cuts') WHERE name = 'seq'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seq_type, "BLOB", "device cuts retain the full unsigned u64 domain");
    assert!(
        isolated
            .execute(
                "INSERT INTO account_stream_grant_cuts(
                     grant_id, owner_account_id, device_fingerprint, seq, entry_hash
                 ) VALUES (?1, ?1, ?1, ?2, ?1)",
                rusqlite::params![[0u8; 32].as_slice(), [0u8; 7].as_slice()],
            )
            .is_err(),
        "the fixed-width cut coordinate rejects corrupt stored values",
    );

    truncate_schema_to(&conn, 63);
    conn.execute_batch(
        "DROP TABLE account_stream_grant_cuts;
         DROP TABLE account_stream_grants;
         DROP TABLE account_stream_ownership;
         DROP TABLE account_owner_incarnations;
         DROP TABLE account_roster_history;
         DROP TABLE account_auth_state;",
    )
    .unwrap();
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    assert!(conn_table_exists(&conn, "account_auth_state"));
}

#[test]
fn migration_065_adds_historical_authority_boundaries() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert!(conn_table_exists(&conn, "account_roster_content_boundaries"));
    for table in ["account_roster_history", "account_owner_incarnations"] {
        for column in [
            "control_boundary",
            "control_seq",
            "control_hash",
            "secrets_boundary",
            "secrets_seq",
            "secrets_hash",
        ] {
            assert!(
                conn_table_columns(&conn, table).contains(&column.to_string()),
                "V065 adds {table}.{column}"
            );
        }
    }
    schema::apply_account_authority_boundaries(&conn).expect("V065 replay is idempotent");
}

#[test]
fn migration_066_adds_the_content_candidate_dag() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    for table in ["content_entries", "content_entry_status", "content_pre_verify"] {
        assert!(conn_table_exists(&conn, table), "V066 creates {table}");
    }
    let columns = conn_table_columns(&conn, "content_entries");
    for column in [
        "stream_id",
        "author_account_id",
        "device_fingerprint",
        "seq",
        "prev_hash",
        "grant_id",
        "roster_ref",
        "owner_auth_len",
        "author_auth_len",
        "accepted",
        "signed_bytes",
    ] {
        assert!(columns.contains(&column.to_string()), "V066 adds content_entries.{column}");
    }
    schema::apply_content_candidate_dag(&conn).expect("V066 replay is idempotent");
    let insert = |hash: u8, seq_width: usize| {
        conn.execute(
            "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?8, 0, ?9, 0)",
            rusqlite::params![
                vec![hash; 32],
                vec![1_u8; 32],
                vec![2_u8; 32],
                vec![3_u8; 32],
                vec![0_u8; seq_width],
                vec![4_u8; 32],
                vec![0_u8; 8],
                vec![0_u8; 8],
                vec![hash],
            ],
        )
    };
    assert!(insert(1, 7).is_err(), "V066 rejects truncated unsigned counters");
    insert(1, 8).unwrap();
    insert(2, 8).expect("equivocating candidates are first-class while unaccepted");
    conn.execute("UPDATE content_entries SET accepted = 1 WHERE entry_hash = ?1", [vec![1; 32]])
        .unwrap();
    assert!(
        conn.execute("UPDATE content_entries SET accepted = 1 WHERE entry_hash = ?1", [vec![
            2;
            32
        ]])
        .is_err(),
        "V066 permits at most one accepted candidate per dense slot"
    );
    truncate_schema_to(&conn, 65);
    conn.execute_batch(
        "DROP TABLE content_pre_verify;
         DROP TABLE content_entry_status;
         DROP TABLE content_entries;",
    )
    .unwrap();
    assert!(!conn_table_exists(&conn, "content_entries"));
    schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("V065 upgrades through V066");
    assert!(conn_index_exists(&conn, "content_entries_chain"));
    assert!(conn_index_exists(&conn, "content_entries_predecessor"));
    assert!(conn_index_exists(&conn, "content_accepted_slot"));
    assert!(conn_index_exists(&conn, "content_pre_verify_author"));
    schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("a second V066 forward migration is a no-op");
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
}

#[test]
fn migration_068_hides_suppressed_edge_candidates() {
    // The absolute-tip pin moved to `migration_069_*` (V069 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES ('App.swift', 'swift', 'source', 'sha', 0, 0, 'head', '')",
        [],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence) \
         VALUES (?1, 'Observable', 'uses_macro', 'NameOnly', 'suppressed', '@Observable')",
        [file_id],
    )
    .unwrap();
    truncate_schema_to(&conn, 67);
    let current_view: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'edges'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // The current view filters on the materialized `hidden` flag (V075); the pre-V068 view had
    // only the dispatch-fact exclusion, evaluated inline. Reconstruct that shape so the row (a
    // suppressed `uses_macro` candidate) is visible before the upgrade.
    let v67_view = current_view.replace(
        "WHERE d.hidden = 0",
        "WHERE d.edge_kind_id NOT IN (
            SELECT id FROM name_strings WHERE value IN ('dispatch_construct', 'dispatch_handle')
        )",
    );
    assert_ne!(v67_view, current_view, "fixture must remove the V068 public-edge filter");
    conn.execute_batch("DROP VIEW edges;").unwrap();
    conn.execute_batch(&v67_view).unwrap();
    let visible_before_upgrade: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0)).unwrap();
    assert_eq!(visible_before_upgrade, 1, "the V067 view exposes the retained candidate");

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    let raw_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges_data", [], |row| row.get(0)).unwrap();
    let visible_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0)).unwrap();
    assert_eq!(raw_count, 1, "suppressed candidates remain available to the resolver");
    assert_eq!(visible_count, 0, "suppressed candidates stay out of query-layer reads");
    let v68_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '068_suppressed_edge_candidates'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v68_recorded, 1, "the forward migration records V068");
}

#[test]
fn migration_069_adds_the_local_account_pointer() {
    // The absolute-tip pin moved to `migration_070_*` (V070 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    // The single-row pointer table exists with its full column set.
    for column in ["id", "genesis_entry_hash", "created_at_ms"] {
        assert!(
            conn_table_columns(&conn, "oplog_local_account").contains(&column.to_string()),
            "V069 gives oplog_local_account its {column} column",
        );
    }

    // `CHECK (id = 0)` + the primary key make it a strict single-row table: id 0 inserts once; a
    // non-zero id is refused by the CHECK; a second id-0 insert is refused by the PK. The
    // `length(genesis_entry_hash) = 32` CHECK rejects a wrong-width pointer.
    conn.execute(
        "INSERT INTO oplog_local_account(id, genesis_entry_hash, created_at_ms)
         VALUES (0, zeroblob(32), 0)",
        [],
    )
    .expect("the sole id=0 pointer row inserts");
    assert!(
        conn.execute(
            "INSERT INTO oplog_local_account(id, genesis_entry_hash, created_at_ms)
             VALUES (1, zeroblob(32), 0)",
            [],
        )
        .is_err(),
        "CHECK (id = 0) rejects a second, non-zero pointer",
    );
    assert!(
        conn.execute(
            "INSERT INTO oplog_local_account(id, genesis_entry_hash, created_at_ms)
             VALUES (0, zeroblob(32), 1)",
            [],
        )
        .is_err(),
        "the id=0 primary key rejects a second pointer",
    );
    assert!(
        conn.execute(
            "UPDATE oplog_local_account SET genesis_entry_hash = zeroblob(31) WHERE id = 0",
            [],
        )
        .is_err(),
        "the length(genesis_entry_hash) = 32 CHECK rejects a wrong-width hash",
    );

    // Deferred-absence in ISOLATION: a bare DB lacks the table until the V069 applier runs, and a
    // replay is an idempotent no-op (CREATE ... IF NOT EXISTS).
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    assert!(
        !conn_table_exists(&isolated, "oplog_local_account"),
        "bare DB lacks oplog_local_account before the isolated apply",
    );
    schema::apply_oplog_local_account(&isolated).unwrap();
    schema::apply_oplog_local_account(&isolated).expect("replay is a no-op");
    assert!(
        conn_table_columns(&isolated, "oplog_local_account")
            .contains(&"genesis_entry_hash".to_string()),
        "the isolated applier recreates the table",
    );

    // A forward migrate over a ledger truncated below V069 replays the step and records V069.
    truncate_schema_to(&conn, 68);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip",
    );
    let v69_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '069_oplog_local_account'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v69_recorded, 1, "the forward migration records V069");
}

#[test]
fn migration_070_adds_the_content_projected_tables() {
    // The absolute-tip pin moved to `migration_071_*` (V071 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    // Both /3 projection tables exist with their full column set, mirroring the stream-keyed /1
    // shadow tables (V053).
    for column in ["stream_id", "node_id", "content_json", "status"] {
        assert!(
            conn_table_columns(&conn, "content_projected_nodes").contains(&column.to_string()),
            "V070 gives content_projected_nodes its {column} column",
        );
    }
    for column in ["stream_id", "edge_key", "spec_json", "resolved_json"] {
        assert!(
            conn_table_columns(&conn, "content_projected_edges").contains(&column.to_string()),
            "V070 gives content_projected_edges its {column} column",
        );
    }

    // The primary key is the composite (stream_id, node_id) / (stream_id, edge_key): the SAME
    // (node_id / edge_key) may recur under a DIFFERENT stream, but a duplicate under one stream is
    // refused — the stream-keying that keeps two /2 streams' projections from colliding.
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
         VALUES (zeroblob(32), 'n1', '{}', 'active')",
        [],
    )
    .expect("first node row inserts");
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
         VALUES (randomblob(32), 'n1', '{}', 'active')",
        [],
    )
    .expect("the same node_id under a different stream is a distinct row");
    assert!(
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (zeroblob(32), 'n1', '{}', 'active')",
            [],
        )
        .is_err(),
        "the (stream_id, node_id) primary key rejects a duplicate within one stream",
    );

    // Deferred-absence in ISOLATION: a bare DB lacks the tables until the V070 applier runs, and a
    // replay is an idempotent no-op (CREATE ... IF NOT EXISTS).
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    assert!(
        !conn_table_exists(&isolated, "content_projected_nodes"),
        "bare DB lacks content_projected_nodes before the isolated apply",
    );
    schema::apply_content_projected_tables(&isolated).unwrap();
    schema::apply_content_projected_tables(&isolated).expect("replay is a no-op");
    assert!(
        conn_table_columns(&isolated, "content_projected_edges").contains(&"spec_json".to_string()),
        "the isolated applier recreates the tables",
    );

    // A forward migrate over a ledger truncated below V070 replays the step and records V070.
    truncate_schema_to(&conn, 69);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip",
    );
    let v70_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '070_content_projected_tables'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v70_recorded, 1, "the forward migration records V070");
}

#[test]
fn migration_071_indexes_edge_target_qname() {
    // The absolute-tip pin moved to `migration_072_*` (V072 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    let index_on = |conn: &rusqlite::Connection| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_edges_target_qname' AND tbl_name = 'edges_data'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(index_on(&conn), 1, "V071 creates idx_edges_target_qname on edges_data");

    // Idempotent applier: drop it, re-run twice — CREATE INDEX IF NOT EXISTS reconverges.
    conn.execute("DROP INDEX idx_edges_target_qname", []).unwrap();
    assert_eq!(index_on(&conn), 0, "index dropped");
    schema::apply_edge_target_qname_index(&conn).unwrap();
    schema::apply_edge_target_qname_index(&conn).expect("replay is a no-op");
    assert_eq!(index_on(&conn), 1, "the isolated applier recreates the index");

    // A forward migrate over a ledger truncated below V071 replays the step and records V071.
    truncate_schema_to(&conn, 70);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip",
    );
    let v71_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '071_edge_target_qname_index'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v71_recorded, 1, "the forward migration records V071");
}

#[test]
fn migration_072_queues_pending_refold() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    // The deferred-refold work queue exists with just its stream_id primary key.
    assert!(
        conn_table_columns(&conn, "content_streams_pending_refold")
            .contains(&"stream_id".to_string()),
        "V072 gives content_streams_pending_refold its stream_id column",
    );

    // PRIMARY KEY(stream_id) is what dedups repeat enqueues of one stream into a single queued
    // refold: a plain duplicate is refused, and the INSERT OR IGNORE the ingest path uses is a
    // no-op rather than an error.
    conn.execute("INSERT INTO content_streams_pending_refold(stream_id) VALUES (zeroblob(32))", [])
        .expect("first enqueue inserts");
    assert!(
        conn.execute(
            "INSERT INTO content_streams_pending_refold(stream_id) VALUES (zeroblob(32))",
            [],
        )
        .is_err(),
        "the stream_id primary key rejects a duplicate enqueue of one stream",
    );
    conn.execute(
        "INSERT OR IGNORE INTO content_streams_pending_refold(stream_id) VALUES (zeroblob(32))",
        [],
    )
    .expect("INSERT OR IGNORE dedups a repeat enqueue without erroring");

    // CHECK(length(stream_id) = 32) rejects a non-32-byte stream_id — a stream id is a sha256, so a
    // shorter/longer blob is corruption, matching every sibling 32-byte-blob column.
    assert!(
        conn.execute(
            "INSERT INTO content_streams_pending_refold(stream_id) VALUES (zeroblob(31))",
            [],
        )
        .is_err(),
        "a 31-byte stream_id violates the length CHECK",
    );

    // Deferred-absence in ISOLATION: a bare DB lacks the table until the V072 applier runs, and a
    // replay is an idempotent no-op (CREATE ... IF NOT EXISTS).
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    assert!(
        !conn_table_exists(&isolated, "content_streams_pending_refold"),
        "bare DB lacks content_streams_pending_refold before the isolated apply",
    );
    schema::apply_content_streams_pending_refold(&isolated).unwrap();
    schema::apply_content_streams_pending_refold(&isolated).expect("replay is a no-op");
    assert!(
        conn_table_columns(&isolated, "content_streams_pending_refold")
            .contains(&"stream_id".to_string()),
        "the isolated applier recreates the table",
    );

    // A forward migrate over a ledger truncated below V072 replays the step and records V072.
    truncate_schema_to(&conn, 71);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip",
    );
    let v72_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '072_content_streams_pending_refold'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v72_recorded, 1, "the forward migration records V072");
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

/// #585: bringing the schema current records WHO did it into the global `index_meta`, so a stranded
/// fleet is diagnosable in one query instead of forensics. Covers the `schema::apply` (create /
/// funnel-2) path.
#[test]
fn applying_the_schema_records_migration_provenance() {
    use rusqlite::OptionalExtension;

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    let read = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
            .optional()
            .unwrap()
    };

    assert_eq!(
        read("last_migration_to_version").as_deref(),
        Some(schema::LATEST_SCHEMA_VERSION.to_string().as_str()),
        "provenance records the schema version migrated TO"
    );
    assert_eq!(
        read("last_migration_binary_version").as_deref(),
        Some(rag_rat_base::version::binary_version()),
        "provenance records this binary's version string"
    );
    assert!(read("last_migration_binary_exe").is_some(), "provenance records the binary path");
    assert!(
        read("last_migration_at_ms").and_then(|value| value.parse::<i64>().ok()).unwrap_or(0) > 0,
        "provenance records a timestamp"
    );
}

/// #585: the `Newer`-schema refusal (what every stranded process prints) names this binary's schema
/// ceiling AND who last migrated the store, so the fleet outage is diagnosable from the error text.
#[test]
fn newer_schema_refusal_names_the_migrating_binary_and_ceiling() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap(); // stamps provenance for THIS binary
    // Fabricate a Newer schema: an applied migration this binary doesn't know.
    conn.execute(
        "INSERT INTO schema_version(id, applied_at_ms, checksum, description) VALUES \
         ('999_from_the_future', 0, 'sha256:future', 'a migration this binary lacks')",
        [],
    )
    .unwrap();

    let status = schema::status(&conn).unwrap();
    assert_eq!(status.state, schema::SchemaState::Newer);
    assert!(
        status.message.contains(rag_rat_base::version::binary_version()),
        "refusal should name the migrating binary version; got: {}",
        status.message
    );
    assert!(
        status.message.contains(&schema::LATEST_SCHEMA_VERSION.to_string()),
        "refusal should name this binary's schema ceiling; got: {}",
        status.message
    );
}

/// #585: a forward migration of an existing store (the `migrate_forward` funnel) also stamps
/// provenance — the stranding path, not just create.
#[test]
fn forward_migration_records_migration_provenance() {
    use rusqlite::OptionalExtension;

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // Roll back one migration and clear provenance, then forward-migrate.
    conn.execute(
        "DELETE FROM schema_version WHERE id = (SELECT id FROM schema_version ORDER BY id DESC \
         LIMIT 1)",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM index_meta WHERE key LIKE 'last_migration_%'", []).unwrap();
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();

    let to: Option<String> = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'last_migration_to_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(to.as_deref(), Some(schema::LATEST_SCHEMA_VERSION.to_string().as_str()));
}

#[test]
fn migration_073_builds_the_distill_substrate() {
    // The absolute-tip pin moved to `migration_074_*` (V074 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    // The closing-edge table exists with the full column set, repo_id included from birth.
    let cols = conn_table_columns(&conn, "papertrail_closing_edges");
    for col in [
        "tracker",
        "project",
        "issue_kind",
        "issue_key",
        "closer_kind",
        "closer_key",
        "closer_commit",
        "source",
        "synced_at_ms",
        "repo_id",
    ] {
        assert!(cols.contains(&col.to_string()), "V073 closing-edge column `{col}` exists");
    }
    // The natural key is the (issue, closer) PAIR — `source` is an attribute, so the same edge
    // discovered by the text tier and then the provider tier converges to one row instead of two.
    conn.execute(
        "INSERT INTO papertrail_closing_edges(tracker, project, issue_kind, issue_key, \
         closer_kind, closer_key, source, synced_at_ms, repo_id) VALUES ('github', 'o/r', \
         'issue', '5', 'change_request', '9', 'text', 1, 'r')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO papertrail_closing_edges(tracker, project, issue_kind, issue_key, \
             closer_kind, closer_key, source, synced_at_ms, repo_id) VALUES ('github', 'o/r', \
             'issue', '5', 'change_request', '9', 'provider', 2, 'r')",
            [],
        )
        .is_err(),
        "the natural key rejects a second row for the same (issue, closer) pair",
    );
    // A DIFFERENT closer for the same issue is a distinct edge (an issue can be closed by a
    // change request AND referenced by the closing commit).
    conn.execute(
        "INSERT INTO papertrail_closing_edges(tracker, project, issue_kind, issue_key, \
         closer_kind, closer_key, source, synced_at_ms, repo_id) VALUES ('github', 'o/r', \
         'issue', '5', 'commit', 'abc123', 'text', 1, 'r')",
        [],
    )
    .unwrap();

    // The item outcome columns exist on items; the author facets on comments too.
    let item_cols = conn_table_columns(&conn, "papertrail_items");
    for col in [
        "closed_at",
        "resolution",
        "merge_commit_sha",
        "state_normalized",
        "author_kind",
        "author_association",
    ] {
        assert!(item_cols.contains(&col.to_string()), "V073 item column `{col}` exists");
    }
    let comment_cols = conn_table_columns(&conn, "papertrail_comments");
    for col in ["author_kind", "author_association"] {
        assert!(comment_cols.contains(&col.to_string()), "V073 comment column `{col}` exists");
    }
}

#[test]
fn migration_073_backfills_state_normalized_from_the_provider_truthful_pair() {
    // The trap this column exists for: GitLab merged MRs carry state='merged', GitHub merged PRs
    // carry state='closed' + merged_at — a consumer filtering raw `WHERE state='closed'` silently
    // drops every merged GitLab MR. The backfill derives the normalized value for pre-V073 rows;
    // rerunning it is a no-op for already-stamped rows (idempotent replay).
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    let insert = |key: &str, state: &str, merged_at: Option<&str>| {
        conn.execute(
            "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, \
             title, body, merged_at, synced_at_ms, repo_id, state_normalized) VALUES ('github', \
             'o/r', 'change_request', ?1, 'u', ?2, 't', 'b', ?3, 1, 'r', '')",
            rusqlite::params![key, state, merged_at],
        )
        .unwrap();
    };
    insert("1", "closed", Some("2026-01-03T00:00:00Z")); // GitHub merged PR
    insert("2", "merged", None); // GitLab merged MR
    insert("3", "closed", None); // closed unmerged
    insert("4", "open", None);
    rag_rat_db::schema::apply_papertrail_distill_substrate(&conn).unwrap();
    let normalized = |key: &str| -> String {
        conn.query_row(
            "SELECT state_normalized FROM papertrail_items WHERE item_key = ?1",
            [key],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(normalized("1"), "merged", "merged_at wins over raw closed state");
    assert_eq!(normalized("2"), "merged", "GitLab's state='merged' normalizes to merged");
    assert_eq!(normalized("3"), "closed");
    assert_eq!(normalized("4"), "open");
    // Idempotent: a stamped row is untouched by a replay (the WHERE '' predicate skips it).
    conn.execute("UPDATE papertrail_items SET state_normalized = 'closed' WHERE item_key = '4'", [
    ])
    .unwrap();
    rag_rat_db::schema::apply_papertrail_distill_substrate(&conn).unwrap();
    assert_eq!(normalized("4"), "closed", "replay does not re-derive a stamped row");
}

#[test]
fn migration_074_refreshes_the_edges_view() {
    // The absolute-tip pin moved to `migration_076_*` (V076 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    // A DB that migrated through V068-V073 carries the ORIGINAL view text (per-row
    // `NOT IN (SELECT ...)` suppressed-edge probe). Simulate it: swap the current materialized
    // WHERE back to the historical inline predicates, truncate the ledger to V073, and seed one
    // suppressed candidate.
    let current_view: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'edges'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let old_view = current_view.replace(
        "WHERE d.hidden = 0",
        "WHERE d.edge_kind_id NOT IN (
            SELECT id FROM name_strings WHERE value IN ('dispatch_construct', 'dispatch_handle')
        )
        AND d.resolution_id NOT IN (
            SELECT id FROM name_strings WHERE value = 'suppressed'
        )",
    );
    assert_ne!(old_view, current_view, "fixture must reconstruct the pre-V074 clause");
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES ('App.swift', 'swift', 'source', 'sha', 0, 0, 'head', '')",
        [],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence) \
         VALUES (?1, 'Observable', 'uses_macro', 'NameOnly', 'suppressed', '@Observable')",
        [file_id],
    )
    .unwrap();
    truncate_schema_to(&conn, 73);
    conn.execute_batch("DROP VIEW edges;").unwrap();
    conn.execute_batch(&old_view).unwrap();

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    let refreshed_view: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'edges'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // The V074 refresh runs inside the same forward pass as V075, so the re-installed view is
    // the current shape: the materialized flag, no inline membership probes.
    assert!(
        refreshed_view.contains("WHERE d.hidden = 0"),
        "the forward pass re-installs the materialized-visibility view: {refreshed_view}"
    );
    // Semantics preserved across the refresh: the suppressed candidate stays in edges_data for
    // the resolver but never surfaces through the compatibility view.
    let raw_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges_data", [], |row| row.get(0)).unwrap();
    let visible_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0)).unwrap();
    assert_eq!(raw_count, 1, "suppressed candidates remain available to the resolver");
    assert_eq!(visible_count, 0, "suppressed candidates stay out of query-layer reads");
    let v74_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '074_edges_view_scalar_suppression'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v74_recorded, 1, "the forward migration records V074");
}

#[test]
fn migration_075_materializes_edge_visibility() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES ('App.swift', 'swift', 'source', 'sha', 0, 0, 'head', '')",
        [],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    // One row per visibility class: a suppressed candidate, an internal dispatch FACT, and a
    // public edge.
    for (to_name, edge_kind, resolution) in [
        ("Observable", "uses_macro", "suppressed"),
        ("Msg::Start", "dispatch_construct", "unresolved"),
        ("spawn_worker", "calls_name", "unresolved"),
    ] {
        conn.execute(
            "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution) VALUES \
             (?1, ?2, ?3, 'NameOnly', ?4)",
            rusqlite::params![file_id, to_name, edge_kind, resolution],
        )
        .unwrap();
    }

    // Simulate a pre-V075 DB: rows never carried the flag (zero it under the writers' backs) and
    // the view still evaluates the V074 scalar clause inline. The backfill — not the insert
    // trigger that already stamped these rows — must be what restores visibility.
    conn.execute("UPDATE edges_data SET hidden = 0", []).unwrap();
    let current_view: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'edges'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let v74_view = current_view.replace(
        "WHERE d.hidden = 0",
        "WHERE d.edge_kind_id NOT IN (
            SELECT id FROM name_strings WHERE value IN ('dispatch_construct', 'dispatch_handle')
        )
        AND d.resolution_id <> COALESCE(
            (SELECT id FROM name_strings WHERE value = 'suppressed'), -1
        )",
    );
    assert_ne!(v74_view, current_view, "fixture must reconstruct the V074 view shape");
    truncate_schema_to(&conn, 74);
    conn.execute_batch("DROP VIEW edges;").unwrap();
    conn.execute_batch(&v74_view).unwrap();

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();

    // The backfill re-derives the flag from the kind/resolution predicates.
    let hidden_for = |to_name: &str| -> i64 {
        conn.query_row(
            "SELECT hidden FROM edges_data
             WHERE to_name_id = (SELECT id FROM name_strings WHERE value = ?1)",
            [to_name],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(hidden_for("Observable"), 1, "suppressed candidates backfill hidden");
    assert_eq!(hidden_for("Msg::Start"), 1, "dispatch FACT rows backfill hidden");
    assert_eq!(hidden_for("spawn_worker"), 0, "public edges stay visible");

    // The refreshed view filters on the flag alone — the per-row kind/resolution machinery is
    // gone from the read path (the point of the migration).
    let refreshed_view: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'edges'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        refreshed_view.contains("WHERE d.hidden = 0"),
        "V075 installs the materialized-visibility filter: {refreshed_view}"
    );
    let visible: Vec<String> = conn
        .prepare("SELECT to_name FROM edges ORDER BY to_name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(visible, ["spawn_worker"], "only the public edge surfaces through the view");
    let v75_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '075_edges_hidden_flag'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v75_recorded, 1, "the forward migration records V075");
}

#[test]
fn migration_076_adds_sync_security_events() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    // Simulate a pre-V076 DB: the table's DDL is not part of truncate_schema_to (it rolls the
    // ledger only), so drop it, then roll the ledger back to V075.
    truncate_schema_to(&conn, 75);
    conn.execute_batch("DROP TABLE sync_security_events;").unwrap();

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();

    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = \
             'sync_security_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_exists, 1, "the forward migration re-creates sync_security_events");
    let dedup_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = \
             'sync_security_events_dedup'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dedup_index, 1, "the dedup unique index is installed");
    let v76_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '076_sync_security_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v76_recorded, 1, "the forward migration records V076");
}

#[test]
fn migration_077_builds_the_distill_record_store() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    // The record row carries the derived-facet columns + the raw status floors; repo_id from birth.
    let cols = conn_table_columns(&conn, "papertrail_distill");
    for col in [
        "tracker",
        "project",
        "item_kind",
        "item_key",
        "distill_input_hash",
        "pipeline_version",
        "prompt_version",
        "model_input_hash",
        "root_issue",
        "root_cause",
        "root_cause_class",
        "decision_chosen",
        "outcome_summary",
        "outcome_status_model",
        "epistemic_status_decision",
        "epistemic_status_outcome",
        "fix_edge_source",
        "quotes_materialized",
        "anchors_qualified_count",
        "thread_shape",
        "outcome_claim_verified",
        "decision_provenance_verified",
        "revert_override",
        "closing_keyword_floor",
        "distilled_at_ms",
        "repo_id",
    ] {
        assert!(cols.contains(&col.to_string()), "V077 distill column `{col}` exists");
    }

    // The record is keyed to the coalesced work-unit thread: one row per
    // (repo_id, tracker, project, item_kind, item_key). A regenerated body replaces in place.
    conn.execute(
        "INSERT INTO papertrail_distill(tracker, project, item_kind, item_key, \
         distill_input_hash, pipeline_version, fix_edge_source, thread_shape, distilled_at_ms, \
         repo_id) VALUES ('github', 'o/r', 'issue', '5', 'h1', 1, 'provider', 'investigation', 1, \
         'r')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO papertrail_distill(tracker, project, item_kind, item_key, \
             distill_input_hash, pipeline_version, fix_edge_source, thread_shape, \
             distilled_at_ms, repo_id) VALUES ('github', 'o/r', 'issue', '5', 'h2', 2, 'text', \
             'thin', 2, 'r')",
            [],
        )
        .is_err(),
        "the natural key holds one record per coalesced thread",
    );

    // The junction/companion tables all land in the same migration.
    for table in [
        "papertrail_distill_evidence",
        "papertrail_distill_anchors",
        "papertrail_distill_alternatives",
        "papertrail_distill_record_commits",
        "papertrail_distill_edges",
        "papertrail_distill_queue",
        "papertrail_distill_runs",
    ] {
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "V077 companion table `{table}` exists");
    }

    let v77_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '077_distill_record_store'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v77_recorded, 1, "the forward migration records V077");
}

#[test]
fn migration_078_distinguishes_candidates_from_selections() {
    #[derive(Debug, PartialEq, Eq)]
    struct UpgradedAnchor {
        item_key: String,
        candidate_ordinal: i64,
        selected: i64,
        logical_symbol_id: Option<String>,
        file_path: Option<String>,
    }

    // Exercise the real V077 -> V078 upgrade with existing rows. Row-id order is the deterministic
    // tie-break within each thread; a second thread starts its own zero-based ordinal sequence.
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_distill_record_store(&legacy).unwrap();
    legacy
        .execute_batch(
            "
            INSERT INTO papertrail_distill_anchors
                (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, file_path,
                 name, resolved, repo_id)
            VALUES
                ('github', 'o/r', 'issue', '5', 'file', NULL, 'src/widget.rs',
                 'src/widget.rs', 1, 'r'),
                ('github', 'o/r', 'issue', '5', 'symbol', 'sym_3e7', 'src/widget.rs',
                 'render_widget', 1, 'r'),
                ('github', 'o/r', 'issue', '6', 'file', NULL, 'src/other.rs',
                 'src/other.rs', 1, 'r');
            ",
        )
        .unwrap();
    // Simulate a crash after V078's first ADD COLUMN but before its backfill/index. The migration
    // must recognize the missing completion index and reconverge rather than keep three ordinal-0
    // rows and fail unique-index creation forever.
    legacy
        .execute_batch(
            "ALTER TABLE papertrail_distill_anchors ADD COLUMN candidate_ordinal
                 INTEGER NOT NULL DEFAULT 0 CHECK(candidate_ordinal >= 0);",
        )
        .unwrap();
    schema::apply_distill_anchor_selection(&legacy).unwrap();

    let upgraded: Vec<UpgradedAnchor> = legacy
        .prepare(
            "SELECT item_key, candidate_ordinal, selected, logical_symbol_id, file_path
             FROM papertrail_distill_anchors ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok(UpgradedAnchor {
                item_key: row.get(0)?,
                candidate_ordinal: row.get(1)?,
                selected: row.get(2)?,
                logical_symbol_id: row.get(3)?,
                file_path: row.get(4)?,
            })
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        upgraded,
        vec![
            UpgradedAnchor {
                item_key: "5".into(),
                candidate_ordinal: 0,
                selected: 0,
                logical_symbol_id: None,
                file_path: Some("src/widget.rs".into()),
            },
            UpgradedAnchor {
                item_key: "5".into(),
                candidate_ordinal: 1,
                selected: 0,
                logical_symbol_id: Some("sym_3e7".into()),
                file_path: Some("src/widget.rs".into()),
            },
            UpgradedAnchor {
                item_key: "6".into(),
                candidate_ordinal: 0,
                selected: 0,
                logical_symbol_id: None,
                file_path: Some("src/other.rs".into()),
            },
        ],
        "backfill is per-thread, deterministic, unselected, and preserves exact anchors",
    );

    assert!(
        legacy
            .execute_batch(
                "UPDATE papertrail_distill_anchors SET candidate_ordinal = -1 WHERE id = 1",
            )
            .is_err(),
        "candidate ordinals are non-negative",
    );
    assert!(
        legacy
            .execute("UPDATE papertrail_distill_anchors SET selected = 2 WHERE id = 1", [])
            .is_err(),
        "selected is a checked SQLite boolean",
    );
    assert!(
        legacy
            .execute(
                "UPDATE papertrail_distill_anchors SET candidate_ordinal = 0 WHERE id = 2",
                [],
            )
            .is_err(),
        "candidate ordinals are unique within a thread",
    );
    legacy.execute("UPDATE papertrail_distill_anchors SET selected = 1 WHERE id = 2", []).unwrap();
    schema::apply_distill_anchor_selection(&legacy).unwrap();
    let selected: i64 = legacy
        .query_row("SELECT selected FROM papertrail_distill_anchors WHERE id = 2", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(selected, 1, "replaying V078 does not erase a later model selection");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let v78_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '078_distill_anchor_selection'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v78_recorded, 1, "the forward migration records V078");
    for index in
        ["idx_papertrail_distill_anchors_candidate", "idx_papertrail_distill_anchors_selected"]
    {
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "V078 index `{index}` exists");
    }
}

#[test]
fn migration_079_builds_safe_input_snapshots() {
    // `LATEST_SCHEMA_VERSION` pin moved to `migration_083_*`, the new tip; this uses only the
    // symbolic checks (the hardcoded-LATEST footgun).
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_distill_record_store(&legacy).unwrap();
    schema::apply_distill_anchor_selection(&legacy).unwrap();
    assert!(!conn_table_columns(&legacy, "papertrail_distill").contains(&"prompt_version".into()));
    assert!(!schema::table_exists(&legacy, "papertrail_distill_sources").unwrap());

    schema::apply_distill_safe_input_snapshot(&legacy).unwrap();
    let distill_columns = conn_table_columns(&legacy, "papertrail_distill");
    assert!(distill_columns.contains(&"prompt_version".into()));
    assert!(distill_columns.contains(&"model_input_hash".into()));
    for table in ["papertrail_distill_sources", "papertrail_distill_units"] {
        assert!(schema::table_exists(&legacy, table).unwrap(), "V079 table `{table}` exists");
    }

    legacy
        .execute(
            "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, repo_id)
             VALUES ('github','o/r','issue','5',0,'primary',NULL,'issue','5','item','title','5',
                     'same','repoA')",
            [],
        )
        .unwrap();
    legacy
        .execute(
            "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, repo_id)
             VALUES ('github','o/r','issue','5',0,'primary',NULL,'issue','5','item','title','5',
                     'same','repoB')",
            [],
        )
        .unwrap();
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_sources
                     (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                      source_item_kind, source_item_key, source_kind, source_part, source_id,
                      exact_text, repo_id)
                 VALUES ('github','o/r','issue','5',0,'primary',NULL,'issue','5','item','body','5',
                         'same','repoA')",
                [],
            )
            .is_err(),
        "source ordinals are unique only inside the full repo-scoped record identity",
    );
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_sources
                     (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                      source_item_kind, source_item_key, source_kind, source_part, source_id,
                      exact_text, repo_id)
                 VALUES ('github','o/r','issue','5',1,'partner',NULL,'change_request','6','item',
                         'body','6','x','repoA')",
                [],
            )
            .is_err(),
        "partner sources require a partner ordinal",
    );
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_units
                     (tracker, project, item_kind, item_key, unit_ordinal, source_ordinal,
                      byte_start, byte_end, repo_id)
                 VALUES ('github','o/r','issue','5',0,0,4,4,'repoA')",
                [],
            )
            .is_err(),
        "unit spans are non-empty half-open byte ranges",
    );

    // Torn replay: both columns and the source table survived, while the unit table/index did not.
    // The additive applier must converge without touching existing source rows.
    legacy.execute_batch("DROP TABLE papertrail_distill_units;").unwrap();
    schema::apply_distill_safe_input_snapshot(&legacy).unwrap();
    assert!(schema::table_exists(&legacy, "papertrail_distill_units").unwrap());
    let sources: i64 = legacy
        .query_row("SELECT COUNT(*) FROM papertrail_distill_sources", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sources, 2, "replay preserves existing snapshots");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '079_distill_safe_input_snapshot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V079");
}

#[test]
fn migration_080_builds_enriched_context_snapshots() {
    // The `LATEST_SCHEMA_VERSION` pin lives on `migration_083_*`, the current tip; this uses
    // only the symbolic checks (the hardcoded-LATEST footgun).
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_distill_record_store(&legacy).unwrap();
    schema::apply_distill_anchor_selection(&legacy).unwrap();
    schema::apply_distill_safe_input_snapshot(&legacy).unwrap();
    assert!(!schema::table_exists(&legacy, "papertrail_distill_fix_diffs").unwrap());
    assert!(!schema::table_exists(&legacy, "papertrail_distill_xrefs").unwrap());

    schema::apply_distill_enriched_context(&legacy).unwrap();
    for table in ["papertrail_distill_fix_diffs", "papertrail_distill_xrefs"] {
        assert!(schema::table_exists(&legacy, table).unwrap(), "V080 table `{table}` exists");
    }

    legacy
        .execute(
            "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES ('github','o/r','issue','5','abc123','src/lib.rs','patch A','repoA')",
            [],
        )
        .unwrap();
    legacy
        .execute(
            "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES ('github','o/r','issue','5','abc123','src/lib.rs','patch B','repoB')",
            [],
        )
        .unwrap();
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_fix_diffs
                     (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
                 VALUES ('github','o/r','issue','5','abc123','src/lib.rs','dup','repoA')",
                [],
            )
            .is_err(),
        "one patch row per (record, commit, path) inside the full repo-scoped record identity",
    );
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_xrefs
                     (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                      target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                      repo_id)
                 VALUES ('github','o/r','issue','5',-1,'github','o/r','issue','9','reference',
                         't','o','repoA')",
                [],
            )
            .is_err(),
        "xref ordinals are non-negative",
    );
    legacy
        .execute(
            "INSERT INTO papertrail_distill_xrefs
                 (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                  target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                  repo_id)
             VALUES ('github','o/r','issue','5',0,'github','o/r','issue','9','reference','t','o',
                     'repoA')",
            [],
        )
        .unwrap();
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_xrefs
                     (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                      target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                      repo_id)
                 VALUES ('github','o/r','issue','5',0,'github','o/r','issue','10','reference',
                         't','o','repoA')",
                [],
            )
            .is_err(),
        "xref ordinals are unique within a record",
    );

    // Torn replay: the diff table survived, the xref table did not. The additive applier must
    // converge without touching existing rows.
    legacy.execute_batch("DROP TABLE papertrail_distill_xrefs;").unwrap();
    schema::apply_distill_enriched_context(&legacy).unwrap();
    assert!(schema::table_exists(&legacy, "papertrail_distill_xrefs").unwrap());
    let diffs: i64 = legacy
        .query_row("SELECT COUNT(*) FROM papertrail_distill_fix_diffs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(diffs, 2, "replay preserves existing diff snapshots");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '080_distill_enriched_context'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V080");
}

#[test]
fn migration_081_adds_evidence_source_part() {
    // The `LATEST_SCHEMA_VERSION` pin lives on `migration_083_*`, the current tip.

    // The evidence table predates the column: build the record store (V077) without V081, then
    // apply V081 and confirm the column appears.
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_distill_record_store(&legacy).unwrap();
    assert!(
        !schema::column_exists(&legacy, "papertrail_distill_evidence", "source_part").unwrap(),
        "V077 evidence has no source_part",
    );

    schema::apply_distill_evidence_source_part(&legacy).unwrap();
    assert!(
        schema::column_exists(&legacy, "papertrail_distill_evidence", "source_part").unwrap(),
        "V081 adds the source_part column",
    );

    // A pre-V081 row is representable with NULL source_part (the value CHECK passes NULL).
    legacy
        .execute(
            "INSERT INTO papertrail_distill_evidence
                 (tracker, project, item_kind, item_key, field, source_kind, source_id,
                  byte_start, byte_end, quote, repo_id)
             VALUES ('github','o/r','issue','5','root_cause','item','5',0,4,'quot','repoA')",
            [],
        )
        .unwrap();
    // A new row must carry one of title|body|comment.
    legacy
        .execute(
            "INSERT INTO papertrail_distill_evidence
                 (tracker, project, item_kind, item_key, field, source_kind, source_part,
                  source_id, byte_start, byte_end, quote, repo_id)
             VALUES ('github','o/r','issue','5','decision','item','title','5',0,4,'quot','repoA')",
            [],
        )
        .unwrap();
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_evidence
                     (tracker, project, item_kind, item_key, field, source_kind, source_part,
                      source_id, byte_start, byte_end, quote, repo_id)
                 VALUES ('github','o/r','issue','5','decision','item','headline','5',0,4,'q',
                         'repoA')",
                [],
            )
            .is_err(),
        "source_part is CHECK-constrained to title|body|comment",
    );

    // Torn replay: the column already exists, so re-applying is a no-op, not a duplicate-column
    // error, and existing rows survive.
    schema::apply_distill_evidence_source_part(&legacy).unwrap();
    let rows: i64 = legacy
        .query_row("SELECT COUNT(*) FROM papertrail_distill_evidence", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2, "an idempotent re-apply preserves existing evidence rows");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '081_distill_evidence_source_part'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V081");
}

#[test]
fn migration_082_accounts_for_content_refold_work() {
    // The `LATEST_SCHEMA_VERSION` pin lives on `migration_084_*`, the current tip.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    let insert_candidate =
        |conn: &rusqlite::Connection, hash: u8, stream: u8, signed_bytes: &[u8], received_at_ms| {
            conn.execute(
                "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                     grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                     received_at_ms)
                 VALUES(?1, ?2, zeroblob(32), zeroblob(32), zeroblob(8), NULL, NULL, zeroblob(32),
                        zeroblob(8), zeroblob(8), 0, ?3, ?4)",
                rusqlite::params![vec![hash; 32], vec![stream; 32], signed_bytes, received_at_ms],
            )
        };

    // Reconstruct a pre-V082 index: retain the V081 distill migrations and their data, but
    // restore the V072 queue shape and remove every V082 object, then replay forward from V081
    // (V082 does the content-refold work; later tips are orthogonal to it).
    conn.execute_batch(
        "DROP TRIGGER content_stream_stats_after_insert;
         DROP TRIGGER content_stream_stats_after_delete;
         DROP TRIGGER content_stream_stats_after_update;
         DROP TABLE content_stream_stats;
         DROP INDEX content_streams_pending_refold_order;
         DROP TABLE content_streams_pending_refold;
         CREATE TABLE content_streams_pending_refold(
             stream_id BLOB PRIMARY KEY CHECK(length(stream_id) = 32)
         ) STRICT;",
    )
    .unwrap();
    insert_candidate(&conn, 1, 0x11, b"abc", 300).unwrap();
    insert_candidate(&conn, 2, 0x11, b"12345", 100).unwrap();
    insert_candidate(&conn, 3, 0x22, b"payload", 200).unwrap();
    conn.execute(
        "INSERT INTO content_streams_pending_refold(stream_id) VALUES (?1), (?2)",
        rusqlite::params![vec![0x11u8; 32], vec![0x33u8; 32]],
    )
    .unwrap();
    truncate_schema_to(&conn, 81);

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    // Replaying from 81 runs V082 and everything after it, so pin the TIP rather than a literal —
    // this assertion is about the forward path completing, not about which migration is last.
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);

    let queued: Vec<(Vec<u8>, i64, i64, i64)> = conn
        .prepare(
            "SELECT stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms
             FROM content_streams_pending_refold ORDER BY stream_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        queued,
        vec![(vec![0x11u8; 32], 1, 100, 300), (vec![0x33u8; 32], 1, 0, 0)],
        "legacy rows become content-candidate work with deterministic source-derived times",
    );

    let stats: Vec<(Vec<u8>, i64, i64)> = conn
        .prepare(
            "SELECT stream_id, candidate_count, candidate_bytes
             FROM content_stream_stats ORDER BY stream_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        stats,
        vec![(vec![0x11u8; 32], 2, 72), (vec![0x22u8; 32], 1, 39)],
        "candidate_bytes is signed_bytes plus the separately loaded 32-byte entry hash per row",
    );

    let ordered_index: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master
              WHERE type = 'index' AND name = 'content_streams_pending_refold_order'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ordered_index, 1, "ordered pending selection has its composite index");

    // Insert, ignored duplicate, failed duplicate, signed-byte update, stream move, and deletes all
    // flow through database-owned accounting. Removing the final candidate removes the sparse stats
    // row rather than retaining a zero-work stream.
    insert_candidate(&conn, 4, 0x22, b"xx", 400).unwrap();
    let duplicate_ignored = conn
        .execute(
            "INSERT OR IGNORE INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, roster_ref,
                 owner_auth_len, author_auth_len, signed_bytes, received_at_ms)
             SELECT entry_hash, ?1, author_account_id, device_fingerprint, seq, roster_ref,
                    owner_auth_len, author_auth_len, x'00', received_at_ms
             FROM content_entries WHERE entry_hash = ?2",
            rusqlite::params![vec![0x44u8; 32], vec![4u8; 32]],
        )
        .unwrap();
    assert_eq!(duplicate_ignored, 0);
    assert!(insert_candidate(&conn, 4, 0x44, b"different", 401).is_err());
    conn.execute(
        "UPDATE content_entries SET signed_bytes = ?1, stream_id = ?2 WHERE entry_hash = ?3",
        rusqlite::params![b"updated", vec![0x44u8; 32], vec![4u8; 32]],
    )
    .unwrap();
    conn.execute("DELETE FROM content_entries WHERE stream_id = ?1", [vec![0x44u8; 32]]).unwrap();
    let stream_44_stats: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_stream_stats WHERE stream_id = ?1",
            [vec![0x44u8; 32]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stream_44_stats, 0, "last-row deletion removes the stats row");
    let stream_22_stats: (i64, i64) = conn
        .query_row(
            "SELECT candidate_count, candidate_bytes FROM content_stream_stats WHERE stream_id = \
             ?1",
            [vec![0x22u8; 32]],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stream_22_stats, (1, 39), "move/delete and duplicate attempts do not drift peers");

    for sql in [
        "INSERT INTO content_stream_stats VALUES(zeroblob(31), 0, 0)",
        "INSERT INTO content_stream_stats VALUES(zeroblob(32), -1, 0)",
        "INSERT INTO content_stream_stats VALUES(zeroblob(32), 0, -1)",
        "INSERT INTO content_streams_pending_refold VALUES(zeroblob(31), 1, 0, 0)",
        "INSERT INTO content_streams_pending_refold VALUES(randomblob(32), 0, 0, 0)",
        "INSERT INTO content_streams_pending_refold VALUES(randomblob(32), 4, 0, 0)",
    ] {
        assert!(conn.execute_batch(sql).is_err(), "constraint must reject `{sql}`");
    }

    // A full-ladder replay recomputes the same source-derived stats and leaves queue metadata
    // intact.
    schema::apply_content_refold_queue_and_stats(&conn).unwrap();
    schema::apply_content_refold_queue_and_stats(&conn).unwrap();
    let replay_stats: Vec<(Vec<u8>, i64, i64)> = conn
        .prepare(
            "SELECT stream_id, candidate_count, candidate_bytes
             FROM content_stream_stats ORDER BY stream_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(replay_stats, vec![(vec![0x11u8; 32], 2, 72), (vec![0x22u8; 32], 1, 39)]);
    assert_eq!(
        conn.query_row(
            "SELECT first_enqueued_at_ms, last_enqueued_at_ms
             FROM content_streams_pending_refold WHERE stream_id = ?1",
            [vec![0x11u8; 32]],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap(),
        (100, 300),
        "replay does not rebuild or overwrite the upgraded queue",
    );
    let v81_recorded: i64 = conn
        .query_row(
            "SELECT count(*) FROM schema_version WHERE id = '081_distill_evidence_source_part'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v81_recorded, 1, "the previous-tip V081 migration remains recorded");
    let v82_recorded: i64 = conn
        .query_row(
            "SELECT count(*) FROM schema_version WHERE id = '082_content_refold_queue_and_stats'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v82_recorded, 1, "forward migration records V082");
}

/// V083 recomputes `logical_symbols.group_reason` from member evidence.
///
/// The column is derived but PERSISTED, so a new labelling rule alone would never reach an existing
/// index: a query-only server over an unchanged repository never runs `rebuild_logical_symbols` and
/// would keep serving the old `cfg_variant` for every multi-member group indefinitely.
#[test]
fn migration_083_relabels_logical_groups_by_evidence() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    // Two `files` ROWS for one path — what a worktree-overlay or commit scope produces.
    for (id, worktree) in [(1_i64, "base"), (2_i64, "wt")] {
        conn.execute(
            "INSERT INTO files(id, path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                               commit_sha, worktree_id)
             VALUES (?1, 'src/lib.rs', 'rust', 'source', 'sha', 0, 0, 'head', ?2)",
            rusqlite::params![id, worktree],
        )
        .unwrap();
    }
    let symbol = |id: i64, file_id: i64, name: &str| {
        conn.execute(
            "INSERT INTO symbols(id, file_id, language, name, kind, start_byte, end_byte)
             VALUES (?1, ?2, 'rust', ?3, 'function', 0, 1)",
            rusqlite::params![id, file_id, name],
        )
        .unwrap();
    };
    // `replicated` is ONE symbol seen in both scopes; `collided` is TWO symbols in one file.
    symbol(1, 1, "replicated");
    symbol(2, 2, "replicated");
    symbol(3, 1, "collided");
    symbol(4, 1, "collided");
    symbol(5, 2, "solo");

    let group = |id: i64, name: &str, members: &[i64]| {
        conn.execute(
            "INSERT INTO logical_symbols(id, language, path, logical_name, kind, variant_count,
                                         group_reason)
             VALUES (?1, 'rust', 'src/lib.rs', ?2, 'function', ?3, 'cfg_variant')",
            rusqlite::params![id, name, members.len() as i64],
        )
        .unwrap();
        for symbol_id in members {
            conn.execute(
                "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line,
                                                    end_line)
                 VALUES (?1, ?2, 1, 2)",
                rusqlite::params![id, symbol_id],
            )
            .unwrap();
        }
    };
    group(100, "replicated", &[1, 2]);
    group(200, "collided", &[3, 4]);
    group(300, "solo", &[5]);

    truncate_schema_to(&conn, 82);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    // Replaying from 82 runs V083 and everything after it, so pin the TIP rather than a literal.
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);

    let reason = |id: i64| -> String {
        conn.query_row("SELECT group_reason FROM logical_symbols WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap()
    };
    assert_eq!(
        reason(100),
        "scope_replica",
        "one symbol indexed in two scopes is a replica, not a cfg variant — this is the case the \
         old label got wrong for the overwhelming majority of groups",
    );
    assert_eq!(
        reason(200),
        "same_file_multi",
        "two symbols inside one file row genuinely share an identity",
    );
    assert_eq!(reason(300), "single");
}

/// V084 links each chunk to the symbol it was cut from, and is the schema tip.
#[test]
fn migration_084_is_the_tip_and_links_chunks_to_symbols() {
    assert_eq!(schema::LATEST_SCHEMA_VERSION, 84, "move this pin with the next schema migration");

    // The chunks table predates the column (it lives in the baseline). Build bare chunks + symbols
    // tables WITHOUT symbol_id, seed pre-migration rows, apply V084 in ISOLATION, and confirm the
    // column appears AND the backfill links each chunk — asserting absence against this migration's
    // own precondition, never the full ladder's end state.
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE chunks(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 file_id INTEGER NOT NULL,
                 chunk_kind TEXT NOT NULL,
                 symbol_path TEXT,
                 start_byte INTEGER NOT NULL,
                 end_byte INTEGER NOT NULL,
                 start_line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL,
                 text_hash TEXT NOT NULL);
             CREATE TABLE name_strings(id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL);
             CREATE TABLE symbols(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 file_id INTEGER NOT NULL,
                 qualified_name_id INTEGER NOT NULL,
                 start_byte INTEGER NOT NULL,
                 end_byte INTEGER NOT NULL,
                 start_line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL);",
        )
        .unwrap();
    assert!(
        !schema::column_exists(&legacy, "chunks", "symbol_id").unwrap(),
        "pre-V084 chunks has no symbol_id",
    );

    // Symbols in one file: an OUTER `outer` (id 1, bytes 100..200) with a DIFFERENT-named NESTED
    // `inner` (id 2, bytes 130..160); a PAIR of same-name `g` (ids 3 & 4, disjoint bytes on one
    // physical line — the minified same-line case); and an OUTER `wrap` (id 5, bytes 400..600) with
    // a same-name NESTED `wrap` (id 6, bytes 500..560). Their line spans are 0/0 — the migration
    // DEFAULT for a symbol never reindexed since the line columns were added — so the backfill MUST
    // key off byte spans. Chunks carry the qualified name they were cut from (a split continuation
    // appends `#<n>`).
    legacy
        .execute_batch(
            "INSERT INTO name_strings(id, value) VALUES (1,'outer'), (2,'inner'), (3,'g'), \
             (4,'wrap'), (5,'src/od#d.rs::run'), (6,'trailing2');
             INSERT INTO symbols(id, file_id, qualified_name_id, start_byte, end_byte, start_line, \
             end_line)
                 VALUES (1, 1, 1, 100, 200, 0, 0), (2, 1, 2, 130, 160, 0, 0),
                        (3, 1, 3, 300, 310, 0, 0), (4, 1, 3, 320, 330, 0, 0),
                        (5, 1, 4, 400, 600, 0, 0), (6, 1, 4, 500, 560, 0, 0),
                        (7, 1, 5, 700, 720, 0, 0), (8, 1, 6, 800, 820, 0, 0);
             INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte, start_line,
                                end_line, text_hash)
                 VALUES (1,'code','outer',100,200,13,14,'h'),
                        (1,'code','inner',130,160,14,15,'h'),
                        (1,'code','outer#1',130,160,13,14,'h'),
                        (1,'code','g',300,330,30,30,'h'),
                        (1,'code','wrap#1',520,540,55,58,'h'),
                        (1,'code',NULL,900,910,80,81,'h'),
                        (1,'code','src/od#d.rs::run',700,720,70,71,'h'),
                        (1,'code','trailing2',800,820,75,76,'h');",
        )
        .unwrap();

    schema::apply_chunk_symbol_id(&legacy).unwrap();
    assert!(
        schema::column_exists(&legacy, "chunks", "symbol_id").unwrap(),
        "V084 adds the symbol_id column",
    );

    // The backfill binds a chunk to the same-named symbol it overlaps in BYTES only when that
    // symbol is UNIQUE — matching by NAME (line spans are 0, so a line predicate would match
    // nothing):
    //   * `outer` -> id 1, even though the `inner` bytes also overlap (`inner` is a different name,
    //     so it is not a candidate);
    //   * `inner` -> id 2;
    //   * `outer#1` CONTINUATION -> id 1: the `#1` suffix is stripped to `outer`, whose only
    //     overlapping symbol is the outer — recovered, and NOT mis-bound to the nested `inner`.
    // It leaves NULL every case no stored data can settle: the same-line `g` chunk (its whole-line
    // bytes overlap both `g` symbols), the `wrap#1` continuation (overlaps both the outer and
    // nested `wrap`), and the uncovered chunk (no symbol_path).
    let linked: Vec<(Option<String>, Option<i64>)> = legacy
        .prepare("SELECT symbol_path, symbol_id FROM chunks ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        linked,
        vec![
            (Some("outer".to_string()), Some(1)),
            (Some("inner".to_string()), Some(2)),
            (Some("outer#1".to_string()), Some(1)),
            (Some("g".to_string()), None),
            (Some("wrap#1".to_string()), None),
            (None, None),
            // A `#` inside the FILE PATH is not a continuation marker. Splitting on the FIRST `#`
            // would truncate this to `src/od`, match nothing, and — since unchanged files are
            // never re-chunked — strand the row at NULL forever. Only a TRAILING
            // `#<digits>` is a suffix.
            (Some("src/od#d.rs::run".to_string()), Some(7)),
            // Trailing digits that are part of the NAME are likewise not a suffix: there is no `#`
            // before them.
            (Some("trailing2".to_string()), Some(8)),
        ],
        "backfill links a uniquely-named container (including a stripped continuation); a \
         same-name tie / nested continuation / uncovered chunk stays NULL, and a `#` inside the \
         FILE PATH is never mistaken for a continuation marker",
    );

    // Torn replay: the column already exists and every chunk is already linked, so re-applying is a
    // no-op — it neither errors nor overwrites a resolved symbol_id.
    schema::apply_chunk_symbol_id(&legacy).unwrap();
    let relinked: Vec<(Option<String>, Option<i64>)> = legacy
        .prepare("SELECT symbol_path, symbol_id FROM chunks ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(relinked, linked, "an idempotent re-apply preserves the backfilled links");

    // Full ladder: the tip provisions cleanly and records V084 with the column present.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    assert!(
        schema::column_exists(&conn, "chunks", "symbol_id").unwrap(),
        "the full ladder ends with chunks.symbol_id present",
    );
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '084_chunk_symbol_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V084");
}
