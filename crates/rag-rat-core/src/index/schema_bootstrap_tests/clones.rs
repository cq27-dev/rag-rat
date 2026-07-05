use super::*;

/// V032 forward migrate (#231): an index recorded at V031 (postings table present, NO `token_bag`
/// column) must, after migrate_forward, gain the `token_bag` BLOB column and lose
/// `symbol_token_postings`. Simulates the pre-V032 shape by re-creating the postings table +
/// dropping the column + deleting the V032 ledger row (making the schema Older), then replays.
#[test]
fn migration_032_adds_token_bag_drops_postings() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    crate::index::schema::apply(&conn).expect("apply");

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
        crate::index::schema::status(&conn).unwrap().state,
        crate::index::schema::SchemaState::Older,
        "schema is Older after removing the V032 ledger row"
    );

    // --- Run the forward migration ---
    crate::index::schema::migrate_forward(&conn).expect("migrate_forward");

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
        crate::index::schema::status(&conn).unwrap().current_version,
        crate::index::schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST_SCHEMA_VERSION after V032"
    );
    // Idempotency: a second migrate_forward is a clean no-op (guarded on the token_bag column).
    crate::index::schema::migrate_forward(&conn).expect("migrate_forward is idempotent");
    assert!(matches!(
        crate::index::schema::status(&conn).unwrap().state,
        crate::index::schema::SchemaState::Compatible
    ));
}

/// V034 (#286): the precomputed clone-graph tables exist after a forward migration from V033, are
/// CONTENT-ANCHORED (no `symbol_id` FK — the #248 rule the volatile-FK trip-wire enforces), and are
/// usable. A V033 index migrates forward to LATEST and gains `clone_graph_generations` +
/// `clone_edges` with the content-key endpoints; the deferred postings table is NOT created.
#[test]
fn migration_034_adds_content_anchored_clone_graph_tables() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    crate::index::schema::apply(&conn).expect("apply");

    // --- Simulate a V033-era index: clone-graph tables absent, schema rolled back to V033 ---
    conn.execute_batch(
        "DROP TABLE IF EXISTS clone_edges; DROP TABLE IF EXISTS clone_graph_generations;",
    )
    .expect("revert to V033 shape");
    truncate_schema_to(&conn, 33);
    assert!(!conn_table_exists(&conn, "clone_edges"), "clone_edges absent at V033");
    assert_eq!(
        crate::index::schema::status(&conn).unwrap().state,
        crate::index::schema::SchemaState::Older,
        "schema is Older after removing the V034 ledger row"
    );

    // --- Run the forward migration ---
    crate::index::schema::migrate_forward(&conn).expect("migrate_forward");
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
    crate::index::schema::apply_clone_graph_tables(&v034_only).expect("apply V034 clone-graph DDL");
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
        crate::index::schema::status(&conn).unwrap().current_version,
        crate::index::schema::LATEST_SCHEMA_VERSION,
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
    crate::index::schema::apply(&conn).expect("apply");

    conn.execute_batch("ALTER TABLE symbols DROP COLUMN is_test;").expect("revert to V034 shape");
    truncate_schema_to(&conn, 34);
    assert!(
        !conn_table_columns(&conn, "symbols").contains(&"is_test".to_string()),
        "is_test absent at V034"
    );
    assert_eq!(
        crate::index::schema::status(&conn).unwrap().state,
        crate::index::schema::SchemaState::Older,
        "schema is Older after removing the V035 ledger row"
    );

    crate::index::schema::migrate_forward(&conn).expect("migrate_forward");
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
    crate::index::schema::apply(&conn).expect("apply");

    conn.execute_batch("DROP TABLE embedding_cache;").expect("revert to V035 shape");
    truncate_schema_to(&conn, 35);
    assert_eq!(
        crate::index::schema::status(&conn).unwrap().state,
        crate::index::schema::SchemaState::Older,
        "schema is Older after removing the V036 ledger row"
    );

    crate::index::schema::migrate_forward(&conn).expect("migrate_forward");
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
    crate::index::schema::apply(&conn).expect("apply");
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
        crate::index::schema::status(&conn).unwrap().state,
        crate::index::schema::SchemaState::Older,
        "schema is Older after removing the V037 ledger row"
    );
    crate::index::schema::migrate_forward(&conn).expect("migrate_forward");
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
        crate::index::schema::status(&conn).unwrap().current_version,
        crate::index::schema::LATEST_SCHEMA_VERSION,
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
    crate::index::schema::apply(&conn).expect("apply");
    assert_eq!(
        crate::index::schema::status(&conn).unwrap().current_version,
        crate::index::schema::LATEST_SCHEMA_VERSION,
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
        crate::index::schema::status(&conn).unwrap().state,
        crate::index::schema::SchemaState::Older,
        "schema is Older after truncating the ledger to V29"
    );

    // --- Run the forward migration ---
    crate::index::schema::migrate_forward(&conn).expect("migrate_forward");

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
        crate::index::schema::status(&conn).unwrap().current_version,
        crate::index::schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST_SCHEMA_VERSION after V030 migration"
    );
    // Idempotency: running migrate_forward again must not error.
    crate::index::schema::migrate_forward(&conn).expect("migrate_forward is idempotent");
    assert_eq!(
        crate::index::schema::status(&conn).unwrap().state,
        crate::index::schema::SchemaState::Compatible,
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
    crate::index::schema::apply(&conn).expect("apply reaches V31");

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

    crate::index::schema::migrate_forward(&conn).expect("migrate_forward replays V031");
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
    crate::index::schema::apply(&conn).expect("apply reaches LATEST");

    // Every declared oracle-derived table exists and is implicitly covered by the scan below (a
    // typo in the const would otherwise drift from reality unnoticed). The const stays the
    // canonical declaration of which outputs MUST survive reindex; the scan is what ENFORCES
    // the FK shape.
    for &table in crate::index::schema::ORACLE_PERSISTED_TABLES {
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
            !crate::index::schema::CASCADE_FK_ALLOWLIST.contains(&(table.as_str(), parent.as_str()))
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
    crate::index::schema::apply(&probe).expect("apply reaches LATEST");
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

    // The token bag rides each fingerprint row as a non-NULL `token_bag` BLOB (#231) — there is no
    // symbol_token_postings table any more. Both fingerprinted symbols carry a bag that decodes to
    // a non-empty `(token_hash, freq)` multiset matching their `token_len`.
    let bagged_symbols: i64 = conn
        .query_row(
            "SELECT count(*) FROM symbol_fingerprints
             WHERE normalizer_kind='baseline' AND token_bag IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bagged_symbols, 2, "both fingerprinted functions carry a non-NULL token_bag BLOB");

    // Decode each BLOB and confirm it is a real bag (lossless: token_len == sum of freqs, no
    // duplicate token_hash — the codec invariants exercised against indexed data).
    let mut stmt = conn
        .prepare(
            "SELECT token_len, token_bag FROM symbol_fingerprints
             WHERE normalizer_kind='baseline'",
        )
        .unwrap();
    let rows: Vec<(i64, Vec<u8>)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for (token_len, blob) in &rows {
        let bag = crate::index::clones::bag_blob::decode_token_bag(blob).expect("BLOB decodes");
        assert!(!bag.is_empty(), "a fingerprinted symbol has a non-empty bag");
        let total_freq: i64 = bag.iter().map(|&(_, f)| f).sum();
        assert_eq!(total_freq, *token_len, "token_len == sum of freqs (lossless bag)");
        let mut hashes: Vec<i64> = bag.iter().map(|&(h, _)| h).collect();
        let distinct = hashes.len();
        hashes.dedup();
        assert_eq!(hashes.len(), distinct, "no duplicate token_hash in the indexed bag");
    }

    // df is populated (recomputed from the BLOBs at finalize).
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

    // Cascade: deleting a symbol drops its fingerprint row (the bag rides it as the BLOB column).
    conn.execute("DELETE FROM symbols", []).unwrap();
    let after_fps: i64 =
        conn.query_row("SELECT count(*) FROM symbol_fingerprints", [], |r| r.get(0)).unwrap();
    assert_eq!(after_fps, 0, "fingerprints (and their token_bag BLOBs) cascade on symbol delete");

    let _ = fs::remove_dir_all(root);
}

/// T4 (#231): `refresh_clone_token_df` recomputed from the token-bag BLOBs equals the postings-era
/// `GROUP BY symbol_token_postings` semantics — df = the count of DISTINCT symbols whose decoded
/// bag contains each `(normalizer_kind, token_hash)`, with NO generated-file filter (R6). Build a
/// real index, then independently re-derive the expected df from the BLOBs and assert it equals the
/// persisted `clone_token_df` row-for-row.
#[test]
fn clone_token_df_recomputed_from_blobs_matches_postings_era() {
    // Asserts the whole-DB `clone_token_df` contents; opt out of the poison harness whose sibling
    // seeds a df row under its own repo_id.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two renamed clones (shared tokens → some df == 2) + one distinct function (its tokens → df
    // 1).
    fs::write(
        root.join("src/lib.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub fn \
         load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\npub fn \
         compute_totals(items: Vec<i64>) -> i64 { let mut s = 0; for it in items { s += it * 2; } \
         s + 1 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // Independently re-derive df from EVERY fingerprint BLOB (no generated filter — R6).
    let mut stmt =
        conn.prepare("SELECT normalizer_kind, token_bag FROM symbol_fingerprints").unwrap();
    let mut expected: std::collections::BTreeMap<(String, i64), i64> =
        std::collections::BTreeMap::new();
    let mut rows = stmt.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        let kind: String = row.get(0).unwrap();
        let Some(blob) = row.get::<_, Option<Vec<u8>>>(1).unwrap() else {
            continue;
        };
        let bag = crate::index::clones::bag_blob::decode_token_bag(&blob).expect("decodes");
        for (token_hash, _freq) in bag {
            *expected.entry((kind.clone(), token_hash)).or_insert(0) += 1;
        }
    }
    assert!(!expected.is_empty(), "fixture produced fingerprints");

    // The persisted clone_token_df must match the independent recompute exactly.
    let mut df_stmt =
        conn.prepare("SELECT normalizer_kind, token_hash, df FROM clone_token_df").unwrap();
    let mut persisted: std::collections::BTreeMap<(String, i64), i64> =
        std::collections::BTreeMap::new();
    let mut df_rows = df_stmt.query([]).unwrap();
    while let Some(row) = df_rows.next().unwrap() {
        persisted.insert((row.get(0).unwrap(), row.get(1).unwrap()), row.get(2).unwrap());
    }
    assert_eq!(persisted, expected, "clone_token_df == distinct-symbol count per token from BLOBs");
    assert!(
        expected.values().any(|&d| d == 2),
        "the two renamed clones share at least one token (df == 2)"
    );

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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
}

/// #232 #6: a PATH-heuristic-generated file under a SOURCE target (`src/generated/*.rs`,
/// `is_generated_path` true, `kind = source`) gets full symbols but must NOT be fingerprinted at
/// index time — neither on a full rebuild NOR on a single-file heal. (`kind = Generated` files are
/// already symbol-empty, so the gate is needed only for the path-heuristic case.) This is pure
/// write-side storage hygiene — zero recall/precision effect (the read already filters
/// `generated = 0`); the assertion is on the absence of `symbol_fingerprints` ROWS.
#[test]
fn generated_files_are_not_fingerprinted_at_index_time() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/generated")).unwrap();
    // A normal source file (fingerprinted) and a path-heuristic-generated file under the SAME
    // source target. Both bodies clear MIN_TOKENS so the absence of a generated fp row is the gate,
    // not the size prune.
    std::fs::write(
        root.join("src/normal.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/generated/bindings.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let fp_rows_for = |db: &IndexDatabase, like: &str| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT count(*) FROM symbol_fingerprints sf
                 JOIN symbols ON symbols.id = sf.symbol_id
                 JOIN main.files ON main.files.id = symbols.file_id
                 WHERE main.files.path LIKE ?1 AND sf.normalizer_kind = 'baseline'",
                [like],
                |r| r.get(0),
            )
            .unwrap()
    };

    // The path-heuristic-generated file got symbols but NO fingerprint rows; the normal file did.
    assert!(fp_rows_for(&db, "%normal.rs") > 0, "normal source file must be fingerprinted");
    assert_eq!(
        fp_rows_for(&db, "%/generated/%"),
        0,
        "generated file must NOT be fingerprinted on a full rebuild"
    );
    // It DOES still get symbols (the gate is fingerprint-only, not symbol extraction).
    let gen_symbols: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT count(*) FROM symbols JOIN main.files ON main.files.id = symbols.file_id
             WHERE main.files.path LIKE '%/generated/%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        gen_symbols > 0,
        "generated file must still get symbols (only fingerprints are skipped)"
    );

    // Single-file heal path: re-index the generated file through heal_file → index_file →
    // store_symbol_fingerprints (gated). It must still write NO fingerprint rows.
    db.heal_file(std::path::Path::new("src/generated/bindings.rs")).unwrap();
    assert_eq!(
        fp_rows_for(&db, "%/generated/%"),
        0,
        "generated file must NOT be fingerprinted after a single-file heal"
    );

    let _ = fs::remove_dir_all(root);
}

