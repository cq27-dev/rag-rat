//! V038 (memory-sync phase A): the `repos` / `repo_roots` / `repo_meta` registry + `register_repo`
//! adoption. Bootstrap-migration coverage follows the directory conventions (fresh `apply`, forward
//! path, deferred-absence anchored to the migration DDL in isolation — see the directory memory).

use super::*;
use crate::index::schema::{self, LEGACY_REPO_ID, register_repo};
use crate::repo_identity::RepoIdentity;

fn identity(repo_id: &str, display_name: &str) -> RepoIdentity {
    RepoIdentity { repo_id: repo_id.to_string(), display_name: display_name.to_string() }
}

fn repo_row_count(conn: &rusqlite::Connection, repo_id: &str) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM repos WHERE repo_id = ?1", [repo_id], |r| r.get(0))
        .unwrap()
}

fn root_count(conn: &rusqlite::Connection, repo_id: &str) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM repo_roots WHERE repo_id = ?1", [repo_id], |r| r.get(0))
        .unwrap()
}

/// Upsert a key into a `(key, value)` k/v table (`index_meta` / `reconcile_meta`) — seeds the
/// pre-relocation state a legacy DB carries.
fn upsert_meta(conn: &rusqlite::Connection, table: &str, key: &str, value: &str) {
    conn.execute(
        &format!(
            "INSERT INTO {table}(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value"
        ),
        [key, value],
    )
    .unwrap();
}

/// Whether `table` still holds `key` (used to assert relocated keys are gone / retained keys stay).
fn meta_present(conn: &rusqlite::Connection, table: &str, key: &str) -> bool {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE key = ?1"), [key], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap()
        > 0
}

/// Fresh `apply` creates the three registry tables with their exact columns, STRICT, and seeds the
/// single adoption placeholder whose id MUST equal `LEGACY_REPO_ID`. The absolute
/// `LATEST_SCHEMA_VERSION` pin moved to the new tip's test (`migration_039_*`); this one uses only
/// the symbolic `current_version == LATEST_SCHEMA_VERSION` check (the hardcoded-LATEST footgun).
#[test]
fn migration_038_creates_repos_registry_tables() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");

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

    schema::apply_repos_registry(&bare).expect("V038 DDL applies standalone on a bare conn");

    for table in ["repos", "repo_roots", "repo_meta"] {
        assert!(conn_table_exists(&bare, table), "V038 creates {table}");
    }
    assert_eq!(repo_row_count(&bare, LEGACY_REPO_ID), 1, "seeds the adoption placeholder");
}

/// A V037 index gains the registry on `migrate_forward`.
#[test]
fn migration_038_forward_migrates_a_v037_index() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");

    // Revert to the V037 shape: drop children before the parent (FK), then drop the ledger row.
    conn.execute_batch("DROP TABLE repo_meta; DROP TABLE repo_roots; DROP TABLE repos;")
        .expect("revert to V037 shape");
    truncate_schema_to(&conn, 37);
    assert_eq!(
        schema::status(&conn).unwrap().state,
        schema::SchemaState::Older,
        "schema is Older after removing the V038 ledger row"
    );

    schema::migrate_forward(&conn).expect("migrate_forward");
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

/// Fresh registration ADOPTS the placeholder in place: the `__unassigned__` row becomes the real
/// repo, no placeholder remains, and the working-tree root is recorded.
#[test]
fn register_repo_adopts_the_placeholder() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");

    let returned =
        register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/myrepo"), 123)
            .expect("register");
    assert_eq!(returned, "repo-abc");

    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 0, "placeholder is gone after adoption");
    let (display, at_ms): (String, i64) = conn
        .query_row(
            "SELECT display_name, registered_at_ms FROM repos WHERE repo_id=?1",
            ["repo-abc"],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(display, "myrepo");
    assert_eq!(at_ms, 123, "adoption stamps the injected now_ms");

    let root: String = conn
        .query_row("SELECT root FROM repo_roots WHERE repo_id=?1", ["repo-abc"], |r| r.get(0))
        .unwrap();
    assert_eq!(root, "/src/myrepo");
}

