use super::*;

#[test]
fn v026_recreates_chunk_fts_contentless_and_repopulates() {
    // #77 Phase 2: chunk_fts becomes a CONTENTLESS FTS5 index. Fresh apply yields a contentless
    // table that supports delete-by-rowid (contentless_delete=1); the forward-migrate (V026)
    // converts an existing external-content table and repopulates it from chunks.text.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

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

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn full_rebuild_populates_the_chunk_text_store() {
    // `chunk_text` is a GLOBAL compression store keyed by `chunk_id` (no repo dimension); this test
    // asserts it mirrors the WHOLE `chunks` table 1:1. The poison-sibling harness seeds a raw
    // tripwire chunk with no `chunk_text` row, breaking that whole-DB 1:1 by design — a single-repo
    // invariant, not a leak. Opt out.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
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
        let back =
            rag_rat_db::text_compression::decompress(&blob, &dict, raw_len as usize).unwrap();
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
    // Whole-DB `chunk_text`↔`chunks` 1:1 invariant (global store) — the poison-sibling tripwire
    // chunk has no `chunk_text` mirror by design. Single-repo invariant; opt out.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
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
        let back =
            rag_rat_db::text_compression::decompress(&blob, &dict, raw_len as usize).unwrap();
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
    // Asserts the WHOLE DB has zero chunks (a whitespace-only markdown produces none). The
    // poison-sibling tripwire seeds one chunk under a sibling repo, so this unscoped whole-DB count
    // is a single-repo assertion; opt out.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
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
            .execute(
                "DELETE FROM schema_version WHERE id = (SELECT id FROM schema_version ORDER BY id \
                 DESC LIMIT 1)",
                [],
            )
            .unwrap();
        assert_eq!(
            schema::status(db.storage.connection()).unwrap().state,
            schema::SchemaState::Older,
            "removing the newest ledger row makes the schema Older"
        );
    }

    // Hold the PER-REPO write lock exactly as the CLI `index` command does, then open under it.
    // Pre-#226 the open-time migrate took the SAME lock and self-deadlocked; A6 moved the migrate
    // onto the GLOBAL schema lock, so it is now an INDEPENDENT lock — the held per-repo write lock
    // never blocks it, and the open migrates immediately.
    let _lock = rag_rat_base::locks::WriteLock::acquire_blocking(&db_path, "testrepo0000").unwrap();
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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
        ",
    )
    .unwrap();
    // Truncate the ledger to V21 so `known_version` reads V21 and the forward-migrate replays
    // V022+. The artifacts the later migrations added — the edges view (V023),
    // files.has_test_code (V024), … — can stay: their apply fns are idempotent, so the
    // forward-migrate is a clean no-op for the parts already present.
    truncate_schema_to(&conn, 21);
    assert_eq!(schema::status(&conn).unwrap().current_version, 21, "now looks like a V21 index");
    assert!(!conn_table_exists(&conn, "packages"));

    // Forward-migrate: re-running apply (the Older→apply path) converges to the latest version.
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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

