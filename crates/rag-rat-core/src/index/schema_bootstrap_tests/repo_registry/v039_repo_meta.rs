use super::*;

// --- V039: per-repo meta relocation (memory-sync phase A2) ---

/// Fresh `apply` runs V039; it relocates the listed per-repo singleton keys into `repo_meta` under
/// the placeholder and leaves the machine-level keys in their global tables. (The absolute
/// `LATEST_SCHEMA_VERSION` pin moved to `migration_040_*`, the new tip; this uses only the symbolic
/// `current_version == LATEST` check.)
#[test]
fn migration_039_relocates_per_repo_meta_and_leaves_global_keys() {
    let conn = fresh_conn();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST after apply"
    );

    // Seed the SOURCE tables as a legacy DB carries them: relocated keys plus, to prove
    // selectivity, one machine-level key in EACH table that MUST stay put.
    upsert_meta(&conn, "index_meta", "source_root", "/src/repo");
    upsert_meta(&conn, "index_meta", "git_commit", "abc123");
    upsert_meta(&conn, "index_meta", "active_embedding_model", "embedding-hash");
    upsert_meta(&conn, "index_meta", "generated_flags_version", "1"); // machine-level, stays
    upsert_meta(&conn, "reconcile_meta", "embedding_active_model_version", "hash-v1");
    upsert_meta(&conn, "reconcile_meta", "vector_int8_reencode_cursor", "42\nmodel-a");
    upsert_meta(&conn, "reconcile_meta", "last_embedding_reconcile_started_at_ms", "1000"); // stays

    // Re-run the relocation (the tables were empty when apply() first ran it).
    schema::migrations::apply_move_per_repo_meta(&conn).expect("relocate");

    for (key, value) in [
        ("source_root", "/src/repo"),
        ("git_commit", "abc123"),
        ("active_embedding_model", "embedding-hash"),
        ("embedding_active_model_version", "hash-v1"),
        ("vector_int8_reencode_cursor", "42\nmodel-a"),
    ] {
        assert_eq!(
            rag_rat_db::meta::repo_meta(&conn, LEGACY_REPO_ID, key).unwrap().as_deref(),
            Some(value),
            "{key} relocated to repo_meta under the placeholder"
        );
        assert!(!meta_present(&conn, "index_meta", key), "{key} removed from index_meta");
        assert!(!meta_present(&conn, "reconcile_meta", key), "{key} removed from reconcile_meta");
    }

    // Machine-level keys are untouched: they stay in their global tables and never enter repo_meta.
    assert!(meta_present(&conn, "index_meta", "generated_flags_version"), "index_meta key stays");
    assert!(
        meta_present(&conn, "reconcile_meta", "last_embedding_reconcile_started_at_ms"),
        "reconcile timing key stays"
    );
    assert!(
        rag_rat_db::meta::repo_meta(&conn, LEGACY_REPO_ID, "generated_flags_version")
            .unwrap()
            .is_none(),
        "machine-level key did not leak into repo_meta"
    );
}

/// A legacy V038 index (per-repo keys still in the global tables, ledger at 38) gains the
/// relocation on `migrate_forward`.
#[test]
fn migration_039_forward_migrates_a_v038_index() {
    let conn = fresh_conn();

    // Simulate the V038 state: per-repo keys still in the GLOBAL tables, ledger reverted to 38.
    upsert_meta(&conn, "index_meta", "indexed_at_ms", "5000");
    upsert_meta(&conn, "reconcile_meta", "embedding_active_model_version", "hash-v1");
    truncate_schema_to(&conn, 38);
    assert_eq!(
        schema::status(&conn).unwrap().state,
        schema::SchemaState::Older,
        "schema is Older after removing the V039 ledger row"
    );

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).expect("migrate_forward");

    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST after forward migrate"
    );
    assert_eq!(
        rag_rat_db::meta::repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms").unwrap().as_deref(),
        Some("5000"),
        "index_meta key relocated on forward migrate"
    );
    assert_eq!(
        rag_rat_db::meta::repo_meta(&conn, LEGACY_REPO_ID, "embedding_active_model_version")
            .unwrap()
            .as_deref(),
        Some("hash-v1"),
        "reconcile_meta key relocated on forward migrate"
    );
    assert!(!meta_present(&conn, "index_meta", "indexed_at_ms"));
    assert!(!meta_present(&conn, "reconcile_meta", "embedding_active_model_version"));
}