/// Re-applying the full schema AFTER adoption must NOT resurrect the `__unassigned__` placeholder.
/// `schema::apply` re-runs every additive migration (this is the exact path
/// `IndexDatabase::rebuild` takes via `create_or_migrate` on an already-migrated DB), so the V038
/// seed is conditional on "no real repo row yet". An unconditional `INSERT OR IGNORE` would re-mint
/// the placeholder after adoption UPDATE'd its PK away — leaving both the real repo and the legacy
/// marker. A3 extends adoption to more direct-scoped tables, so this invariant has to hold before
/// then.
#[test]
fn reapplying_schema_after_adoption_does_not_resurrect_the_placeholder() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/myrepo"), 1).unwrap();
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 0, "adopted: placeholder gone");

    // The exact re-run `create_or_migrate` (hence `rebuild`) performs on an existing index.
    schema::apply(&conn).expect("re-apply is idempotent on an already-migrated DB");

    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 0, "placeholder must NOT reappear");
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 1, "exactly one repos row (the real one) remains");
    assert_eq!(repo_row_count(&conn, "repo-abc"), 1, "the adopted repo survives the re-apply");
}

/// The cross-phase interaction of V038's conditional seed, the one-repos-row invariant, and V039's
/// per-repo `repo_meta`: an ADOPTED DB carrying relocated meta must survive a full `schema::apply`
/// re-run (the `create_or_migrate`/`rebuild` path) with its identity and meta intact. If the V038
/// seed regressed to an unconditional `INSERT OR IGNORE`, the re-apply would re-mint the
/// placeholder beside the real row — two `repos` rows — which trips `single_repo_id`'s one-row
/// `debug_assert` AND leaves it resolving an arbitrary repo, so the per-repo `repo_meta` accessors
/// would read the wrong scope. This pins BOTH sides at once: after re-apply `single_repo_id` still
/// returns the real id, and the meta rows stay under it (never resurrected under the placeholder).
#[test]
fn reapplying_schema_after_adoption_keeps_single_repo_id_and_repo_meta_under_the_real_id() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    // As V039 leaves a not-yet-adopted DB: per-repo meta under the placeholder.
    crate::index::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "source_root", "/src/repo").unwrap();
    crate::index::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms", "9").unwrap();

    register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/repo"), 1).unwrap();
    // Adoption re-pointed the meta to the real id and `single_repo_id` resolves it.
    assert_eq!(
        schema::single_repo_id(&conn).unwrap(),
        "repo-abc",
        "adopted: real id is the sole repo"
    );

    // The exact re-run `create_or_migrate` (hence `rebuild`) performs on an existing index.
    schema::apply(&conn).expect("re-apply is idempotent on an already-migrated DB");

    // `single_repo_id` still resolves the real id — its internal one-row `debug_assert` also fires
    // if the conditional seed regressed and resurrected the placeholder beside the real row.
    assert_eq!(
        schema::single_repo_id(&conn).unwrap(),
        "repo-abc",
        "re-apply leaves the real id as the sole repo (no placeholder resurrected)"
    );
    // The relocated meta stays scoped to the real id, with its values, across the re-apply.
    assert_eq!(
        crate::index::meta::repo_meta(&conn, "repo-abc", "source_root").unwrap().as_deref(),
        Some("/src/repo"),
    );
    assert_eq!(
        crate::index::meta::repo_meta(&conn, "repo-abc", "indexed_at_ms").unwrap().as_deref(),
        Some("9"),
    );
    let placeholder_meta: i64 = conn
        .query_row("SELECT COUNT(*) FROM repo_meta WHERE repo_id = ?1", [LEGACY_REPO_ID], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(placeholder_meta, 0, "no repo_meta rows resurrected under the placeholder");
}

/// Defense in depth (FINDING 3): even though the resolver refuses a pinned placeholder, a
/// hand-built `RepoIdentity` carrying the reserved marker must be REFUSED by `register_repo` —
/// otherwise adoption degenerates: `real_repo_ids` filters the marker, the adoption UPDATE rewrites
/// the placeholder PK to itself, roots pool under the marker, and registration reports success
/// while the DB stays unadopted. The exact degenerate sequence: pin the marker → register → assert
/// error, DB unchanged; then a real registration still adopts cleanly.
#[test]
fn register_repo_refuses_the_reserved_placeholder_and_stays_adoptable() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");

    let err =
        register_repo(&conn, &identity(LEGACY_REPO_ID, "myrepo"), Path::new("/src/myrepo"), 1)
            .expect_err("registering the reserved placeholder must be refused");
    assert!(err.to_string().contains(LEGACY_REPO_ID), "refusal names the reserved value: {err}");

    // DB unchanged: exactly the placeholder row, no real repo, no root recorded under the marker.
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 1, "placeholder untouched");
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 1, "no extra repos row minted");
    let roots: i64 = conn.query_row("SELECT COUNT(*) FROM repo_roots", [], |r| r.get(0)).unwrap();
    assert_eq!(roots, 0, "no root recorded under the marker");

    // A subsequent REAL registration adopts cleanly (the failed attempt left nothing behind).
    register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/myrepo"), 2)
        .expect("a real repo still adopts after the refused placeholder attempt");
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 0, "placeholder adopted away");
    assert_eq!(repo_row_count(&conn, "repo-abc"), 1, "the real repo owns the DB");
    assert_eq!(root_count(&conn, "repo-abc"), 1, "its root is recorded");
}