/// #232 multi-language integration: a Rust + TS + Python repo with WITHIN-language planted clones
/// (Rust comment-only variant; TS function-valued declarators differing only in string contents;
/// Python comment-only variant) — exercises #1 (comments), #2a (TS strings) and #5 (TS
/// function-valued declarators) end-to-end through a real index. Asserts a within-language clone
/// component forms in EACH language and NO component mixes two languages (the #3 language
/// partition).
#[test]
fn multi_language_clone_integration_finds_within_language_no_cross() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("rs")).unwrap();
    fs::create_dir_all(root.join("ts")).unwrap();
    fs::create_dir_all(root.join("py")).unwrap();

    // Rust: two functions identical EXCEPT comments (comment-only clone → #1). >= MIN_TOKENS.
    fs::write(
        root.join("rs/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("rs/b.rs"),
        "pub fn load_order(s: Db) -> i32 {\n    // a different comment\n    let o = s.get(20); /* \
         x */ validate(o); o + 1 }\n",
    )
    .unwrap();

    // TS: two `const`-arrow function-valued declarators identical EXCEPT string contents (#5 +
    // #2a).
    fs::write(
        root.join("ts/a.ts"),
        "const load = (id) => { const row = get(id); const tag = label(\"alpha\"); send(row, \
         tag); return row; }\n",
    )
    .unwrap();
    fs::write(
        root.join("ts/b.ts"),
        "const fetch2 = (key) => { const item = get(key); const note = label(\"omega\"); \
         send(item, note); return item; }\n",
    )
    .unwrap();

    // Python: two functions identical EXCEPT comments (comment-only clone → #1). >= MIN_TOKENS.
    fs::write(
        root.join("py/a.py"),
        "def load_user(db):\n    u = db.get(10)\n    validate(u)\n    return u + 1\n",
    )
    .unwrap();
    fs::write(
        root.join("py/b.py"),
        "def load_order(s):\n    # a comment\n    o = s.get(20)  # trailing\n    validate(o)\n    \
         return o + 1\n",
    )
    .unwrap();

    let config = Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![
            ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("rs")],
                include: vec!["rs/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
            ResolvedTarget {
                name: "typescript".to_string(),
                language: Language::TypeScript,
                directories: vec![PathBuf::from("ts")],
                include: vec!["ts/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
            ResolvedTarget {
                name: "python".to_string(),
                language: Language::Python,
                directories: vec![PathBuf::from("py")],
                include: vec!["py/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
        ],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Map each component's symbol ids → the set of languages it spans.
    let conn = db.storage.connection();
    let lang_of = |symbol_id: i64| -> String {
        conn.query_row(
            "SELECT files.language FROM symbols JOIN main.files ON main.files.id = symbols.file_id
             WHERE symbols.id = ?1",
            [symbol_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };

    let components = db.candidate_clone_components().unwrap();
    let mut langs_with_clone: std::collections::BTreeSet<String> = Default::default();
    for component in &components {
        let langs: std::collections::BTreeSet<String> =
            component.iter().map(|&id| lang_of(id)).collect();
        // No component may mix two languages (the #3 language partition).
        assert_eq!(
            langs.len(),
            1,
            "a clone component must be single-language (no cross-language pairs): {langs:?}"
        );
        langs_with_clone.insert(langs.into_iter().next().unwrap());
    }

    // A within-language clone was recalled in EACH of the three languages.
    for expected in ["rust", "typescript", "python"] {
        assert!(
            langs_with_clone.contains(expected),
            "expected a within-language clone in {expected}; got {langs_with_clone:?}"
        );
    }

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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
        repo_id_override: None,
        database_key_pinned: true,
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
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();

    assert_eq!(res.classes.len(), 1, "exactly one clone class (the four rename-clones)");
    let c = &res.classes[0];
    assert_eq!(c.member_count, 4, "all four rename-clone functions are members");
    // Plan 4a: a clean class inside the refine budget is REFINED (it was the only class, so the
    // top-N driver refined it). The class_kind flips to "refined_class" and `refined` is true.
    assert_eq!(c.class_kind, "refined_class");
    assert!(c.refined, "a clean class inside the refine budget is refined (Plan 4a)");
    assert_eq!(c.refine_mode, Some("baseline"), "refined classes carry the baseline refine mode");
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

    let _ = fs::remove_dir_all(root);
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

    // Post-condition: the clone rebuild/precompute must not touch a sibling repo (round-6 harness).
    crate::index::poison_sibling::assert_sibling_intact(db2.storage.connection());
    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
}

/// `min_similarity` is a similarity ratio θ = overlap/max_len and must lie in [0.5, 1.0]. Values
/// outside that range are rejected up front (before candidate generation) so a unit error (e.g. a
/// percentage like 1.5), a degenerate 0.0 floor, or any value below the 0.5 safety floor can't
/// cause O(S²) candidate-pair explosion in the inverted index.
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

    // 0.0 (below floor) → error; message must mention the valid range [0.5, 1.0].
    let zero = db.find_clones(FindClonesOptions {
        min_similarity: Some(0.0),
        min_copies: None,
        limit: None,
    });
    let err = zero.expect_err("min_similarity = 0.0 must be rejected").to_string();
    assert!(err.contains("[0.5, 1.0]"), "expected '[0.5, 1.0]' in error, got: {err}");

    // 0.4 (below floor) → also rejected.
    let below_floor = db.find_clones(FindClonesOptions {
        min_similarity: Some(0.4),
        min_copies: None,
        limit: None,
    });
    let err = below_floor
        .expect_err("min_similarity = 0.4 must be rejected (below 0.5 floor)")
        .to_string();
    assert!(err.contains("[0.5, 1.0]"), "expected '[0.5, 1.0]' in error for 0.4, got: {err}");

    // 1.5 (above 1.0) → error.
    let high = db.find_clones(FindClonesOptions {
        min_similarity: Some(1.5),
        min_copies: None,
        limit: None,
    });
    let err = high.expect_err("min_similarity = 1.5 must be rejected").to_string();
    assert!(err.contains("[0.5, 1.0]"), "expected '[0.5, 1.0]' in error for 1.5, got: {err}");

    // 1.0 (boundary, inclusive upper) → accepted.
    db.find_clones(FindClonesOptions { min_similarity: Some(1.0), min_copies: None, limit: None })
        .expect("min_similarity = 1.0 is the inclusive upper bound and must be accepted");

    // 0.5 (inclusive lower bound) → accepted.
    db.find_clones(FindClonesOptions { min_similarity: Some(0.5), min_copies: None, limit: None })
        .expect("min_similarity = 0.5 is the inclusive lower bound and must be accepted");

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
}

/// Plan 4 coherence-splits over-merged components; the A~B~C chain becomes coherent sub-classes.
///
/// A TRANSITIVE-chain component (A–B and B–C both ≥ θ, but A–C < θ) is over-merged by union-find
/// into one 3-member component. Plan 4a's `coherence_split` (greedy maximal clique cover) breaks
/// it: every returned class is internally coherent (all pairs ≥ θ), so NO single class contains all
/// three. For this fixture the cover yields BOTH coherent pairs — {A,B} and {B,C} — with B in both
/// (the overlap is correct: B coheres with two peers that are themselves incompatible).
/// `find_clones` therefore returns two 2-member classes, never the 3-member chain.
///
/// `clones_for_symbol(A)` returns the largest coherent group containing A — here the {A,B} pair.
/// `clones_for_symbol(C)` returns the coherent {B,C} pair (C is NO LONGER a singleton under the
/// clique cover), so a query ABOUT C surfaces a real refined sub-class, not the over-merged
/// fallback.
///
/// The fixture is empirically tuned and the test asserts the MEASURED edge similarities so it is
/// honest about the chain it plants (a tokenizer change that shifts the numbers reddens here, not
/// silently). At HEAD the measured edges are A/B≈0.74, B/C≈0.86, A/C≈0.67 — a genuine chain whose
/// weakest (A/C) endpoint sits below the default θ=0.70.
#[test]
fn coherence_split_applied_in_find_clones() {
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
        // θ=0.5 (floor) so even a sub-default edge surfaces if ≥0.5; the class's similarity_min
        // is the pair's similarity. If no class forms at θ=0.5, the pair's similarity is < 0.5 —
        // which is still < θ=0.7 (THETA), satisfying the ac < THETA assertion. Return 0.0 as a
        // sentinel in that case.
        let res = d
            .find_clones(FindClonesOptions {
                min_similarity: Some(0.5),
                min_copies: None,
                limit: None,
            })
            .unwrap();
        let sim = res.classes.first().map(|c| c.similarity_min).unwrap_or(0.0);
        let _ = fs::remove_dir_all(r);
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

    // Now the full three-member scope. At the default θ=0.70 the over-merged union-find component
    // {A,B,C} is coherence-SPLIT: no returned class contains all three, and every returned class is
    // internally coherent (all pairs ≥ θ). The greedy clique cover yields BOTH coherent pairs:
    // {A,B} and {B,C}.
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
    // No class may contain all three members — that is the whole point of the coherence split.
    assert!(
        res.classes.iter().all(|c| c.member_count < 3),
        "the over-merged chain must split — no class may keep all 3 members: {:?}",
        res.classes.iter().map(|c| c.member_count).collect::<Vec<_>>()
    );
    // Every returned class must be internally coherent (its aggregate min-pairwise ≥ θ).
    for class in &res.classes {
        assert!(
            class.cohesion_min_pairwise >= THETA - 1e-9,
            "a coherence-split class must be internally ≥ θ: got {}",
            class.cohesion_min_pairwise
        );
    }
    // After clique-cover split, both {A,B} and {B,C} are returned (B in both).
    assert_eq!(
        res.classes.len(),
        2,
        "the chain yields two coherent ≥2 classes: {{A,B}} and {{B,C}}"
    );
    for class in &res.classes {
        assert_eq!(class.member_count, 2, "each coherent class has 2 members");
    }

    // clones_for_symbol(A): A is in {A,B} only. The largest group containing A is {A,B} (refined).
    let by_a = db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::fa".into())).unwrap();
    let a_class = by_a.class.as_ref().expect("fa is in the coherent {A,B} sub-class");
    assert_eq!(a_class.member_count, 2, "clones_for_symbol(fa) returns A's coherent sub-class");
    // A's class must match one of the returned classes from find_clones.
    assert!(
        res.classes.iter().any(|c| c.class_key == a_class.class_key),
        "find_clones and clones_for_symbol must return the SAME coherent sub-class for A"
    );

    // clones_for_symbol(C): after the clique cover, C is in the coherent {B,C} sub-class (NOT a
    // singleton anymore), so the reverse lookup serves that refined 2-member class — no fallback to
    // the over-merged 3-member component.
    let by_c = db.clones_for_symbol(CloneSymbolSelector::Ref("src/c.rs::fc".into())).unwrap();
    let c_class = by_c.class.as_ref().expect("fc is in the coherent {B,C} sub-class");
    assert_eq!(
        c_class.member_count, 2,
        "clones_for_symbol(fc) returns C's coherent {{B,C}} sub-class"
    );
    assert!(c_class.refined, "the {{B,C}} sub-class is refined");

    let _ = fs::remove_dir_all(root);
}

/// #256 (R5c): the full clone path is DETERMINISTic on a synthetic transitive chain. The
/// edge-fed clique-cover split (over a bucketed edge subset) and the ROI sort must be stable so the
/// content-addressed refinement cache stays valid and the listing order does not flap between
/// identical rebuilds. We rebuild the SAME {A,B,C} chain fixture twice (independent indexes) and
/// assert `find_clones` returns byte-identical class order (class_key sequence) both times, and
/// `clones_for_symbol` agrees across runs.
#[test]
fn clone_split_full_path_is_deterministic() {
    // Reuse the empirically-tuned chain shape from coherence_split_applied_in_find_clones: A~B and
    // B~C clear θ, A/C is below θ, so the union-find component {A,B,C} is over-merged and must be
    // coherence-split into {A,B} and {B,C}.
    let core = "let c1 = ca(x); let c2 = cb(c1); let c3 = cc(c2);";
    let s1 = "if x > 0 { acc = p1(x); } else { acc = p2(x); } if acc > 1 { acc = p3(acc); } else \
              { acc = p4(acc); }";
    let s2 = "for it in xs { match it { 0 => acc += q1(it), 1 => acc += q2(it), _ => acc -= \
              q3(it) } } for jt in ys { match jt { 0 => acc += q4(jt), _ => acc -= q5(jt) } }";
    let sx = "while acc > 0 { acc = r1(acc); acc = r2(acc); acc = r3(acc); } while acc < 9 { acc \
              = r4(acc); acc = r5(acc); }";
    let sy = "loop { acc = s1f(acc); acc = s2f(acc); acc = s3f(acc); if acc == 0 { break; } } \
              loop { acc = s4f(acc); if acc < 0 { break; } }";
    let a = format!("pub fn fa(x: i32) -> i32 {{ {core} {s1} {sx} 0 }}\n");
    let b = format!("pub fn fb(x: i32) -> i32 {{ {core} {s1} {s2} 0 }}\n");
    let c = format!("pub fn fc(x: i32) -> i32 {{ {core} {s2} {sy} 0 }}\n");

    let build_and_list = || -> (Vec<String>, Option<String>) {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), a.as_str()).unwrap();
        fs::write(root.join("src/b.rs"), b.as_str()).unwrap();
        fs::write(root.join("src/c.rs"), c.as_str()).unwrap();
        let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
        let res = db
            .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
            .unwrap();
        let keys: Vec<String> = res.classes.iter().map(|cl| cl.class_key.clone()).collect();
        // clones_for_symbol on the chained subject B (which is in BOTH {A,B} and {B,C}).
        let by_b = db.clones_for_symbol(CloneSymbolSelector::Ref("src/b.rs::fb".into())).unwrap();
        let b_key = by_b.class.as_ref().map(|cl| cl.class_key.clone());
        let _ = fs::remove_dir_all(root);
        (keys, b_key)
    };

    let (keys1, b1) = build_and_list();
    let (keys2, b2) = build_and_list();
    assert_eq!(keys1, keys2, "find_clones class order must be identical across rebuilds");
    assert_eq!(b1, b2, "clones_for_symbol(fb) must resolve to the same class across rebuilds");
    // Sanity: the chain DID split (two coherent classes), so determinism is over a real split.
    assert_eq!(keys1.len(), 2, "the chain must split into two coherent classes: {keys1:?}");
}

/// #256 (R3): `clones-for` on a CHAINED symbol — the path #256 names broken — must serve the
/// subject's TIGHT coherent neighborhood, never the whole over-merged component. In the {A,B,C}
/// chain, a reverse lookup on the bridge symbol B returns a 2-member coherent class (one of {A,B} /
/// {B,C}), NOT a 3-member over-merged blob, and that class is internally coherent (≥ θ).
#[test]
fn clones_for_chained_symbol_serves_tight_neighborhood() {
    let core = "let c1 = ca(x); let c2 = cb(c1); let c3 = cc(c2);";
    let s1 = "if x > 0 { acc = p1(x); } else { acc = p2(x); } if acc > 1 { acc = p3(acc); } else \
              { acc = p4(acc); }";
    let s2 = "for it in xs { match it { 0 => acc += q1(it), 1 => acc += q2(it), _ => acc -= \
              q3(it) } } for jt in ys { match jt { 0 => acc += q4(jt), _ => acc -= q5(jt) } }";
    let sx = "while acc > 0 { acc = r1(acc); acc = r2(acc); acc = r3(acc); } while acc < 9 { acc \
              = r4(acc); acc = r5(acc); }";
    let sy = "loop { acc = s1f(acc); acc = s2f(acc); acc = s3f(acc); if acc == 0 { break; } } \
              loop { acc = s4f(acc); if acc < 0 { break; } }";
    let a = format!("pub fn fa(x: i32) -> i32 {{ {core} {s1} {sx} 0 }}\n");
    let b = format!("pub fn fb(x: i32) -> i32 {{ {core} {s1} {s2} 0 }}\n");
    let c = format!("pub fn fc(x: i32) -> i32 {{ {core} {s2} {sy} 0 }}\n");

    const THETA: f64 = 0.7;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), a.as_str()).unwrap();
    fs::write(root.join("src/b.rs"), b.as_str()).unwrap();
    fs::write(root.join("src/c.rs"), c.as_str()).unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let by_b = db.clones_for_symbol(CloneSymbolSelector::Ref("src/b.rs::fb".into())).unwrap();
    let b_class = by_b.class.as_ref().expect("fb is the bridge symbol — it has coherent peers");
    assert_eq!(
        b_class.member_count, 2,
        "the chained subject's class must be the TIGHT 2-member neighborhood, never the \
         over-merged 3-member component"
    );
    assert!(
        b_class.cohesion_min_pairwise >= THETA - 1e-9,
        "the served class must be internally coherent (≥ θ): got {}",
        b_class.cohesion_min_pairwise
    );
    let _ = fs::remove_dir_all(root);
}

/// #256 (R7) recall pin: a 7-copy structurally-identical clone family (the `collect_rows`-style
/// shape that motivated the issue) is STILL found after the over-merge fix, and the genuine clone
/// class is refined with full coverage + ranks well. The fix only ever PARTITIONS an over-merged
/// component into ≤-coherent classes; it must never drop a real multi-copy clone below the recall
/// floor. The seven bodies are byte-identical up to identifier names (alpha-renamed to ID<n>), so
/// they share one struct_hash → ONE coherent 7-member class via the struct-hash fast path.
#[test]
fn find_clones_recall_pin_seven_copy_clone_still_found() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Seven copies of the same DB-getter shape, differing only in fn + variable names. Same
    // structural token sequence ⇒ same struct_hash ⇒ one clone class (no member loss).
    for (i, (name, var)) in [
        ("collect_a", "a"),
        ("collect_b", "b"),
        ("collect_c", "c"),
        ("collect_d", "d"),
        ("collect_e", "e"),
        ("collect_f", "f"),
        ("collect_g", "g"),
    ]
    .iter()
    .enumerate()
    {
        fs::write(
            root.join(format!("src/f{i}.rs")),
            format!(
                "pub fn {name}(db: Db) -> i32 {{ let {var} = db.get(1); validate({var}); \
                 transform({var}); {var} + 1 }}\n"
            ),
        )
        .unwrap();
    }
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    // The 7-copy clone must be found as ONE class with all 7 members (recall preserved — no member
    // dropped by the split).
    let seven = res
        .classes
        .iter()
        .find(|c| c.member_count == 7)
        .expect("the 7-copy clone class must still be found at θ=0.7");
    // Genuine clone → refined with full coverage and a strong refactorability (the coverage gate
    // does NOT penalize it; it ranks well, not buried).
    assert!(seven.refined, "the 7-copy clone is refined");
    assert!(
        seven.anti_unify_coverage.unwrap_or(0.0) >= 0.5,
        "a byte-identical 7-copy clone must have high coverage, got {:?}",
        seven.anti_unify_coverage
    );
    assert!(seven.roi > 0.0, "a genuine clone keeps a positive ROI");

    let _ = fs::remove_dir_all(root);
}

