use super::*;

/// V032 forward migrate (#231): an index recorded at V031 (postings table present, NO `token_bag`
/// column) must, after migrate_forward, gain the `token_bag` BLOB column and lose
/// `symbol_token_postings`. Simulates the pre-V032 shape by re-creating the postings table +
/// dropping the column + deleting the V032 ledger row (making the schema Older), then replays.
#[test]
fn migration_032_adds_token_bag_drops_postings() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).expect("apply");

    // --- Simulate a V031-era index: postings table present, no token_bag column ---
    conn.execute_batch(
        "ALTER TABLE symbol_fingerprints DROP COLUMN token_bag;
         CREATE TABLE IF NOT EXISTS symbol_token_postings(
             symbol_id       INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
             normalizer_kind TEXT    NOT NULL,
             token_hash      INTEGER NOT NULL,
             freq            INTEGER NOT NULL,
             PRIMARY KEY (symbol_id, normalizer_kind, token_hash)
         ) STRICT;",
    )
    .expect("revert to V031 shape");
    truncate_schema_to(&conn, 31);
    assert!(
        !conn_table_columns(&conn, "symbol_fingerprints").contains(&"token_bag".to_string()),
        "token_bag is absent before the migration runs"
    );
    assert!(conn_table_exists(&conn, "symbol_token_postings"), "postings present at V031");
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().state,
        rag_rat_db::schema::SchemaState::Older,
        "schema is Older after removing the V032 ledger row"
    );

    // --- Run the forward migration ---
    rag_rat_db::schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("migrate_forward");

    assert!(
        conn_table_columns(&conn, "symbol_fingerprints").contains(&"token_bag".to_string()),
        "V032 adds the token_bag BLOB column"
    );
    assert!(!conn_table_exists(&conn, "symbol_token_postings"), "V032 drops symbol_token_postings");
    // The column is a queryable BLOB.
    let _: i64 = conn
        .query_row("SELECT COUNT(token_bag) FROM symbol_fingerprints", [], |r| r.get(0))
        .expect("SELECT token_bag must succeed after V032");
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().current_version,
        rag_rat_db::schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST_SCHEMA_VERSION after V032"
    );
    // Idempotency: a second migrate_forward is a clean no-op (guarded on the token_bag column).
    rag_rat_db::schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("migrate_forward is idempotent");
    assert!(matches!(
        rag_rat_db::schema::status(&conn).unwrap().state,
        rag_rat_db::schema::SchemaState::Compatible
    ));
}