/// An empty or whitespace-only repo_id cannot scope rows, so `register_repo` refuses it (defense in
/// depth alongside the reserved-marker guard) rather than adopting the placeholder under a blank
/// id.
#[test]
fn register_repo_refuses_an_empty_or_whitespace_repo_id() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");

    for blank in ["", "   "] {
        let err = register_repo(&conn, &identity(blank, "myrepo"), Path::new("/src/myrepo"), 1)
            .expect_err("an empty/whitespace repo_id must be refused");
        assert!(err.to_string().contains("empty"), "refusal explains the empty id: {err}");
    }
    // Untouched: still just the placeholder, nothing minted by the refusals.
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 1, "placeholder untouched by refusals");
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 1, "no rows minted by the refused blank registrations");
}

/// Re-registering the same repo+root is a no-op (no duplicate rows).
#[test]
fn register_repo_is_idempotent() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    let id = identity("repo-abc", "myrepo");

    register_repo(&conn, &id, Path::new("/src/myrepo"), 1).unwrap();
    register_repo(&conn, &id, Path::new("/src/myrepo"), 2).unwrap();

    assert_eq!(repo_row_count(&conn, "repo-abc"), 1);
    assert_eq!(root_count(&conn, "repo-abc"), 1, "same root is not duplicated");
}

/// A second root path for the SAME repo (a worktree/clone on the same machine) appends a
/// `repo_roots` row without minting a new repo.
#[test]
fn register_repo_appends_a_second_root() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    let id = identity("repo-abc", "myrepo");

    register_repo(&conn, &id, Path::new("/src/myrepo"), 1).unwrap();
    register_repo(&conn, &id, Path::new("/src/myrepo-worktree"), 2).unwrap();

    assert_eq!(repo_row_count(&conn, "repo-abc"), 1, "still one repo");
    assert_eq!(root_count(&conn, "repo-abc"), 2, "both roots recorded");
}

/// Once a real repo owns the DB, registering a DIFFERENT real repo is REFUSED (the single-repo
/// invariant for phase A — multi-repo registration lands with the default-path flip).
#[test]
fn register_repo_refuses_a_different_real_repo() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    register_repo(&conn, &identity("repo-abc", "a"), Path::new("/src/a"), 1).unwrap();

    let err = register_repo(&conn, &identity("repo-xyz", "b"), Path::new("/src/b"), 2)
        .expect_err("a different real repo must be refused");
    assert!(err.to_string().contains("repo-abc"), "refusal names the incumbent repo: {err}");
    // The incumbent is untouched; the intruder never landed.
    assert_eq!(repo_row_count(&conn, "repo-abc"), 1);
    assert_eq!(repo_row_count(&conn, "repo-xyz"), 0);
}

// --- V039: per-repo meta relocation (memory-sync phase A2) ---

/// Fresh `apply` runs V039; it relocates the listed per-repo singleton keys into `repo_meta` under
/// the placeholder and leaves the machine-level keys in their global tables. This is the schema
/// tip, so it owns the absolute `LATEST_SCHEMA_VERSION` pin.
#[test]
fn migration_039_relocates_per_repo_meta_and_leaves_global_keys() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    assert_eq!(schema::LATEST_SCHEMA_VERSION, 39, "V039 is the schema tip");
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST after V039"
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
    schema::apply_move_per_repo_meta(&conn).expect("relocate");

    for (key, value) in [
        ("source_root", "/src/repo"),
        ("git_commit", "abc123"),
        ("active_embedding_model", "embedding-hash"),
        ("embedding_active_model_version", "hash-v1"),
        ("vector_int8_reencode_cursor", "42\nmodel-a"),
    ] {
        assert_eq!(
            crate::index::meta::repo_meta(&conn, LEGACY_REPO_ID, key).unwrap().as_deref(),
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
        crate::index::meta::repo_meta(&conn, LEGACY_REPO_ID, "generated_flags_version")
            .unwrap()
            .is_none(),
        "machine-level key did not leak into repo_meta"
    );
}

