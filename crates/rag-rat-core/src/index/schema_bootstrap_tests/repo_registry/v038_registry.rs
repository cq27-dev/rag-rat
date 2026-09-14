use super::*;

/// Fresh `apply` creates the three registry tables with their exact columns, STRICT, and seeds the
/// single adoption placeholder whose id MUST equal `LEGACY_REPO_ID`. The absolute
/// `LATEST_SCHEMA_VERSION` pin moved to the new tip's test (`migration_039_*`); this one uses only
/// the symbolic `current_version == LATEST_SCHEMA_VERSION` check (the hardcoded-LATEST footgun).
#[test]
fn migration_038_creates_repos_registry_tables() {
    let conn = fresh_conn();

    assert_eq!(conn_table_columns(&conn, "repos"), vec![
        "repo_id",
        "display_name",
        "registered_at_ms"
    ]);
    assert_eq!(conn_table_columns(&conn, "repo_roots"), vec![
        "repo_id",
        "root",
        "registered_at_ms"
    ]);
    assert_eq!(conn_table_columns(&conn, "repo_meta"), vec!["repo_id", "key", "value"]);

    // STRICT on every new table (schema convention).
    for table in ["repos", "repo_roots", "repo_meta"] {
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.to_ascii_uppercase().contains("STRICT"), "{table} is STRICT: {sql}");
    }

    // Exactly the adoption placeholder, and its id is the LEGACY_REPO_ID constant (the DDL literal
    // and the constant must stay coupled — register_repo reads the constant to adopt the row).
    let repos: Vec<(String, String, i64)> = {
        let mut stmt =
            conn.prepare("SELECT repo_id, display_name, registered_at_ms FROM repos").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(repos, vec![(LEGACY_REPO_ID.to_string(), String::new(), 0)]);

    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST after V038"
    );
}

/// The V038 DDL is self-contained: it references only its own tables, so `apply_repos_registry`
/// runs on a BARE connection (no baseline) and is exactly what introduces the registry. Anchoring
/// this to the migration function (not the full ladder) keeps it valid when a future migration also
/// touches these tables (the directory's "assert absence/DDL in isolation" rule).
#[test]
fn v038_registry_ddl_is_self_contained_and_introduces_the_registry() {
    let bare = rusqlite::Connection::open_in_memory().expect("open");
    assert!(!conn_table_exists(&bare, "repos"), "no registry before the migration runs");

    schema::migrations::apply_repos_registry(&bare)
        .expect("V038 DDL applies standalone on a bare conn");

    for table in ["repos", "repo_roots", "repo_meta"] {
        assert!(conn_table_exists(&bare, table), "V038 creates {table}");
    }
    assert_eq!(repo_row_count(&bare, LEGACY_REPO_ID), 1, "seeds the adoption placeholder");
}

/// A V037 index gains the registry on `migrate_forward`.
#[test]
fn migration_038_forward_migrates_a_v037_index() {
    let conn = fresh_conn();

    // Revert to the V037 shape: remove future triggers before their repo_meta target, drop
    // children before the parent (FK), then drop the ledger rows.
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memory_bindings_lens_revision_insert;
         DROP TRIGGER IF EXISTS memory_bindings_lens_revision_delete;
         DROP TRIGGER IF EXISTS memory_bindings_lens_revision_update;
         DROP TRIGGER IF EXISTS memory_bindings_lane_revision_insert;
         DROP TRIGGER IF EXISTS memory_bindings_lane_revision_delete;
         DROP TRIGGER IF EXISTS memory_bindings_lane_revision_update;
         DROP TRIGGER papertrail_items_lens_revision_insert;
         DROP TRIGGER papertrail_items_lens_revision_delete;
         DROP TRIGGER papertrail_items_lens_revision_update;
         DROP TRIGGER papertrail_items_lane_revision_insert;
         DROP TRIGGER papertrail_items_lane_revision_delete;
         DROP TRIGGER papertrail_items_lane_revision_update;
         DROP TRIGGER papertrail_refs_lens_revision_insert;
         DROP TRIGGER papertrail_refs_lens_revision_delete;
         DROP TRIGGER papertrail_refs_lens_revision_update;
         DROP TRIGGER papertrail_refs_lane_revision_insert;
         DROP TRIGGER papertrail_refs_lane_revision_delete;
         DROP TRIGGER papertrail_refs_lane_revision_update;
         DROP TABLE repo_meta;
         DROP TABLE repo_roots;
         DROP TABLE repos;",
    )
    .expect("revert to V037 shape");
    truncate_schema_to(&conn, 37);
    assert_eq!(
        schema::status(&conn).unwrap().state,
        schema::SchemaState::Older,
        "schema is Older after removing the V038 ledger row"
    );

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).expect("migrate_forward");
    for table in ["repos", "repo_roots", "repo_meta"] {
        assert!(conn_table_exists(&conn, table), "V038 recreates {table} on forward migrate");
    }
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 1, "placeholder re-seeded");
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST after forward migrate"
    );
}