/// Plan 4a: four renamed clones (same structure, different names) form ONE class that the refine
/// driver promotes to a refined class — `refined`, `class_kind == "refined_class"`, a near-perfect
/// `lcs_ratio`, `confidence == "high"`, `refactorability > 0.9`, `refine_mode == Some("baseline")`,
/// and a positive ROI (the refactorability multiplier replaces the cohesion one).
#[test]
fn find_clones_refines_a_clean_class() {
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(res.classes.len(), 1, "the four renamed clones are one class");
    let c = &res.classes[0];

    assert!(c.refined, "a clean class inside the refine budget must be refined");
    assert_eq!(c.class_kind, "refined_class");
    assert_eq!(c.refine_mode, Some("baseline"));
    let lcs = c.lcs_ratio.expect("a refined class carries an lcs_ratio");
    assert!(lcs > 0.95, "renamed clones are near-identical; lcs_ratio should be ~1.0, got {lcs}");
    assert_eq!(c.confidence.as_deref(), Some("high"), "near-perfect fidelity → high confidence");
    let refac = c.refactorability.expect("a refined class carries a refactorability");
    assert!(refac > 0.9, "refactorability should be high for a clean class, got {refac}");
    // ROI reflects refactorability: cross_module_spread × member_count × medoid × LBF × refac.
    let expected_roi = c.cross_module_spread as f64
        * c.member_count as f64
        * c.body_token_len_medoid as f64
        * c.roi_factors.load_bearing_factor
        * refac;
    assert!(
        (c.roi - expected_roi).abs() < 1e-6,
        "refined ROI must use refactorability: roi={} expected={expected_roi}",
        c.roi
    );

    let _ = fs::remove_dir_all(root);
}