/// A legacy V038 index (per-repo keys still in the global tables, ledger at 38) gains the
/// relocation on `migrate_forward`.
#[test]
fn migration_039_forward_migrates_a_v038_index() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");

    // Simulate the V038 state: per-repo keys still in the GLOBAL tables, ledger reverted to 38.
    upsert_meta(&conn, "index_meta", "indexed_at_ms", "5000");
    upsert_meta(&conn, "reconcile_meta", "embedding_active_model_version", "hash-v1");
    truncate_schema_to(&conn, 38);
    assert_eq!(
        schema::status(&conn).unwrap().state,
        schema::SchemaState::Older,
        "schema is Older after removing the V039 ledger row"
    );

    schema::migrate_forward(&conn).expect("migrate_forward");

    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema is at LATEST after forward migrate"
    );
    assert_eq!(
        crate::index::meta::repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms").unwrap().as_deref(),
        Some("5000"),
        "index_meta key relocated on forward migrate"
    );
    assert_eq!(
        crate::index::meta::repo_meta(&conn, LEGACY_REPO_ID, "embedding_active_model_version")
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
    schema::apply_repos_registry(&bare).expect("V038 registry");
    upsert_meta(&bare, "index_meta", "fts_dirty", "true");
    upsert_meta(&bare, "reconcile_meta", "vector_int8_reencode_cursor", "7\nm");

    schema::apply_move_per_repo_meta(&bare).expect("V039 relocation standalone");
    schema::apply_move_per_repo_meta(&bare).expect("V039 relocation is idempotent");

    assert_eq!(
        crate::index::meta::repo_meta(&bare, LEGACY_REPO_ID, "fts_dirty").unwrap().as_deref(),
        Some("true"),
    );
    assert_eq!(
        crate::index::meta::repo_meta(&bare, LEGACY_REPO_ID, "vector_int8_reencode_cursor")
            .unwrap()
            .as_deref(),
        Some("7\nm"),
    );
    assert!(!meta_present(&bare, "index_meta", "fts_dirty"));
    assert!(!meta_present(&bare, "reconcile_meta", "vector_int8_reencode_cursor"));
    // Re-run left exactly one relocated row (no duplicate from the second pass).
    let count: i64 = bare
        .query_row(
            "SELECT COUNT(*) FROM repo_meta WHERE repo_id = ?1 AND key = 'fts_dirty'",
            [LEGACY_REPO_ID],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "re-run does not duplicate the relocated row");
}

/// V039 leaves the relocated meta under the placeholder repo_id; `register_repo` adoption MUST
/// carry those rows over to the real repo_id (Step 4), so a post-migration open does not orphan
/// them.
#[test]
fn register_repo_adoption_relocates_repo_meta_rows() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    // As V039 leaves it: per-repo meta under the placeholder.
    crate::index::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "source_root", "/src/repo").unwrap();
    crate::index::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms", "9").unwrap();

    register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/repo"), 1).unwrap();

    assert_eq!(
        crate::index::meta::repo_meta(&conn, "repo-abc", "source_root").unwrap().as_deref(),
        Some("/src/repo"),
    );
    assert_eq!(
        crate::index::meta::repo_meta(&conn, "repo-abc", "indexed_at_ms").unwrap().as_deref(),
        Some("9"),
    );
    let placeholder_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM repo_meta WHERE repo_id = ?1", [LEGACY_REPO_ID], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        placeholder_rows, 0,
        "no repo_meta rows remain under the placeholder after adoption"
    );
}

/// `single_repo_id` returns the sole `repos` row — the placeholder before adoption, the real id
/// after — the connection-level stand-in the per-repo accessors resolve until A3.
#[test]
fn single_repo_id_returns_the_sole_repo() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    assert_eq!(
        schema::single_repo_id(&conn).unwrap(),
        LEGACY_REPO_ID,
        "the placeholder is the sole repo before adoption"
    );

    register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/repo"), 1).unwrap();
    assert_eq!(
        schema::single_repo_id(&conn).unwrap(),
        "repo-abc",
        "the adopted real id is the sole repo after registration"
    );
}

