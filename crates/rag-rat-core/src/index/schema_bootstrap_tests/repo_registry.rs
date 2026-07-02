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

/// Fresh `apply` creates the three registry tables with their exact columns, STRICT, and seeds the
/// single adoption placeholder whose id MUST equal `LEGACY_REPO_ID`. This is also the current
/// schema-tip test, so it owns the absolute `LATEST_SCHEMA_VERSION` pin (V037's test relinquished
/// it — see the hardcoded-LATEST footgun).
#[test]
fn migration_038_creates_repos_registry_tables() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    assert_eq!(schema::LATEST_SCHEMA_VERSION, 38, "V038 is the schema tip");

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