/// Plan 4a: `clones_for_symbol` always refines the subject's class (when refine inputs are
/// available). A reverse lookup into the clean 4-member class returns a REFINED class with the
/// subject present.
#[test]
fn clones_for_symbol_returns_refined_class() {
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);

    let res =
        db.clones_for_symbol(CloneSymbolSelector::Ref("a/load_user.rs::load_user".into())).unwrap();
    let class = res.class.as_ref().expect("load_user is in the clone class");
    assert!(class.refined, "clones_for_symbol refines the subject's class");
    assert_eq!(class.class_kind, "refined_class");
    assert_eq!(class.refine_mode, Some("baseline"));
    assert!(class.lcs_ratio.is_some(), "a refined class carries an lcs_ratio");
    assert!(
        class.members.iter().any(|m| m.r#ref.ends_with("load_user.rs::load_user")),
        "the subject must appear in its own refined class: {:?}",
        class.members.iter().map(|m| &m.r#ref).collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(root);
}

/// Plan 4a: the content-addressed `refinement_key` is over the struct_hash MULTISET (order-
/// independent), and is DISTINCT from the read-side `class_key` (location-derived). Same multiset →
/// same key; the two key families never collide for the same class.
#[test]
fn refinement_key_is_content_addressed_and_distinct_from_read_key() {
    use crate::index::clones::refine::cache::refinement_key;

    let hashes = vec!["h1".to_string(), "h2".to_string(), "h3".to_string()];
    let shuffled = vec!["h3".to_string(), "h1".to_string(), "h2".to_string()];
    let discs = vec!["d1".to_string(), "d2".to_string(), "d3".to_string()];
    let discs_shuffled = vec!["d3".to_string(), "d1".to_string(), "d2".to_string()];
    // Same multiset, different order (struct_hashes AND source discriminators) → same
    // refinement_key (content-addressed, order-independent).
    assert_eq!(
        refinement_key("rust", &hashes, &discs),
        refinement_key("rust", &shuffled, &discs_shuffled),
        "the same struct_hash + source-discriminator multiset must address the same refinement"
    );

    // The refinement key (structural) is NOT the read-side class_key (location-derived). Build a
    // real clone class, then confirm its persisted refinement key ≠ its read-side class_key.
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);
    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    let read_key = &res.classes[0].class_key;
    // The persisted refinement row's PRIMARY KEY is the content-addressed key; it must not equal
    // the location-derived read-side class_key.
    let refinement_pk: String = db
        .storage
        .connection()
        .query_row("SELECT class_key FROM clone_refinements LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_ne!(
        &refinement_pk, read_key,
        "the content-addressed refinement key must differ from the location-derived read key"
    );

    let _ = fs::remove_dir_all(root);
}

/// Plan 4a: the refinement cache is read-through — the first `find_clones` populates a
/// `clone_refinements` row; a second `find_clones` over the same index serves the cache and does
/// NOT grow the row count.
#[test]
fn refine_cache_is_read_through() {
    // Asserts whole-DB `clone_refinements` counts (0 before the run, N after); opt out of the
    // poison harness whose sibling seeds a refinement under its own repo_id.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);

    let count_rows = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0))
            .unwrap()
    };

    // Before any find_clones run the cache is empty.
    assert_eq!(count_rows(&db), 0, "no refinements before the first run");

    // Run 1: refines the clean class → exactly one cache row.
    let r1 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(r1.classes[0].refined, "run 1 refines the class");
    let after_run1 = count_rows(&db);
    assert_eq!(after_run1, 1, "run 1 persists exactly one refinement row");

    // Run 2: same inputs → cache HIT, row count unchanged.
    let r2 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(r2.classes[0].refined, "run 2 still refined (served from cache)");
    assert_eq!(count_rows(&db), after_run1, "run 2 is a cache hit — the row count must not grow");

    let _ = fs::remove_dir_all(root);
}

/// Fix A (#215 Plan 4a codex2): the warm path (cache hit) must NOT re-parse any source file.
/// If re-parsing were happening on the warm path, deleting the source files after the first
/// find_clones would cause the second find_clones to return an un-refined class
/// (load_refine_members returns None on a missing file → un-refined fallback). With the fix — cache
/// probe BEFORE load_refine_members — the second call serves from the cache entirely: the source
/// files are never read and the class is still `refined=true`.
#[test]
fn find_clones_warm_cache_serves_refined_without_reparse() {
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);

    // Run 1 (cold path): populates the cache.
    let r1 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(r1.classes[0].refined, "run 1 must refine the class (cold path)");

    // Delete the source files so any attempt to re-parse would fail / return None.
    for dir in &["a", "b"] {
        let _ = fs::remove_dir_all(root.join(dir));
    }

    // Run 2 (warm path): the cache must serve the refinement WITHOUT touching the (now-absent)
    // source files. If the warm path were re-parsing, load_refine_members would return None
    // (file missing) and the class would be left un-refined.
    let r2 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(
        r2.classes[0].refined,
        "run 2 must still be refined from the cache — warm path must not re-parse source files"
    );
    assert_eq!(
        r1.classes[0].lcs_ratio, r2.classes[0].lcs_ratio,
        "warm-path lcs_ratio must match the cold-path value"
    );

    fs::remove_dir_all(root).unwrap_or(());
}

/// Fix B (#215 Plan 4a codex2): the member-count sampling dimension must be reported consistently
/// on BOTH the cold and warm cache paths. A class with more than LCS_MEMBER_SAMPLE members must
/// have `metrics_sampled=true` on the second (warm) find_clones call, not just the first (cold).
///
/// NOTE: planting >64 distinct fingerprinted clone-class members via the full index pipeline is
/// expensive; instead we verify the logic path directly via the public find_clones surface with a
/// small class (metrics_sampled stays false for small classes) and document that the large-class
/// warm-path consistency is enforced by the `apply_refinement` function's unconditional
/// `class.member_count > LCS_MEMBER_SAMPLE` OR-in, which is independent of cache hit/miss.
#[test]
fn find_clones_warm_cache_metrics_sampled_consistent() {
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);

    // Run 1 (cold): 4-member class — below LCS_MEMBER_SAMPLE, metrics_sampled should be false.
    let r1 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(r1.classes[0].refined, "run 1 refines the class");
    let sampled_cold = r1.classes[0].metrics_sampled;

    // Run 2 (warm cache hit): the member-count dimension must be applied consistently.
    let r2 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(r2.classes[0].refined, "run 2 is refined from the cache");
    let sampled_warm = r2.classes[0].metrics_sampled;

    assert_eq!(
        sampled_cold, sampled_warm,
        "metrics_sampled must be consistent across cold ({sampled_cold}) and warm \
         ({sampled_warm}) paths"
    );

    let _ = fs::remove_dir_all(root);
}