/// V034 (#286): the precomputed clone-graph tables exist after a forward migration from V033, are
/// CONTENT-ANCHORED (no `symbol_id` FK — the #248 rule the volatile-FK trip-wire enforces), and are
/// usable. A V033 index migrates forward to LATEST and gains `clone_graph_generations` +
/// `clone_edges` with the content-key endpoints; the deferred postings table is NOT created.
#[test]
fn migration_034_adds_content_anchored_clone_graph_tables() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).expect("apply");

    // --- Simulate a V033-era index: clone-graph tables absent, schema rolled back to V033 ---
    conn.execute_batch(
        "DROP TABLE IF EXISTS clone_edges; DROP TABLE IF EXISTS clone_graph_generations;",
    )
    .expect("revert to V033 shape");
    truncate_schema_to(&conn, 33);
    assert!(!conn_table_exists(&conn, "clone_edges"), "clone_edges absent at V033");
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().state,
        rag_rat_db::schema::SchemaState::Older,
        "schema is Older after removing the V034 ledger row"
    );

    // --- Run the forward migration ---
    rag_rat_db::schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("migrate_forward");
    assert!(
        conn_table_exists(&conn, "clone_graph_generations"),
        "V034 adds clone_graph_generations"
    );
    assert!(conn_table_exists(&conn, "clone_edges"), "V034 adds clone_edges");
    // V034 DEFERS the persisted postings table (it lands in V037, #296). Assert the V034 DDL itself
    // does NOT create it — checked in ISOLATION on a bare connection, so a later migration (V037)
    // that DOES add it cannot mask the deferral. (`conn` above was migrated to LATEST and therefore
    // now HAS the table — that is V037's job, verified by
    // `migration_037_adds_content_anchored_clone_subblock_postings`.)
    let v034_only = rusqlite::Connection::open_in_memory().expect("open v034-only conn");
    rag_rat_db::schema::apply_clone_graph_tables(&v034_only).expect("apply V034 clone-graph DDL");
    assert!(
        !conn_table_exists(&v034_only, "clone_subblock_postings"),
        "the persisted postings table is deferred to V037 — the V034 DDL must NOT create it"
    );

    // Content-anchored: clone_edges must carry NO foreign key to a reindex-volatile parent
    // (symbols). Its only FK is to clone_graph_generations (durable). This is the #248
    // invariant in situ.
    let edge_fk_parents: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT \"table\" FROM pragma_foreign_key_list('clone_edges')").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
    };
    assert!(
        !edge_fk_parents.iter().any(|p| p == "symbols"),
        "clone_edges must NOT FK symbols (content-anchored, #248); FKs = {edge_fk_parents:?}"
    );
    let cols = conn_table_columns(&conn, "clone_edges");
    for endpoint_col in
        ["a_path", "a_start_byte", "a_file_sha", "b_path", "b_start_byte", "b_file_sha"]
    {
        assert!(
            cols.contains(&endpoint_col.to_string()),
            "clone_edges has content-key {endpoint_col}"
        );
    }

    // The tables are usable: a generation + an edge round-trips.
    conn.execute_batch(
        "INSERT INTO clone_graph_generations
             (generation, status, theta_floor, normalizer_kind, normalizer_version, \
         source_revision,
              started_at_ms)
         VALUES (1, 'Building', 0.7, 'baseline', 3, 'rev', 0);
         INSERT INTO clone_edges
             (build_generation, a_path, a_start_byte, a_file_sha, b_path, b_start_byte, b_file_sha,
              overlap, a_token_len, b_token_len, similarity, edge_source)
         VALUES (1, 'a.rs', 10, 'sha_a', 'b.rs', 20, 'sha_b', 8, 10, 9, 0.9, 'sub_block');",
    )
    .expect("clone-graph tables are usable");
    let edges: i64 =
        conn.query_row("SELECT COUNT(*) FROM clone_edges", [], |r| r.get(0)).expect("count edges");
    assert_eq!(edges, 1, "round-tripped one edge");

    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().current_version,
        rag_rat_db::schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST_SCHEMA_VERSION after V034"
    );
}

/// V035 (#292): `symbols.is_test` exists after a forward migration from V034 and is queryable. A
/// V034 index migrates forward to LATEST and gains the column (default 0 — accurate values need a
/// reindex with this binary). Simulates a V034 index by dropping the column + rolling the ledger
/// back one step, then asserts `migrate_forward` re-adds it.
#[test]
fn migration_035_adds_symbols_is_test() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).expect("apply");

    conn.execute_batch("ALTER TABLE symbols DROP COLUMN is_test;").expect("revert to V034 shape");
    truncate_schema_to(&conn, 34);
    assert!(
        !conn_table_columns(&conn, "symbols").contains(&"is_test".to_string()),
        "is_test absent at V034"
    );
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().state,
        rag_rat_db::schema::SchemaState::Older,
        "schema is Older after removing the V035 ledger row"
    );

    rag_rat_db::schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("migrate_forward");
    assert!(
        conn_table_columns(&conn, "symbols").contains(&"is_test".to_string()),
        "V035 adds symbols.is_test"
    );
    // The column is queryable + defaults to 0 (non-test) on existing rows.
    let _: i64 = conn
        .query_row("SELECT COUNT(*) FROM symbols WHERE is_test = 0", [], |r| r.get(0))
        .expect("SELECT is_test must succeed after V035");
}