/// V039 in ISOLATION: on a bare conn carrying only the source meta tables + the V038 registry, the
/// migration function relocates the keys — anchored to the migration function, not the full ladder
/// (the directory's "assert behavior in isolation" rule). It also proves the run is idempotent.
#[test]
fn v039_relocation_runs_standalone_and_is_idempotent() {
    let bare = rusqlite::Connection::open_in_memory().expect("open");
    bare.execute_batch(
        "CREATE TABLE index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE reconcile_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .unwrap();
    schema::migrations::apply_repos_registry(&bare).expect("V038 registry");
    // `git_commit` is a genuinely per-repo key V039 relocates; the reencode cursor moves too. (The
    // `fts_*` trio + `content_revision` are RECLASSIFIED GLOBAL by V040, so V039 no longer
    // relocates them — covered by `v039_leaves_reclassified_global_keys_in_index_meta`.)
    upsert_meta(&bare, "index_meta", "git_commit", "abc123");
    upsert_meta(&bare, "reconcile_meta", "vector_int8_reencode_cursor", "7\nm");

    schema::migrations::apply_move_per_repo_meta(&bare).expect("V039 relocation standalone");
    schema::migrations::apply_move_per_repo_meta(&bare).expect("V039 relocation is idempotent");

    assert_eq!(
        rag_rat_db::meta::repo_meta(&bare, LEGACY_REPO_ID, "git_commit").unwrap().as_deref(),
        Some("abc123"),
    );
    assert_eq!(
        rag_rat_db::meta::repo_meta(&bare, LEGACY_REPO_ID, "vector_int8_reencode_cursor")
            .unwrap()
            .as_deref(),
        Some("7\nm"),
    );
    assert!(!meta_present(&bare, "index_meta", "git_commit"));
    assert!(!meta_present(&bare, "reconcile_meta", "vector_int8_reencode_cursor"));
    // Re-run left exactly one relocated row (no duplicate from the second pass).
    let count: i64 = bare
        .query_row(
            "SELECT COUNT(*) FROM repo_meta WHERE repo_id = ?1 AND key = 'git_commit'",
            [LEGACY_REPO_ID],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "re-run does not duplicate the relocated row");
}

/// V040 RECLASSIFICATION: `content_revision` + the `fts_*` freshness trio are GLOBAL infrastructure
/// (one `chunk_fts` index, a digest over the whole `main.files`), NOT per-repo. V039 (frozen) still
/// lists them, so the shared `relocate_meta_keys` filters them out — V039's sweep must LEAVE them
/// in `index_meta` while still relocating the genuinely-per-repo keys beside them. This is the
/// property that keeps `index --full` from hard-erroring at V039 on a consolidated DB after the
/// reclassification.
#[test]
fn v039_leaves_reclassified_global_keys_in_index_meta() {
    let bare = rusqlite::Connection::open_in_memory().expect("open");
    bare.execute_batch(
        "CREATE TABLE index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE reconcile_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .unwrap();
    schema::migrations::apply_repos_registry(&bare).expect("V038 registry");
    // The 4 reclassified-global index_meta keys, plus one genuinely-per-repo key as a positive
    // control that DOES relocate.
    for key in ["content_revision", "fts_dirty", "fts_source_revision", "fts_synced_at_ms"] {
        upsert_meta(&bare, "index_meta", key, "global-val");
    }
    upsert_meta(&bare, "index_meta", "source_root", "/src/repo");

    schema::migrations::apply_move_per_repo_meta(&bare).expect("V039 relocation standalone");

    // The 4 global keys STAY in index_meta and never enter repo_meta.
    for key in ["content_revision", "fts_dirty", "fts_source_revision", "fts_synced_at_ms"] {
        assert!(meta_present(&bare, "index_meta", key), "{key} stays GLOBAL in index_meta");
        assert!(
            rag_rat_db::meta::repo_meta(&bare, LEGACY_REPO_ID, key).unwrap().is_none(),
            "{key} did NOT relocate to repo_meta",
        );
    }
    // The per-repo control still relocated — V039's behavior for non-reclassified keys is intact.
    assert!(!meta_present(&bare, "index_meta", "source_root"), "per-repo key still relocates");
    assert_eq!(
        rag_rat_db::meta::repo_meta(&bare, LEGACY_REPO_ID, "source_root").unwrap().as_deref(),
        Some("/src/repo"),
    );
}

/// The `repo_meta` accessors: upsert, read, no-op-if-unchanged, and delete — each scoped by
/// `(repo_id, key)` so the same key under a different repo is independent.
#[test]
fn repo_meta_accessors_round_trip_and_scope_by_repo() {
    use rag_rat_db::meta::{delete_repo_meta, repo_meta, set_repo_meta, set_repo_meta_if_changed};
    let conn = fresh_conn();
    // A second repos row so the (repo_id, key) scoping is observable (inserted directly — the
    // phase-A single-repo invariant is about register_repo, not the storage layer).
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-b', 'b', 0)",
        [],
    )
    .unwrap();

    // Upsert + read + overwrite.
    assert!(repo_meta(&conn, LEGACY_REPO_ID, "k").unwrap().is_none());
    set_repo_meta(&conn, LEGACY_REPO_ID, "k", "v1").unwrap();
    assert_eq!(repo_meta(&conn, LEGACY_REPO_ID, "k").unwrap().as_deref(), Some("v1"));
    set_repo_meta(&conn, LEGACY_REPO_ID, "k", "v2").unwrap();
    assert_eq!(repo_meta(&conn, LEGACY_REPO_ID, "k").unwrap().as_deref(), Some("v2"));

    // Scoped by repo_id: the same key under a different repo is independent.
    set_repo_meta(&conn, "repo-b", "k", "other").unwrap();
    assert_eq!(repo_meta(&conn, LEGACY_REPO_ID, "k").unwrap().as_deref(), Some("v2"));
    assert_eq!(repo_meta(&conn, "repo-b", "k").unwrap().as_deref(), Some("other"));

    // if_changed: no write when equal, write when different.
    assert!(!set_repo_meta_if_changed(&conn, LEGACY_REPO_ID, "k", "v2").unwrap());
    assert!(set_repo_meta_if_changed(&conn, LEGACY_REPO_ID, "k", "v3").unwrap());
    assert_eq!(repo_meta(&conn, LEGACY_REPO_ID, "k").unwrap().as_deref(), Some("v3"));

    // Delete is scoped and idempotent.
    delete_repo_meta(&conn, LEGACY_REPO_ID, "k").unwrap();
    assert!(repo_meta(&conn, LEGACY_REPO_ID, "k").unwrap().is_none());
    assert_eq!(
        repo_meta(&conn, "repo-b", "k").unwrap().as_deref(),
        Some("other"),
        "delete does not cross repos"
    );
    delete_repo_meta(&conn, LEGACY_REPO_ID, "k").unwrap(); // no-op when already absent
}

/// FINDING 1 regression: an ALREADY-ADOPTED V038 DB (placeholder deleted, one REAL `repos` row)
/// must forward-migrate through V039 without tripping the `repo_meta → repos` FK. V039 relocates
/// the per-repo meta under the SOLE `repos` row — the real id here — not a hardcoded
/// `__unassigned__` placeholder (gone after adoption). With `foreign_keys = ON` (production, via
/// `IndexConnection`), the old hardcoded-placeholder insert ABORTS `migrate_forward` (`INSERT OR
/// IGNORE` does NOT suppress an immediate FK violation); with it off it orphans rows
/// `single_repo_id` can never resolve. Asserting the keys land under the real id — and the
/// migration completes — covers both.
#[test]
fn migration_039_relocates_under_the_real_id_on_an_adopted_v038_db() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    // Match production: the FK is enforced, so a relocation targeting the vanished placeholder
    // aborts (rather than silently orphaning the rows under a dangling id).
    conn.execute_batch("PRAGMA foreign_keys = ON;").expect("enable FK enforcement");
    schema::apply(&conn, &crate::index::migration_hooks()).expect("apply");

    // The pre-relocation legacy shape: the per-repo keys still sit in the GLOBAL k/v tables.
    upsert_meta(&conn, "index_meta", "source_root", "/src/repo");
    upsert_meta(&conn, "index_meta", "indexed_at_ms", "5000");
    upsert_meta(&conn, "reconcile_meta", "embedding_active_model_version", "hash-v1");

    // Adopt: the placeholder row is deleted, leaving exactly one REAL repos row.
    register_repo(
        &conn,
        &identity("repo-abc", "myrepo"),
        Path::new("/src/repo"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 0, "adopted: placeholder gone");

    // Rewind the ledger to V038 and forward-migrate: V039 re-runs against the adopted DB.
    truncate_schema_to(&conn, 38);
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);
    schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("V039 forward-migrates an adopted DB without an FK abort");

    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema reaches LATEST after the forward migrate",
    );
    // The keys land under the REAL id (never the vanished placeholder); `single_repo_id` resolves
    // it.
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), "repo-abc");
    for (key, value) in [
        ("source_root", "/src/repo"),
        ("indexed_at_ms", "5000"),
        ("embedding_active_model_version", "hash-v1"),
    ] {
        assert_eq!(
            rag_rat_db::meta::repo_meta(&conn, "repo-abc", key).unwrap().as_deref(),
            Some(value),
            "{key} relocated under the real repo id",
        );
    }
    let placeholder_meta: i64 = conn
        .query_row("SELECT COUNT(*) FROM repo_meta WHERE repo_id = ?1", [LEGACY_REPO_ID], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(placeholder_meta, 0, "nothing relocated under the vanished placeholder");
}