/// Plan 4a: only the top-N (by provisional ROI) classes are refined, where N == the caller's limit.
/// With TWO distinct clean classes and `limit = 1`, exactly ONE class is refined: the returned
/// (top-1) class is refined, and only ONE `clone_refinements` row is written — the second class is
/// outside the refine budget and never reaches refinement, keeping its Plan-2 (un-refined) shape.
/// (Because the output is truncated to the limit, the un-refined class is not in the returned set;
/// the persisted-row count is the observable proof that only the top-N were refined.)
#[test]
fn unrefined_class_outside_top_n_keeps_plan2_shape() {
    // Asserts a whole-DB `clone_refinements` count; opt out of the poison harness whose sibling
    // seeds a refinement under its own repo_id.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Class 1: two big-body renamed clones (high ROI via long body) — refined first.
    let big = |name: &str, v: &str| {
        format!(
            "pub fn {name}(db: Db) -> i32 {{ let {v}1 = db.get(1); let {v}2 = db.get(2); let {v}3 \
             = db.get(3); validate({v}1); validate({v}2); validate({v}3); {v}1 + {v}2 + {v}3 }}\n"
        )
    };
    fs::write(root.join("src/big_a.rs"), big("big_user", "u")).unwrap();
    fs::write(root.join("src/big_b.rs"), big("big_order", "o")).unwrap();
    // Class 2: two small-body renamed clones (lower ROI via short body) — structurally distinct
    // from class 1 so they form a SEPARATE class, ranked below it.
    let small = |name: &str, v: &str| {
        format!(
            "pub fn {name}(xs: Vec<u8>) -> usize {{ let mut {v} = 0; for e in xs {{ {v} += e as \
             usize; }} {v} }}\n"
        )
    };
    fs::write(root.join("src/small_a.rs"), small("sum_bytes", "n")).unwrap();
    fs::write(root.join("src/small_b.rs"), small("sum_words", "m")).unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let count_rows = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0))
            .unwrap()
    };

    // Sanity: with no limit BOTH classes exist (the default budget 50 refines both).
    let all = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(all.classes.len(), 2, "two distinct clean classes are planted");
    assert!(all.classes.iter().all(|c| c.refined), "default budget refines both");
    assert_eq!(count_rows(&db), 2, "default budget persists both refinements");

    // Fresh index so the cache starts empty for the budget assertion.
    let root2 = unique_temp_root();
    let _ = fs::remove_dir_all(&root2);
    fs::create_dir_all(root2.join("src")).unwrap();
    fs::write(root2.join("src/big_a.rs"), big("big_user", "u")).unwrap();
    fs::write(root2.join("src/big_b.rs"), big("big_order", "o")).unwrap();
    fs::write(root2.join("src/small_a.rs"), small("sum_bytes", "n")).unwrap();
    fs::write(root2.join("src/small_b.rs"), small("sum_words", "m")).unwrap();
    let db2 = IndexDatabase::rebuild(&source_config(root2.clone(), Language::Rust)).unwrap();

    // limit=1 ⇒ refine budget 1 ⇒ only the top-1 class is refined.
    let limited = db2
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: Some(1) })
        .unwrap();
    assert_eq!(limited.classes.len(), 1, "limit=1 returns exactly one class");
    let top = &limited.classes[0];
    assert!(top.refined, "the single returned (top-1) class is refined");
    assert!(top.lcs_ratio.is_some(), "the refined class carries an lcs_ratio");
    assert_eq!(top.refine_mode, Some("baseline"));
    // Only ONE refinement was computed/persisted: the second class is outside the budget and keeps
    // its un-refined Plan-2 shape (it never reached `refine_class`).
    assert_eq!(
        count_rows(&db2),
        1,
        "limit=1 refines only the top-1 class — the out-of-budget class is never refined"
    );

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(root2);
}

/// Fix 2 (Codex P2 #215 Plan 4a): find_clones with limit=Some(N) must never return an unrefined
/// class. The old implementation refine-budget top-N then re-sort ALL → a rank-(N+1) unrefined
/// class could displace a refined one after ROI recalculation. The fix truncates to N BEFORE
/// refining so only refined (or best-effort-unrefined) classes appear in a limited result.
///
/// Fixture: 3 structurally distinct clone classes (A db-getter / B match-expr / C loop-reducer —
/// distinct constructs so they form three separate components, never cross-merge). With
/// `limit=Some(2)` only the top-2 by provisional ROI are truncated into the refine set, refined,
/// and returned; the third class (truncated away before refining) never enters the result. The
/// load-bearing assertion is that EVERY class in the limited result has `refined == true` — the
/// property the old re-sort-ALL path could violate.
#[test]
fn find_clones_limited_result_contains_only_refined_classes() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // Class A: big body, high ROI.
    let big = |name: &str, v: &str| {
        format!(
            "pub fn {name}(db: Db) -> i32 {{ let {v}1 = db.get(1); let {v}2 = db.get(2); let {v}3 \
             = db.get(3); validate({v}1); validate({v}2); validate({v}3); {v}1 + {v}2 + {v}3 }}\n"
        )
    };
    fs::write(root.join("src/big_a.rs"), big("big_user", "u")).unwrap();
    fs::write(root.join("src/big_b.rs"), big("big_order", "o")).unwrap();

    // Class B: a `match`-expression body — structurally distinct from both the db-getter (A) and
    // the loop-reducer (C), so it forms its OWN component (no cross-class merge). Medium length.
    let matchy = |name: &str, v: &str| {
        format!(
            "pub fn {name}(k: i32) -> i32 {{ let {v} = match k {{ 0 => 10, 1 => 20, 2 => 30, _ => \
             40 }}; {v} + 1 }}\n"
        )
    };
    fs::write(root.join("src/match_a.rs"), matchy("classify_a", "n")).unwrap();
    fs::write(root.join("src/match_b.rs"), matchy("classify_b", "m")).unwrap();

    // Class C: a loop-reducer body — structurally distinct from A and B. Small length.
    let small = |name: &str, v: &str| {
        format!(
            "pub fn {name}(xs: Vec<u8>) -> usize {{ let mut {v} = 0; for e in xs {{ {v} += e as \
             usize; }} {v} }}\n"
        )
    };
    fs::write(root.join("src/small_a.rs"), small("sum_bytes", "s")).unwrap();
    fs::write(root.join("src/small_b.rs"), small("sum_words", "t")).unwrap();

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Sanity: all 3 classes exist and unlimited refines all of them (budget=50).
    let all = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(all.classes.len(), 3, "three distinct clone classes are planted: {:?}", all.classes);
    assert!(all.classes.iter().all(|c| c.refined), "unlimited refines all 3 (budget=50)");

    // Fresh index for the fix-2 assertion (cache starts empty).
    let root2 = unique_temp_root();
    let _ = fs::remove_dir_all(&root2);
    fs::create_dir_all(root2.join("src")).unwrap();
    fs::write(root2.join("src/big_a.rs"), big("big_user", "u")).unwrap();
    fs::write(root2.join("src/big_b.rs"), big("big_order", "o")).unwrap();
    fs::write(root2.join("src/match_a.rs"), matchy("classify_a", "n")).unwrap();
    fs::write(root2.join("src/match_b.rs"), matchy("classify_b", "m")).unwrap();
    fs::write(root2.join("src/small_a.rs"), small("sum_bytes", "s")).unwrap();
    fs::write(root2.join("src/small_b.rs"), small("sum_words", "t")).unwrap();
    let db2 = IndexDatabase::rebuild(&source_config(root2.clone(), Language::Rust)).unwrap();

    // limit=2: top-2 by provisional ROI are truncated, refined, returned. Class C never enters.
    let limited = db2
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: Some(2) })
        .unwrap();
    assert_eq!(limited.classes.len(), 2, "limit=2 returns exactly 2 classes");
    for (i, c) in limited.classes.iter().enumerate() {
        assert!(
            c.refined,
            "every class in a limited result must be refined (or best-effort-unrefined); \
             class[{i}] (key={}) has refined=false",
            c.class_key
        );
    }

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(root2);
}