/// V036 (#357): `embedding_cache` (content-addressed vectors) is added by the forward migration and
/// is the schema tip; a DB at V035 gains the table on `migrate_forward`, and it seeds from existing
/// current embeddings so vectors survive the next reindex.
#[test]
fn migration_036_adds_content_addressed_embedding_cache() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).expect("apply");

    conn.execute_batch("DROP TABLE embedding_cache;").expect("revert to V035 shape");
    truncate_schema_to(&conn, 35);
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().state,
        rag_rat_db::schema::SchemaState::Older,
        "schema is Older after removing the V036 ledger row"
    );

    rag_rat_db::schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("migrate_forward");
    assert!(
        conn_table_columns(&conn, "embedding_cache").contains(&"input_hash".to_string()),
        "V036 adds the embedding_cache table"
    );
    // Content-keyed by input_hash: a lookup query is valid after the migration.
    let _: i64 = conn
        .query_row("SELECT COUNT(*) FROM embedding_cache", [], |r| r.get(0))
        .expect("SELECT from embedding_cache must succeed after V036");
}

/// V037 (#296): after the FULL migration ladder, the persisted sub-block postings table exists, is
/// STRICT, is CONTENT-ANCHORED (its only FK is the CASCADE to the DURABLE `clone_graph_generations`
/// — NO `symbol_id` column/FK, the #248 rule), carries the anchor PK
/// `(build_generation, token_hash, path, start_byte)` + the `(build_generation, token_hash)` lookup
/// index, and `clone_graph_generations` gained the `postings_written` upgrade-repopulation gate
/// (review R2). Also drives the forward-migration path from a V036 index.
#[test]
fn migration_037_adds_content_anchored_clone_subblock_postings() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).expect("apply");
    // NB: V037 is no longer the schema tip (V038 added the repos registry), so this test no longer
    // pins LATEST_SCHEMA_VERSION to an absolute number — that pin lives on the current-tip test.

    // The full ladder creates the postings table + the generation completeness column.
    assert!(
        conn_table_exists(&conn, "clone_subblock_postings"),
        "V037 creates clone_subblock_postings"
    );
    assert!(
        conn_table_columns(&conn, "clone_graph_generations")
            .contains(&"postings_written".to_string()),
        "V037 adds clone_graph_generations.postings_written (upgrade-repopulation gate, R2)"
    );

    // STRICT mode (the schema convention for every new table).
    let create_sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='clone_subblock_postings'",
            [],
            |r| r.get(0),
        )
        .expect("read clone_subblock_postings DDL");
    assert!(
        create_sql.to_ascii_uppercase().contains("STRICT"),
        "clone_subblock_postings is STRICT: {create_sql}"
    );

    // CONTENT-ANCHORED (#248): the PK is (path, start_byte)-based, never symbol_id.
    let pk_cols: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM pragma_table_info('clone_subblock_postings') WHERE pk > 0 ORDER \
                 BY pk",
            )
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(
        pk_cols,
        vec!["build_generation", "token_hash", "path", "start_byte"],
        "content-anchored PK (path/start_byte, never symbol_id)"
    );
    let cols = conn_table_columns(&conn, "clone_subblock_postings");
    for expected in ["build_generation", "token_hash", "path", "start_byte", "file_sha"] {
        assert!(cols.contains(&expected.to_string()), "clone_subblock_postings has {expected}");
    }
    assert!(
        !cols.contains(&"symbol_id".to_string()),
        "clone_subblock_postings is content-anchored — no symbol_id column (#248)"
    );

    // The ONLY FK is the CASCADE to the DURABLE clone_graph_generations, NEVER to a
    // reindex-volatile parent (symbols). The volatile-FK trip-wire enforces this at the
    // whole-schema level too.
    let fk_parents: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT \"table\" FROM pragma_foreign_key_list('clone_subblock_postings')")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(
        fk_parents,
        vec!["clone_graph_generations"],
        "the only FK is to the durable generations table; got {fk_parents:?}"
    );

    // The lookup index the write-time check queries exists.
    assert!(
        conn_index_exists(&conn, "idx_clone_subblock_postings_token"),
        "V037 creates idx_clone_subblock_postings_token"
    );

    // Forward-migration path: a V036 index gains the table + column on migrate_forward.
    conn.execute_batch(
        "DROP TABLE clone_subblock_postings;
         ALTER TABLE clone_graph_generations DROP COLUMN postings_written;",
    )
    .expect("revert to V036 shape");
    truncate_schema_to(&conn, 36);
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().state,
        rag_rat_db::schema::SchemaState::Older,
        "schema is Older after removing the V037 ledger row"
    );
    rag_rat_db::schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("migrate_forward");
    assert!(
        conn_table_exists(&conn, "clone_subblock_postings"),
        "V037 recreates clone_subblock_postings on forward migrate"
    );
    assert!(
        conn_table_columns(&conn, "clone_graph_generations")
            .contains(&"postings_written".to_string()),
        "V037 re-adds postings_written on forward migrate"
    );

    // The table is usable + generation-staged: a posting anchored under a generation round-trips.
    conn.execute_batch(
        "INSERT INTO clone_graph_generations
             (generation, status, theta_floor, normalizer_kind, normalizer_version, \
         source_revision, started_at_ms)
         VALUES (1, 'Building', 0.7, 'baseline', 3, 'rev', 0);
         INSERT INTO clone_subblock_postings
             (build_generation, token_hash, path, start_byte, file_sha)
         VALUES (1, 42, 'a.rs', 10, 'sha_a');",
    )
    .expect("clone_subblock_postings is usable");
    let postings: i64 = conn
        .query_row("SELECT COUNT(*) FROM clone_subblock_postings", [], |r| r.get(0))
        .expect("count postings");
    assert_eq!(postings, 1, "round-tripped one posting");
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().current_version,
        rag_rat_db::schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST_SCHEMA_VERSION after V037"
    );
}