/// The `repo_meta` accessors: upsert, read, no-op-if-unchanged, and delete — each scoped by
/// `(repo_id, key)` so the same key under a different repo is independent.
#[test]
fn repo_meta_accessors_round_trip_and_scope_by_repo() {
    use crate::index::meta::{
        delete_repo_meta, repo_meta, set_repo_meta, set_repo_meta_if_changed,
    };
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
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
    schema::apply(&conn).expect("apply");

    // The pre-relocation legacy shape: the per-repo keys still sit in the GLOBAL k/v tables.
    upsert_meta(&conn, "index_meta", "source_root", "/src/repo");
    upsert_meta(&conn, "index_meta", "indexed_at_ms", "5000");
    upsert_meta(&conn, "reconcile_meta", "embedding_active_model_version", "hash-v1");

    // Adopt: the placeholder row is deleted, leaving exactly one REAL repos row.
    register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/repo"), 1).unwrap();
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 0, "adopted: placeholder gone");

    // Rewind the ledger to V038 and forward-migrate: V039 re-runs against the adopted DB.
    truncate_schema_to(&conn, 38);
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);
    schema::migrate_forward(&conn)
        .expect("V039 forward-migrates an adopted DB without an FK abort");

    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema reaches LATEST after the forward migrate",
    );
    // The keys land under the REAL id (never the vanished placeholder); `single_repo_id` resolves
    // it.
    assert_eq!(schema::single_repo_id(&conn).unwrap(), "repo-abc");
    for (key, value) in [
        ("source_root", "/src/repo"),
        ("indexed_at_ms", "5000"),
        ("embedding_active_model_version", "hash-v1"),
    ] {
        assert_eq!(
            crate::index::meta::repo_meta(&conn, "repo-abc", key).unwrap().as_deref(),
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

/// FINDING 2 (atomicity): `register_repo`'s adoption — insert the real row, re-point `repo_meta`,
/// drop the placeholder, record the root — runs in ONE transaction, so a failure mid-sequence rolls
/// the WHOLE thing back. Without it, a crash after the insert but before the delete would leave
/// BOTH the real row and the placeholder: the "already registered" fast path would then never
/// repair it, and `single_repo_id`'s one-row expectation would break. Forced here with a temporary
/// `BEFORE DELETE ON repos` trigger that RAISEs on the placeholder delete — adoption must return
/// Err with the DB FULLY unchanged (placeholder present, its `repo_meta` rows intact, no real row,
/// no roots); after dropping the trigger, adoption succeeds cleanly, proving the failed attempt
/// left nothing behind.
#[test]
fn register_repo_adoption_is_atomic_on_a_mid_sequence_failure() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    conn.execute_batch("PRAGMA foreign_keys = ON;").expect("enable FK enforcement");
    schema::apply(&conn).expect("apply");
    // As V039 leaves a not-yet-adopted DB: per-repo meta under the placeholder.
    crate::index::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "source_root", "/src/repo").unwrap();
    crate::index::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms", "9").unwrap();

    // Fail the adoption at its LAST mutation (the placeholder delete), mid-transaction.
    conn.execute_batch(
        "CREATE TRIGGER fail_placeholder_delete BEFORE DELETE ON repos
         WHEN OLD.repo_id = '__unassigned__'
         BEGIN SELECT RAISE(ABORT, 'injected adoption failure'); END;",
    )
    .expect("install failure trigger");

    let err = register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/repo"), 1)
        .expect_err("adoption must fail while the trigger blocks the placeholder delete");
    assert!(
        err.to_string().contains("injected adoption failure"),
        "surfaces the trigger RAISE: {err}",
    );

    // The transaction rolled back: the DB is the exact pre-adoption state.
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 1, "placeholder survives the rollback");
    assert_eq!(repo_row_count(&conn, "repo-abc"), 0, "the half-inserted real row rolled back");
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 1, "exactly the placeholder row remains");
    assert_eq!(
        crate::index::meta::repo_meta(&conn, LEGACY_REPO_ID, "source_root").unwrap().as_deref(),
        Some("/src/repo"),
        "repo_meta stays under the placeholder (the re-point rolled back)",
    );
    assert_eq!(
        crate::index::meta::repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms").unwrap().as_deref(),
        Some("9"),
    );
    let real_meta: i64 = conn
        .query_row("SELECT COUNT(*) FROM repo_meta WHERE repo_id = 'repo-abc'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(real_meta, 0, "no repo_meta rows re-pointed to the real id");
    let roots: i64 = conn.query_row("SELECT COUNT(*) FROM repo_roots", [], |r| r.get(0)).unwrap();
    assert_eq!(roots, 0, "no root recorded (record_repo_root never committed)");

    // Remove the fault and adopt again: a clean success, proving nothing was left half-done.
    conn.execute_batch("DROP TRIGGER fail_placeholder_delete;").expect("drop trigger");
    register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/repo"), 2)
        .expect("adoption succeeds once the fault is removed");
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 0, "placeholder adopted away");
    assert_eq!(repo_row_count(&conn, "repo-abc"), 1, "the real repo owns the DB");
    assert_eq!(schema::single_repo_id(&conn).unwrap(), "repo-abc");
    assert_eq!(
        crate::index::meta::repo_meta(&conn, "repo-abc", "source_root").unwrap().as_deref(),
        Some("/src/repo"),
        "meta carried over to the real id on the successful adoption",
    );
    assert_eq!(root_count(&conn, "repo-abc"), 1, "its root is recorded");
}