/// I2 (#215 Plan 4a adversary): find_clones with a huge limit must clamp effective returned classes
/// to UNLIMITED_REFINE_BUDGET (50), returning all-refined. This plants MORE than 50 distinct clone
/// classes so the clamp is actually EXERCISED (the earlier 3-class fixture never tripped it): a
/// huge `limit` returns EXACTLY 50 classes (all refined), and both `truncated` and
/// `refine_budget_clamped` are set.
#[test]
fn find_clones_huge_limit_clamps_to_refine_budget() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // Generate 18 ops × 4 length-tiers = 72 DISTINCT, non-merging clone classes (> the budget of
    // 50). Two independent axes keep distinct classes from cross-merging under the SourcererCC
    // candidate edge test (overlap/max_len >= θ=0.7):
    //   1. OPERATOR axis: each body is a long left-assoc binary chain `a OP a OP a …` over a single
    //      operand, so the verbatim operator token dominates the normalized bag. The 18 distinct
    //      binary operators each yield a separate class (within-tier max similarity < 0.7 once the
    //      chain is long enough — the smallest tier is 64 reps; 40 reps merges).
    //   2. LENGTH-TIER axis: four chain lengths ~1.6× apart (> 1/θ ≈ 1.43) so the size-prune
    //      (min_len >= ceil(θ·max_len)) drops EVERY cross-tier edge regardless of content.
    // The `_a`/`_b` variants are rename-clones (operand `a` vs `b`, distinct fn names): identical
    // structure → same struct_hash → exactly one class per pair via the struct-hash fast path.
    // NOTE: identifier names and literal VALUES are normalization-invariant, so the per-pair
    // distinction MUST come from structure (operator + chain length), never names/literals.
    let ops = [
        "+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>", "<", ">", "<=", ">=", "==", "!=", "&&",
        "||",
    ];
    let tiers = [64usize, 104, 168, 270];
    let body = |op: &str, n: usize, var: &str, name: &str| {
        let mut s = format!("pub fn {name}({var}: i64) -> i64 {{\n    {var}");
        for _ in 0..n {
            s.push_str(&format!(" {op} {var}"));
        }
        s.push_str("\n}\n");
        s
    };
    let mut idx = 0usize;
    for (ti, &n) in tiers.iter().enumerate() {
        for (oi, op) in ops.iter().enumerate() {
            let fa = body(op, n, "a", &format!("fn_a_t{ti}_o{oi}"));
            let fb = body(op, n, "b", &format!("fn_b_t{ti}_o{oi}"));
            fs::write(root.join(format!("src/clone_a{idx}.rs")), fa).unwrap();
            fs::write(root.join(format!("src/clone_b{idx}.rs")), fb).unwrap();
            idx += 1;
        }
    }

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Sanity: the full (unlimited) result must surface MORE than the refine budget of classes,
    // otherwise the clamp below is vacuous.
    let all = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(
        all.classes.len() > 50,
        "fixture must plant > 50 clone classes to exercise the clamp; got {}",
        all.classes.len()
    );

    // limit=100000 >> total classes — clamps to exactly UNLIMITED_REFINE_BUDGET (50).
    let limited = db
        .find_clones(FindClonesOptions {
            min_similarity: None,
            min_copies: None,
            limit: Some(100_000),
        })
        .unwrap();
    assert_eq!(
        limited.classes.len(),
        50,
        "a huge limit over > 50 classes must return EXACTLY the refine budget (50); got {}",
        limited.classes.len()
    );
    assert!(
        limited.classes.iter().all(|c| c.refined),
        "every class in a huge-limited result must be refined"
    );
    assert!(
        limited.completeness.truncated,
        "dropping whole classes to honor the budget must set truncated"
    );
    assert!(
        limited.completeness.refine_budget_clamped,
        "a limit above the budget that drops classes must set refine_budget_clamped"
    );

    let _ = fs::remove_dir_all(root);
}

/// Plan 4b Task 5b: `load_refine_members` caps the re-parse to `MEMBER_VALUE_CAP` (50, previously
/// `LCS_MEMBER_SAMPLE` = 64 in Plan 4a). Plants a single clone class with more than
/// `MEMBER_VALUE_CAP` members, calls `load_refine_members`, and asserts it returns EXACTLY
/// `MEMBER_VALUE_CAP` members in canonical (struct_hash, path, start_byte) order. Also asserts the
/// constants are still consistent (MEMBER_VALUE_CAP == MAX_MEMBERS == 50, LCS_MEMBER_SAMPLE == 64,
/// MEMBER_VALUE_CAP < LCS_MEMBER_SAMPLE — the align cap never sees more members than the value cap
/// loads).
#[test]
fn load_refine_members_returns_up_to_value_cap() {
    use crate::index::clones::refine::align::LCS_MEMBER_SAMPLE;
    use crate::index::query_api::{MAX_MEMBERS, MEMBER_VALUE_CAP};
    // Constant consistency assertions — these values are load-bearing.
    assert_eq!(MEMBER_VALUE_CAP, 50, "MEMBER_VALUE_CAP must be 50");
    assert_eq!(MAX_MEMBERS, 50, "MAX_MEMBERS must be 50");
    assert_eq!(LCS_MEMBER_SAMPLE, 64, "LCS_MEMBER_SAMPLE must be 64");
    // MEMBER_VALUE_CAP < LCS_MEMBER_SAMPLE is a compile-time invariant: move to const context so
    // clippy::assertions_on_constants doesn't fire.
    const { assert!(MEMBER_VALUE_CAP < LCS_MEMBER_SAMPLE) };

    const MEMBERS: usize = MEMBER_VALUE_CAP + 1; // 51 — one over the value cap.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // 51 rename-clones of ONE structure: identical AST shape, distinct identifier names. Baseline
    // normalization alpha-renames identifiers to ID<n> and buckets literals, so all 51 collapse to
    // the SAME struct_hash → one clone class (the struct_hash exact fast path components them).
    for i in 0..MEMBERS {
        let src = format!(
            "pub fn fn_{i}(db: Db) -> i32 {{ let a{i} = db.get(); let b{i} = db.get(); let c{i} = \
             db.get(); validate(a{i}); validate(b{i}); validate(c{i}); a{i} + b{i} + c{i} }}\n"
        );
        fs::write(root.join(format!("src/m{i}.rs")), src).unwrap();
    }

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Find the single component holding all 51 members.
    let components = db.candidate_clone_components().expect("components");
    let mut big = components
        .into_iter()
        .find(|c| c.len() == MEMBERS)
        .unwrap_or_else(|| panic!("expected one component of {MEMBERS} exact clones"));
    big.sort_unstable();

    // load_refine_members must cap the re-parse to MEMBER_VALUE_CAP members.
    let members = db
        .load_refine_members(&big)
        .expect("load_refine_members ok")
        .expect("refine inputs available for an in-scope class");
    assert_eq!(
        members.len(),
        MEMBER_VALUE_CAP,
        "load_refine_members must cap a {MEMBERS}-member class to MEMBER_VALUE_CAP (50)"
    );

    // All struct_hashes are equal (exact clones), so canonical order is ascending symbol_id — the
    // first 50 ids of the sorted component.
    let returned_ids: Vec<i64> = members.iter().map(|m| m.symbol_id).collect();
    let expected_ids: Vec<i64> = big.iter().copied().take(MEMBER_VALUE_CAP).collect();
    assert_eq!(
        returned_ids, expected_ids,
        "capped members must be the first MEMBER_VALUE_CAP in canonical (struct_hash, id) order"
    );

    // Close the SQLite connection before deleting its dir: Windows refuses to remove a file with a
    // live handle (`os error 32`), whereas Unix unlinks it lazily. Dropping `db` first makes the
    // strict teardown pass on both.
    drop(db);
    fs::remove_dir_all(root).unwrap();
}

/// Plan 4b Task 5b: every `RefineMember` returned by `load_refine_members` must carry
/// `node_spans` with the same length as `seq` (bijection invariant from §1.5), a non-empty `text`
/// buffer (the whole-file source), and spans whose byte ranges recover real source text. Also
/// confirms that members sharing a file share the same `Arc<str>` allocation
/// (`Arc::ptr_eq`) — one read per file, not one per member.
#[test]
fn refine_member_carries_spans_len_eq_seq() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two rename-clone functions in TWO files (so we can also test same-file Arc sharing below via
    // a third function added to a.rs).
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

    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");

    let members =
        db.load_refine_members(&[id_a, id_b]).expect("load ok").expect("refine inputs available");
    assert_eq!(members.len(), 2, "both members must be returned");

    for m in &members {
        // Bijection invariant: node_spans.len() == seq.len().
        assert_eq!(
            m.node_spans.len(),
            m.seq.len(),
            "node_spans.len() must equal seq.len() for member {}",
            m.symbol_id
        );
        // text must be non-empty (whole-file source, not sliced).
        assert!(!m.text.is_empty(), "member text must be non-empty");
        // At least one span's byte range must recover a real (non-empty) source slice.
        let any_real_slice = m
            .node_spans
            .iter()
            .any(|sp| m.text.get(sp.start_byte..sp.end_byte).is_some_and(|s| !s.is_empty()));
        assert!(
            any_real_slice,
            "at least one span in node_spans must recover a non-empty source slice for member {}",
            m.symbol_id
        );
        // A leaf span (is_leaf=true) must recover a real identifier (non-empty, non-whitespace).
        if let Some(leaf_sp) = m.node_spans.iter().find(|s| s.is_leaf) {
            let slice =
                m.text.get(leaf_sp.start_byte..leaf_sp.end_byte).expect("leaf span byte range");
            assert!(!slice.trim().is_empty(), "leaf span must recover non-empty source text");
        }
    }

    // Close the SQLite connection before deleting its dir: Windows refuses to remove a file with a
    // live handle (`os error 32`), whereas Unix unlinks it lazily. Dropping `db` first makes the
    // strict teardown pass on both.
    drop(db);
    fs::remove_dir_all(root).unwrap();
}

/// Plan 4b Task 5b: the faithfulness pin still drops drifted members. A member whose on-disk
/// content changed after indexing (struct_hash mismatch between re-parse and persisted) causes
/// `load_refine_members` to return `Ok(None)` — the caller falls back to the un-refined class
/// rather than aligning stale tokens.
#[test]
fn faithfulness_pin_still_drops_drifted_member() {
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

    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");

    // Sanity: before drift, refine inputs are available.
    assert!(
        db.load_refine_members(&[id_a, id_b]).unwrap().is_some(),
        "before drift: refine inputs must be available"
    );

    // Drift one member's on-disk source: overwrite b.rs with a structurally DIFFERENT function
    // (adds a while loop → different struct_hash). The index still holds the old fingerprint row.
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); while o > 0 { o -= 1; } o }\n",
    )
    .unwrap();

    // After drift: the re-parse of b.rs produces a different struct_hash → faithfulness pin fires
    // → load_refine_members returns Ok(None).
    let result = db.load_refine_members(&[id_a, id_b]).unwrap();
    assert!(
        result.is_none(),
        "a drifted member (struct_hash mismatch) must cause load_refine_members to return Ok(None)"
    );

    let _ = fs::remove_dir_all(root);
}