/// V028 fresh apply (#224): a brand-new index stores qualified names as `qualified_name_id`
/// (interned into `name_strings`), NOT inline `qualified_name`; the id indexes exist and the old
/// string indexes do not.
#[test]
fn v028_fresh_apply_interns_symbol_qualified_names() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

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
    // 3. Truncate the ledger to V27 so the pre-V028 shape reads Older and the migration replays.
    truncate_schema_to(&conn, 27);
    assert_eq!(
        schema::status(&conn).unwrap().state,
        schema::SchemaState::Older,
        "truncating the ledger to V27 makes the pre-V028 shape Older"
    );

    // --- Forward-migrate ---
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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
    let hit = rag_rat_query::symbol::lookup_candidates(
        &conn,
        &rag_rat_query::symbol::SymbolSelector {
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
    let logical = rag_rat_query::symbol::lookup_logical_by_id(&conn, 7).unwrap().unwrap();
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
    // Insert at the ACTIVE generation (A6) so gc's dead-generation sweep keeps this row (it is the
    // live generation), the same way a real indexed file lands — the default 0 would be a dead
    // generation after the rebuild advanced the live pointer, and gc would sweep it.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                                commit_sha, worktree_id, generation)
         VALUES ('only.rs', 'rust', 'source', 'h', 0, 0, ?1, ?2, ?3)",
        params![db.active_commit_sha, db.active_worktree_id, db.active_generation],
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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

    // Scope the assertion read to the ACTIVE repo: the poison-sibling harness seeds a same-path
    // `main.files` row at this fixture's path, so an unscoped `WHERE path = ...` `query_row` is
    // ambiguous (and after heal re-inserts the primary row at a higher rowid, would return the
    // sibling's flag). This is a single-repo test, but the fix is to scope the query, not disable
    // the harness.
    let repo_id = rag_rat_db::schema::active_repo_id(db.storage.connection()).unwrap();
    let flag = || -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT has_test_code FROM main.files WHERE path = 'src/markers.rs' AND repo_id = \
                 ?1",
                [&repo_id],
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
fn heal_reindexes_a_file_to_the_same_chunk_policies_as_a_full_rebuild() {
    // #518: the heal path (`heal_file` -> `index_file`) now prepares each file through the SAME
    // single-parse core (`prepare_index_content_from_text` -> `insert_prepared_file`) the
    // full-rebuild / changed passes use, instead of its own 5×-parse derivation with a text-based
    // low-signal fallback. This pins the end-to-end consequence: a file re-indexed by heal lands
    // the SAME per-chunk `embedding_policy` (the span-based low-signal decision) as the full
    // rebuild that first indexed it. If `index_file` ever regresses to a bespoke derivation,
    // the policies drift and this fails.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // A doc-comment + `use` block (low-signal) alongside real function bodies (embed) — the same
    // shape as prep.rs's `span_and_text_low_signal_paths_agree_on_prepared_chunk_policies`, so both
    // policy outcomes occur and the parity check below is non-vacuous.
    let src = "//! Module documentation explaining what this file is for.\n\nuse \
               std::collections::BTreeMap;\nuse std::collections::HashSet;\nuse \
               std::path::PathBuf;\n\npub fn real_work(input: usize) -> usize {\n    let doubled \
               = input * 2;\n    let shifted = doubled + 7;\n    shifted * shifted\n}\n\npub fn \
               more_work(count: usize) -> usize {\n    let mut total = 0;\n    for step in \
               0..count {\n        total += step;\n    }\n    total\n}\n";
    fs::write(root.join("src/lib.rs"), src).unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let repo_id = rag_rat_db::schema::active_repo_id(db.storage.connection()).unwrap();
    // Content-keyed (not rowid-keyed: heal removes + re-inserts, so ids change) chunk-policy
    // snapshot for the file, scoped to the active repo (the poison-sibling harness seeds a
    // same-path row under a sibling repo).
    let policies = || -> Vec<(i64, i64, String)> {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT c.start_byte, c.end_byte, c.embedding_policy
                 FROM main.chunks c JOIN main.files f ON f.id = c.file_id
                 WHERE f.path = 'src/lib.rs' AND f.repo_id = ?1
                 ORDER BY c.start_byte, c.end_byte",
            )
            .unwrap();
        stmt.query_map([&repo_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };

    let before = policies();
    assert!(
        before.iter().any(|(.., p)| p == "SkipLowSignal"),
        "fixture must exercise the low-signal outcome: {before:?}"
    );
    assert!(
        before.iter().any(|(.., p)| p == "Embed"),
        "fixture must exercise the embed outcome: {before:?}"
    );

    // Heal the UNCHANGED file: the heal derivation must reproduce the rebuild's chunk policies.
    db.heal_file(std::path::Path::new("src/lib.rs")).unwrap();
    assert_eq!(
        policies(),
        before,
        "heal re-indexes the file to the same span-based chunk policies as the full rebuild (#518)"
    );

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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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

    let _ = fs::remove_dir_all(&root);
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
    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "path memory title".to_string(),
        body: "bound to src/a/x.rs".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
            path: Some("src/a/x.rs".to_string()),
            logical_symbol_id: None,
            symbol_id: None,
            chunk_id: None,
            edge_id: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            tracker: None,
            project: None,
            item_key: None,
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
    let o1 = crate::query::orientation::orientation(conn, &root, &root, None).unwrap();

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
    let o2 = crate::query::orientation::orientation(conn, &root, &root, None).unwrap();
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

    let _ = fs::remove_dir_all(&root);
}