/// Regression test for the P1 schema bug (#215 Plan 4a): an index recorded at V029 WITHOUT
/// `clone_refinements.lcs_sampled` (because V029 was applied before the column landed) must have
/// the column added by the V030 forward migration. Simulates the bug by building a full schema,
/// dropping the column, deleting the V030 ledger row (making the schema Older), then re-running
/// `migrate_forward` and asserting the column is present and a direct SELECT succeeds.
#[test]
fn v030_forward_migrate_adds_lcs_sampled_to_existing_v029_index() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    // Start from a fully-applied schema (includes V029 DDL which already has lcs_sampled).
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).expect("apply");
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().current_version,
        rag_rat_db::schema::LATEST_SCHEMA_VERSION,
        "fresh apply reaches V30"
    );

    // --- Simulate a V029-era index that was recorded before lcs_sampled landed ---
    // SQLite ≥3.35 supports DROP COLUMN.
    conn.execute_batch("ALTER TABLE clone_refinements DROP COLUMN lcs_sampled;")
        .expect("drop lcs_sampled to simulate the pre-column V029 state");
    // Confirm the column is gone.
    let cols_before: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(clone_refinements)").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1)).unwrap().map(|r| r.unwrap()).collect()
    };
    assert!(
        !cols_before.contains(&"lcs_sampled".to_string()),
        "lcs_sampled must be absent before the migration runs"
    );
    // Truncate the ledger to V29 so the schema reads Older and migrate_forward replays V030.
    truncate_schema_to(&conn, 29);
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().state,
        rag_rat_db::schema::SchemaState::Older,
        "schema is Older after truncating the ledger to V29"
    );

    // --- Run the forward migration ---
    rag_rat_db::schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("migrate_forward");

    // --- Assert the column is now present and the schema is current ---
    let cols_after: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(clone_refinements)").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1)).unwrap().map(|r| r.unwrap()).collect()
    };
    assert!(
        cols_after.contains(&"lcs_sampled".to_string()),
        "V030 must add lcs_sampled to an existing V029 clone_refinements table"
    );
    // A direct SELECT proves the column is queryable (the original bug: `no such column:
    // lcs_sampled`).
    let _: i64 = conn
        .query_row("SELECT COUNT(lcs_sampled) FROM clone_refinements", [], |r| r.get(0))
        .expect("SELECT lcs_sampled must succeed after V030 migration");
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().current_version,
        rag_rat_db::schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST_SCHEMA_VERSION after V030 migration"
    );
    // Idempotency: running migrate_forward again must not error.
    rag_rat_db::schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("migrate_forward is idempotent");
    assert_eq!(
        rag_rat_db::schema::status(&conn).unwrap().state,
        rag_rat_db::schema::SchemaState::Compatible,
        "schema is still Compatible after second migrate_forward"
    );
}