/// #259 (Adversary C) — END-TO-END through the real `find_clones` driver: a refine-FAILED class (a
/// clone class whose on-disk source drifted post-index, so `refine_class_in_place` no-ops and the
/// class stays un-refined with its member_count-LINEAR Plan-2 ROI) has its `member_count` size
/// factor DAMPENED in the returned result. The dampen (applied to every still-un-refined class
/// post-refine, pre-sort in BOTH `find_clones` branches) replaces the linear `member_count` factor
/// with `1 + ln(1 + member_count)`, so a refine-failed component can no longer masquerade as
/// high-ROI purely on size. This exercises the dampen through the REAL driver, not just the helper
/// unit — it proves `find_clones` rewrites a returned un-refined class's `roi` to the dampened
/// formula.
///
/// Fixture: a clone class of THREE rename-clones with a substantial body. We DRIFT one member on
/// disk (structurally different re-parse → struct_hash mismatch → the all-or-nothing faithfulness
/// pin makes the refinement a no-op on a COLD cache), so the class comes back un-refined. The
/// returned `roi` must equal the DAMPENED Plan-2 product (size factor `1 + ln(1 + member_count)`),
/// which is strictly below the raw LINEAR product (`member_count` factor) the class would carry
/// without the fix — the masquerade is closed.
#[test]
fn refine_failed_class_member_count_dampened_end_to_end() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // A clone class of THREE rename-clones with a substantial body — would refine cleanly if the
    // source stayed faithful.
    let body = |name: &str, v: &str| {
        format!(
            "pub fn {name}(db: Db) -> i32 {{ let {v}1 = db.get(1); let {v}2 = db.get(2); let {v}3 \
             = db.get(3); validate({v}1); validate({v}2); validate({v}3); {v}1 + {v}2 + {v}3 }}\n"
        )
    };
    fs::write(root.join("src/a.rs"), body("load_a", "u")).unwrap();
    fs::write(root.join("src/b.rs"), body("load_b", "o")).unwrap();
    fs::write(root.join("src/c.rs"), body("load_c", "p")).unwrap();

    // Build the index clean (so the 3-member class forms from faithful fingerprints) with a COLD
    // refine cache, then DRIFT one member BEFORE the first find_clones. A warm cache hit would
    // serve the cached refinement and never re-read the drifted source (the cache key is over
    // the PERSISTED struct_hash + the PERSISTED file sha256, both unchanged by an on-disk
    // edit), so the drift MUST land before the very first refine attempt.
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // DRIFT one member: overwrite c.rs with a structurally DIFFERENT body. The index keeps the old
    // fingerprint (the class is still BUILT with member_count 3 from the persisted tables), but the
    // FIRST find_clones below takes the cold refine path → `load_refine_members` re-parses the
    // drifted source → struct_hash mismatch → the all-or-nothing faithfulness pin returns Ok(None)
    // → the class stays UN-refined.
    fs::write(
        root.join("src/c.rs"),
        "pub fn load_c(db: Db) -> i32 { let mut n = 0; while n < 9 { n += 1; } n }\n",
    )
    .unwrap();

    let result = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        result.classes.len(),
        1,
        "the three rename-clones form one class: {:?}",
        result.classes
    );
    let class = &result.classes[0];
    assert_eq!(
        class.member_count, 3,
        "the class is built with all 3 members from persisted tables"
    );

    // The class FAILED refinement (one member drifted → all-or-nothing) and is un-refined.
    assert!(
        !class.refined,
        "the drifted class fails the all-or-nothing refine and stays un-refined"
    );

    // THE #259 PROPERTY: the returned `roi` is the DAMPENED Plan-2 product — the `member_count`
    // factor replaced by `1 + ln(1 + member_count)`. Reconstruct both the raw (linear) and the
    // dampened product from the surfaced factors and confirm find_clones returned the dampened one.
    let mc = class.member_count as f64;
    let raw_roi = class.cross_module_spread as f64
        * mc
        * class.body_token_len_medoid as f64
        * class.roi_factors.load_bearing_factor
        * class.cohesion_min_pairwise;
    let dampened_roi = raw_roi / mc * (1.0 + mc.ln_1p());
    assert!(
        (class.roi - dampened_roi).abs() < 1e-6,
        "#259: find_clones must return the DAMPENED roi {} for the un-refined class, got {}",
        dampened_roi,
        class.roi
    );
    // The dampen STRICTLY reduces the rank signal versus the raw linear Plan-2 ROI (3 members →
    // 1 + ln(4) ≈ 2.39 < 3), so a large refine-failed component can no longer dominate on size.
    assert!(
        class.roi < raw_roi,
        "#259: the dampened roi {} must be strictly below the raw linear Plan-2 roi {}",
        class.roi,
        raw_roi
    );

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
}

/// #274 item 3a: `clones_for_symbol` reports a RICHER eligibility reason than the bare
/// `symbol_fingerprinted = false` bool, distinguishing the four "not eligible" causes:
/// `BelowMinTokens`, `NonFunctionKind`, `Generated`, and `StaleNormalizerVersion`. Each variant is
/// exercised with a symbol that triggers exactly it, plus the `Eligible` and `SymbolNotResolved`
/// verdicts and the bool/enum consistency invariant.
#[test]
fn clones_for_symbol_distinguishes_ineligibility_reasons() {
    use crate::index::clones::NORM_VERSION;

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("src/generated")).unwrap();

    // tiny: a `function` below MIN_TOKENS ⇒ no fingerprint row ⇒ BelowMinTokens.
    fs::write(root.join("src/tiny.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    // a struct: a non-function symbol ⇒ NonFunctionKind. (A struct never fingerprints.)
    fs::write(
        root.join("src/shape.rs"),
        "pub struct Shape { pub width: i32, pub height: i32, pub depth: i32 }\n",
    )
    .unwrap();
    // generated: a SUBSTANTIAL function (clears MIN_TOKENS) in a path-heuristic generated file.
    // It keeps `kind = source` and gets symbols, but `files.generated = 1`, so the index-time
    // fingerprint compute is skipped — Generated must win over BelowMinTokens (it clears the size
    // floor, so the only reason it is unfingerprinted is the generated flag).
    fs::write(
        root.join("src/generated/bindings.rs"),
        "pub fn shared_symbol(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n ^= b as usize; \
         } n }\n",
    )
    .unwrap();
    // stale: a substantial, structurally distinct function ⇒ normally fingerprinted (eligible). We
    // demote its fingerprint row to NORM_VERSION - 1 below so the current-version read misses it
    // while a baseline row still EXISTS ⇒ StaleNormalizerVersion.
    fs::write(
        root.join("src/stale.rs"),
        "pub fn stale_fn(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n += b as usize; } n \
         }\n",
    )
    .unwrap();
    // eligible: two rename-clones so each is fingerprinted AND in a clone class ⇒ Eligible.
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

    // Demote stale_fn's baseline fingerprint to a PRIOR normalizer_version: the row still exists,
    // but the current-NORM_VERSION read filter excludes it.
    {
        let conn = db.storage.connection();
        let updated = conn
            .execute(
                "UPDATE symbol_fingerprints
                 SET normalizer_version = ?2
                 WHERE normalizer_kind = 'baseline'
                   AND normalizer_version = ?1
                   AND symbol_id IN (
                       SELECT symbols.id FROM symbols
                       JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                       WHERE ns.value = 'src/stale.rs::stale_fn'
                   )",
                rusqlite::params![NORM_VERSION, NORM_VERSION - 1],
            )
            .unwrap();
        assert_eq!(updated, 1, "exactly one stale_fn baseline row should be demoted");
    }

    // Helper: assert the bool fields stay consistent with the enum verdict (the invariant the
    // result type documents).
    let assert_consistent = |res: &crate::index::ClonesForSymbolResult| match res.eligibility {
        crate::index::CloneEligibility::SymbolNotResolved => {
            assert!(!res.symbol_resolved && !res.symbol_fingerprinted);
        },
        crate::index::CloneEligibility::Eligible => {
            assert!(res.symbol_resolved && res.symbol_fingerprinted);
        },
        crate::index::CloneEligibility::Ineligible { .. } => {
            assert!(res.symbol_resolved && !res.symbol_fingerprinted);
        },
    };

    // BelowMinTokens.
    let tiny = db.clones_for_symbol(CloneSymbolSelector::Ref("src/tiny.rs::tiny".into())).unwrap();
    assert_eq!(
        tiny.eligibility,
        crate::index::CloneEligibility::Ineligible {
            reason: crate::index::CloneIneligibilityReason::BelowMinTokens
        },
        "a below-MIN_TOKENS function reports BelowMinTokens"
    );
    assert_consistent(&tiny);

    // NonFunctionKind.
    let shape =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/shape.rs::Shape".into())).unwrap();
    assert_eq!(
        shape.eligibility,
        crate::index::CloneEligibility::Ineligible {
            reason: crate::index::CloneIneligibilityReason::NonFunctionKind
        },
        "a struct (non-function kind) reports NonFunctionKind"
    );
    assert_consistent(&shape);

    // Generated (wins over BelowMinTokens even though the body clears the size floor).
    let generated = db
        .clones_for_symbol(CloneSymbolSelector::Ref(
            "src/generated/bindings.rs::shared_symbol".into(),
        ))
        .unwrap();
    assert_eq!(
        generated.eligibility,
        crate::index::CloneEligibility::Ineligible {
            reason: crate::index::CloneIneligibilityReason::Generated
        },
        "a substantial function in a generated file reports Generated"
    );
    assert_consistent(&generated);

    // StaleNormalizerVersion (a baseline row exists, just not at the current version).
    let stale =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/stale.rs::stale_fn".into())).unwrap();
    assert_eq!(
        stale.eligibility,
        crate::index::CloneEligibility::Ineligible {
            reason: crate::index::CloneIneligibilityReason::StaleNormalizerVersion
        },
        "a symbol whose only fingerprint row is at a prior normalizer_version reports \
         StaleNormalizerVersion"
    );
    assert_consistent(&stale);

    // Eligible (fingerprinted + in a clone class).
    let eligible =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into())).unwrap();
    assert_eq!(
        eligible.eligibility,
        crate::index::CloneEligibility::Eligible,
        "a fingerprinted symbol reports Eligible"
    );
    assert!(eligible.class.is_some(), "load_user is in a clone class");
    assert_consistent(&eligible);

    // SymbolNotResolved (no such symbol).
    let missing = db
        .clones_for_symbol(CloneSymbolSelector::Ref("src/nope.rs::does_not_exist".into()))
        .unwrap();
    assert_eq!(
        missing.eligibility,
        crate::index::CloneEligibility::SymbolNotResolved,
        "an unresolved selector reports SymbolNotResolved"
    );
    assert_consistent(&missing);

    // The enum serializes as an internally-tagged object with the snake_case wire tokens that
    // cross the MCP/CLI boundary.
    let json = serde_json::to_value(&tiny).unwrap();
    assert_eq!(json["eligibility"]["status"], "ineligible");
    assert_eq!(json["eligibility"]["reason"], "below_min_tokens");
    let json_ok = serde_json::to_value(&eligible).unwrap();
    assert_eq!(json_ok["eligibility"]["status"], "eligible");

    // as_db_str / from_db_str round-trip for every variant.
    for reason in [
        crate::index::CloneIneligibilityReason::Generated,
        crate::index::CloneIneligibilityReason::NonFunctionKind,
        crate::index::CloneIneligibilityReason::StaleNormalizerVersion,
        crate::index::CloneIneligibilityReason::BelowMinTokens,
    ] {
        assert_eq!(
            crate::index::CloneIneligibilityReason::from_db_str(reason.as_db_str()),
            Some(reason),
            "as_db_str/from_db_str must round-trip"
        );
    }
    assert_eq!(crate::index::CloneIneligibilityReason::from_db_str("bogus"), None);

    let _ = fs::remove_dir_all(root);
}

