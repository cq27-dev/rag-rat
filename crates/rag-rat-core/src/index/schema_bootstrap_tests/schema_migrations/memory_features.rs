use super::*;

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
    schema::migrations::apply_clone_delta_maintenance(&conn).unwrap();
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
    schema::migrations::apply_clone_df_epoch(&conn).unwrap();
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
    schema::migrations::apply_clone_df_epoch(&conn).unwrap();
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