/// V031 (#248): a DB migrated to LATEST has `edge_oracle` content-anchored — NO `edges_data` FK,
/// the content-key columns present, and a `DELETE FROM edges_data` does NOT cascade-wipe a manually
/// inserted `edge_oracle` row (the bug: V018's `ON DELETE CASCADE` wiped every verdict on reindex).
/// Drives the migration path explicitly: build the OLD (V018) FK shape, record the ledger one short
/// of V031, then `migrate_forward` and assert the rebuilt shape.
#[test]
fn migration_031_edge_oracle_no_fk_content_key() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).expect("apply reaches V31");

    // Simulate a V030-era index whose `edge_oracle` still has the OLD edge_id-keyed FK shape: drop
    // the content-anchored table and recreate the V018 shape, then remove the V031 ledger row so
    // migrate_forward replays the rebuild.
    conn.execute_batch(
        "
        PRAGMA foreign_keys = OFF;
        DROP TABLE edge_oracle;
        CREATE TABLE edge_oracle(
            edge_id INTEGER NOT NULL,
            file_sha TEXT NOT NULL,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            resolved_symbol_id INTEGER,
            scip_symbol TEXT NOT NULL,
            kind TEXT NOT NULL,
            computed_at INTEGER NOT NULL,
            PRIMARY KEY(edge_id, tool, tool_version),
            FOREIGN KEY(edge_id) REFERENCES edges_data(id) ON DELETE CASCADE
        ) STRICT;
        PRAGMA foreign_keys = ON;
        ",
    )
    .expect("recreate legacy V018 edge_oracle");
    // Truncate the ledger to V30 so the schema reads Older and the forward migrate replays V031.
    truncate_schema_to(&conn, 30);
    assert_eq!(
        schema::status(&conn).unwrap().state,
        schema::SchemaState::Older,
        "schema is Older after truncating the ledger to V30 + reverting the table shape"
    );
    // Confirm the legacy FK is really there before migrating.
    let fk_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_list('edge_oracle')", [], |r| r.get(0))
        .unwrap();
    assert!(fk_before > 0, "legacy edge_oracle has an edges_data FK before V031");

    rag_rat_db::schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("migrate_forward replays V031");
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST after V031"
    );

    // (1) No FK on edge_oracle.
    let fk_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_list('edge_oracle')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk_after, 0, "V031 edge_oracle has NO foreign key");

    // (2) The content-key columns exist.
    let cols = conn_table_columns(&conn, "edge_oracle");
    for expected in [
        "source_path",
        "source_start_byte",
        "source_end_byte",
        "callee_start_byte",
        "callee_end_byte",
        "edge_kind",
    ] {
        assert!(cols.contains(&expected.to_string()), "edge_oracle missing {expected}");
    }
    assert!(!cols.contains(&"edge_id".to_string()), "edge_id column is gone");

    // (3) A DELETE FROM edges_data does NOT delete a manually-inserted edge_oracle row (no
    // cascade). Seed a file + an edge so edges_data is non-empty, then insert a verdict whose
    // content key is independent of any edge id.
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES ('a.rs', 'rust', 'source', 'sha-a', 0, 0, 'c', '')",
        [],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO name_strings(value) VALUES ('target'), ('calls_name'), ('unresolved'), \
         ('NameOnly') ON CONFLICT DO NOTHING",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges_data(source_file_id, to_name_id, callee_start_byte, callee_end_byte, \
         resolution_id, edge_kind_id, confidence_id) VALUES (?1, (SELECT id FROM name_strings \
         WHERE value='target'), 14, 20, (SELECT id FROM name_strings WHERE value='unresolved'), \
         (SELECT id FROM name_strings WHERE value='calls_name'), (SELECT id FROM name_strings \
         WHERE value='NameOnly'))",
        params![file_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edge_oracle(source_path, source_start_byte, source_end_byte, \
         callee_start_byte, callee_end_byte, edge_kind, file_sha, tool, tool_version, \
         resolved_symbol_id, scip_symbol, kind, computed_at) VALUES ('a.rs', 0, 0, 14, 20, \
         'calls_name', 'sha-a', 'rust-analyzer', 'v', NULL, 's', 'upgrade', 0)",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM edges_data", []).unwrap();
    let remaining: i64 =
        conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(remaining, 1, "edge_oracle survives a full edges_data delete (no cascade)");
}