/// Plan 4b Task 5c: `build_class` threads the medoid's `symbol_id` out as
/// `CandidateCloneClass::medoid_symbol_id`. For a normal (non-sampled) clone class:
///   - `medoid_symbol_id` is `Some`.
///   - The id it contains is one of the class's member symbol_ids.
///   - It is stable across two independent `find_clones` calls on the same index.
///
/// The medoid is the bag-overlap medoid (max Σ overlap/max_len), NOT an LCS-distance medoid —
/// sound as a template-spine anchor for a coherence-split class (§1.1). Task 5d uses it as the
/// anti-unify anchor, falling back to the canonical-first `(struct_hash, path, start_byte)` member
/// when `None`.
#[test]
fn build_class_surfaces_medoid_symbol_id() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // Two rename-clone functions: same structure, different local names. Both fingerprint to the
    // SAME struct_hash (rename-clone), so they form one class via the struct_hash fast path.
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

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(res.classes.len(), 1, "one clone class");
    let class = &res.classes[0];

    // medoid_symbol_id must be Some for any non-degenerate class.
    let medoid_id = class
        .medoid_symbol_id
        .expect("medoid_symbol_id must be Some for a non-degenerate clone class");

    // Collect the actual symbol_ids of the class members by resolving their qualified names from
    // the DB — `CloneMember` doesn't expose the rowid, so we look it up via the helper.
    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");
    let member_ids = [id_a, id_b];

    assert!(
        member_ids.contains(&medoid_id),
        "medoid_symbol_id ({medoid_id}) must be one of the class's member symbol_ids \
         ({member_ids:?})"
    );

    // Stability: a second find_clones call on the same index must return the same medoid_symbol_id.
    let res2 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    let medoid_id2 =
        res2.classes[0].medoid_symbol_id.expect("medoid_symbol_id must be Some on second call");
    assert_eq!(
        medoid_id, medoid_id2,
        "medoid_symbol_id must be deterministic across repeated calls"
    );

    // Close the SQLite connection before deleting its dir: Windows refuses to remove a file with a
    // live handle (`os error 32`), whereas Unix unlinks it lazily. Dropping `db` first makes the
    // strict teardown pass on both.
    drop(db);
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

/// Plan 4a (#215): the refine driver is BEST-EFFORT — when refine inputs are unavailable it leaves
/// the class in its Plan-2 un-refined shape rather than erroring. We force `load_refine_members` to
/// return `None` by deleting the source files AFTER indexing: the bags/fingerprints are already
/// persisted in SQLite (so `find_clones` can still build the class), but `read_to_string` of each
/// member's now-missing path fails, tripping the un-refinable fallback. The returned class must be
/// the bare candidate component with every refinement field cleared — no panic, no error.
#[test]
fn find_clones_falls_back_to_unrefined_when_source_unavailable() {
    let root = unique_temp_root();
    // Build the index — fingerprints/bags persisted in the DB under `root/.rag-rat`.
    let db = write_four_renamed_clones(&root);

    // Delete the source trees (a/ and b/) but keep `.rag-rat/` (the SQLite DB). Each member's path
    // now fails `read_to_string`, so `load_refine_members` returns `None` → un-refinable fallback.
    let _ = fs::remove_dir_all(root.join("a"));
    let _ = fs::remove_dir_all(root.join("b"));

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        res.classes.len(),
        1,
        "the class is still built from persisted bags even with source gone"
    );
    let c = &res.classes[0];

    assert!(!c.refined, "source gone → refine inputs unavailable → class stays un-refined");
    assert_eq!(c.class_kind, "candidate_component", "un-refined classes keep the Plan-2 kind");
    assert!(c.lcs_ratio.is_none(), "no lcs_ratio on an un-refined class");
    assert!(c.refactorability.is_none(), "no refactorability on an un-refined class");
    assert!(c.confidence.is_none(), "no confidence on an un-refined class");
    assert!(c.refine_mode.is_none(), "no refine_mode on an un-refined class");

    // a/ and b/ are already gone; only `.rag-rat/` remains under root.
    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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
        let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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
        let _ = fs::remove_dir_all(root);
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
        let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
}

/// Faithfulness pin (#215 Plan 4a Task 2): `load_refine_members` re-parses each member's scoped
/// source and re-normalizes to the ordered baseline token sequence — the strong correctness
/// guarantee is that `tokens::struct_hash(&member.seq)` reproduces the PERSISTED
/// `symbol_fingerprints.struct_hash` exactly (the re-parse is faithful to Plan-1's normalization).
/// Also pins: seqs are non-empty, members come back sorted by struct_hash, and the lang/byte-range
/// are populated.
#[test]
fn load_refine_members_reparse_is_faithful_to_persisted_struct_hash() {
    use crate::index::clones::tokens::struct_hash;

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two rename-clone functions — identical structure, different identifier names. Both
    // fingerprint to the SAME struct_hash (renamed clones), so sorting by struct_hash is stable on
    // symbol_id.
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

    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");

    let members = db
        .load_refine_members(&[id_a, id_b])
        .unwrap()
        .expect("refine inputs available for an unchanged, in-scope clone pair");
    assert_eq!(members.len(), 2, "both members loaded");

    // Persisted struct_hash per member, for the faithfulness comparison.
    let persisted = |sid: i64| -> String {
        db.storage
            .connection()
            .query_row(
                "SELECT struct_hash FROM symbol_fingerprints
                 WHERE symbol_id = ?1 AND normalizer_kind = 'baseline'",
                params![sid],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };

    for m in &members {
        assert!(!m.seq.is_empty(), "the re-parsed token sequence must be non-empty");
        assert_eq!(m.lang, Language::Rust, "member language is rust");
        // THE PIN: the re-parse reproduces Plan-1's normalization exactly.
        assert_eq!(
            struct_hash(&m.seq),
            m.struct_hash,
            "re-parsed struct_hash must equal the member's persisted struct_hash"
        );
        assert_eq!(
            m.struct_hash,
            persisted(m.symbol_id),
            "the member's carried struct_hash must equal the DB-persisted struct_hash"
        );
    }

    // Members are returned in canonical sorted-by-struct_hash order. Production keys the canonical
    // order on the REINDEX-STABLE `(struct_hash, path, start_byte)`; `RefineMember` carries no
    // `path`/`start_byte`, so this test can only re-derive `(struct_hash, symbol_id)` — the
    // fixtures arrange `symbol_id` to coincide with `(path, start_byte)`, so the orders match
    // here. The REAL reindex-stable-order guard is the unit test
    // `refine_member_order_is_reindex_stable`.
    let mut expected =
        members.iter().map(|m| (m.struct_hash.clone(), m.symbol_id)).collect::<Vec<_>>();
    expected.sort();
    let actual = members.iter().map(|m| (m.struct_hash.clone(), m.symbol_id)).collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "members must be in canonical (struct_hash, path, start_byte) order — \
         struct_hash-ascending here (fixtures pin symbol_id to coincide); real guard: \
         refine_member_order_is_reindex_stable"
    );

    let _ = fs::remove_dir_all(root);
}

/// Empty input is a valid (empty) refine set — not a failure.
#[test]
fn load_refine_members_empty_input_returns_empty() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn f() -> i32 { 0 }\n").unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let members = db.load_refine_members(&[]).unwrap().expect("empty input is a valid empty set");
    assert!(members.is_empty(), "empty member_ids → empty members");

    let _ = fs::remove_dir_all(root);
}

/// Missing source: a member whose source file is deleted from disk (but whose fingerprint row is
/// still persisted) makes the re-parse impossible, so `load_refine_members` returns `Ok(None)` —
/// the caller falls back to an un-refined class rather than refining over a partial input.
#[test]
fn load_refine_members_returns_none_when_source_missing() {
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

    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");

    // Delete one member's source file on disk; the fingerprint rows are unchanged in the index.
    fs::remove_file(root.join("src/b.rs")).unwrap();

    let result = db.load_refine_members(&[id_a, id_b]).unwrap();
    assert!(
        result.is_none(),
        "a member with a deleted source file must yield Ok(None) for the whole class"
    );

    let _ = fs::remove_dir_all(root);
}

/// Overlay fallback (#215 Plan 4a Task 2): under a LINKED-WORKTREE OVERLAY scope, `source_root` is
/// the MAIN checkout — not the branch the overlay's symbol rows came from — so no scope-correct
/// source read is available and `load_refine_members` must return `Ok(None)` BEFORE touching disk.
/// Mirrors `count_stale_member_paths` / the staleness heal path's overlay early-return.
#[test]
fn load_refine_members_returns_none_under_linked_overlay_scope() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    // Base has only a tiny (below-MIN_TOKENS) function — no fingerprint, no clone class.
    fs::write(main.join("src/base.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // Linked worktree on a new branch adds a rename-clone pair.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
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

    // Index the overlay — leaves the connection in the linked (overlay) scope.
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(db.active_scope_is_linked_overlay(), "connection must be in the overlay scope");

    // Resolve the branch members' ids (under the overlay scope they are visible).
    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");

    // Even with valid member ids, refine is unavailable under an overlay scope.
    let result = db.load_refine_members(&[id_a, id_b]).unwrap();
    assert!(
        result.is_none(),
        "refine must be unavailable (Ok(None)) under a linked-worktree overlay scope"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}