/// ENFORCING STRUCTURAL TRIP-WIRE (#248): the "can't happen again" guard for the whole bug CLASS,
/// rewritten to ENUMERATE EVERY table in a fully-migrated DB (via
/// [`cascading_fks_to_volatile_parents`]) rather than iterate a hand-maintained list. It asserts NO
/// table carries an `ON DELETE CASCADE`/`RESTRICT` FK to a reindex-VOLATILE parent
/// (`schema::REINDEX_VOLATILE_PARENTS`: `edges_data`, `symbols`, `logical_symbols`, the rowid-keyed
/// `files`) EXCEPT the explicit `schema::CASCADE_FK_ALLOWLIST` of `(child, parent)` pairs that are
/// rebuilt-with-their-parent and hold no oracle/durable state.
///
/// This is the exact check that would have FAILED on the original `edge_oracle`
/// `FOREIGN KEY(edge_id) REFERENCES edges_data(id) ON DELETE CASCADE` — the FK that silently wiped
/// every verdict on the first reindex. Crucially, because it scans `sqlite_master` (not
/// `ORACLE_PERSISTED_TABLES`), a FUTURE oracle/durable table that forgets to opt into any list
/// still FAILS automatically: the author must EITHER content-anchor it (no cascading FK) OR
/// consciously add it to the allowlist with a reason — and the allowlist is explicitly NOT for
/// durable state.
///
/// If this fails on a NEW table that genuinely holds oracle/durable output, that is ANOTHER
/// instance of the #248 bug to FIX (re-anchor on a content key + drop the FK), not to allowlist.
#[test]
fn no_table_has_a_reindex_cascading_fk_to_a_volatile_parent() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks())
        .expect("apply reaches LATEST");

    // Every declared oracle-derived table exists and is implicitly covered by the scan below (a
    // typo in the const would otherwise drift from reality unnoticed). The const stays the
    // canonical declaration of which outputs MUST survive reindex; the scan is what ENFORCES
    // the FK shape.
    for &table in rag_rat_db::schema::ORACLE_PERSISTED_TABLES {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "ORACLE_PERSISTED_TABLES lists `{table}` but it does not exist");
    }

    let disallowed: Vec<(String, String, String)> = cascading_fks_to_volatile_parents(&conn)
        .into_iter()
        .filter(|(table, parent, _)| {
            !rag_rat_db::schema::CASCADE_FK_ALLOWLIST.contains(&(table.as_str(), parent.as_str()))
        })
        .collect();

    assert!(
        disallowed.is_empty(),
        "table(s) carry a reindex-cascading FK to a volatile parent (the #248 bug class): \
         {disallowed:?}\noracle/durable outputs MUST survive reindex — content-key + no \
         reindex-cascading FK; reads join the live parent so dangling rows never resolve. If a \
         flagged table is genuinely ephemeral-with-its-parent (rebuilt with it, no durable \
         state), add (table, parent) to schema::CASCADE_FK_ALLOWLIST with a reason. Never \
         allowlist a table that holds oracle/durable state.",
    );

    // NEGATIVE SUB-ASSERTION (the trip-wire has teeth): a synthetic table WITH a cascading FK to a
    // volatile parent (`edges_data`) IS flagged by the scan — proving a future offender would not
    // slip through. Built on its own connection so the production scan above stays clean.
    let probe = rusqlite::Connection::open_in_memory().expect("open probe");
    rag_rat_db::schema::apply(&probe, &crate::index::migration_hooks())
        .expect("apply reaches LATEST");
    probe
        .execute_batch(
            "CREATE TABLE __trip_wire_probe__(
                 id INTEGER PRIMARY KEY,
                 x INTEGER,
                 FOREIGN KEY(x) REFERENCES edges_data(id) ON DELETE CASCADE
             );",
        )
        .unwrap();
    let probe_hits = cascading_fks_to_volatile_parents(&probe);
    assert!(
        probe_hits.iter().any(|(t, p, od)| t == "__trip_wire_probe__"
            && p == "edges_data"
            && od.eq_ignore_ascii_case("CASCADE")),
        "the scan must flag a synthetic CASCADE FK to edges_data — otherwise the trip-wire is \
         toothless; got {probe_hits:?}",
    );
}
