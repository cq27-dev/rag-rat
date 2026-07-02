//! V038 (memory-sync phase A): the `repos` / `repo_roots` / `repo_meta` registry + `register_repo`
//! adoption. Bootstrap-migration coverage follows the directory conventions (fresh `apply`, forward
//! path, deferred-absence anchored to the migration DDL in isolation — see the directory memory).

use super::*;
use crate::index::schema::{self, LEGACY_REPO_ID, register_repo};
use crate::repo_identity::{RepoIdentity, RepoIdentityClass};

fn identity(repo_id: &str, display_name: &str) -> RepoIdentity {
    RepoIdentity {
        repo_id: repo_id.to_string(),
        display_name: display_name.to_string(),
        // The class is a scoping-neutral tag; ADOPTION of a placeholder / refusal of a second repo
        // ignore it — only the LocalOnly→Portable upgrade branch reads it (see `identity_local`).
        class: RepoIdentityClass::Portable,
        shallow_boundary: Vec::new(),
    }
}

/// A machine-local (`LocalOnly`) identity — a `local:`-prefixed id, as a cut shallow clone derives.
/// `register_repo` refuses one against an existing real repo (a deepened clone must not DOWNGRADE a
/// portable id), and only UPGRADES *away* from one when an incoming `Portable` id arrives — after
/// PROVING the deepened clone reaches the recorded `shallow_boundary` (pass the boundary commits a
/// later portable clone must reach; empty ⇒ no proof recorded ⇒ the upgrade is refused).
fn identity_local(
    repo_id: &str,
    display_name: &str,
    shallow_boundary: Vec<String>,
) -> RepoIdentity {
    RepoIdentity {
        repo_id: repo_id.to_string(),
        display_name: display_name.to_string(),
        class: RepoIdentityClass::LocalOnly,
        shallow_boundary,
    }
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
/// placeholder beside the real row — two `repos` rows — leaving `sole_repo_id` to resolve an
/// arbitrary repo, so the per-repo `repo_meta` accessors would read the wrong scope. This pins BOTH
/// sides at once: after re-apply the real id is still the SOLE repos row, and the meta rows stay
/// under it (never resurrected under the placeholder).
#[test]
fn reapplying_schema_after_adoption_keeps_single_repo_id_and_repo_meta_under_the_real_id() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    // As V039 leaves a not-yet-adopted DB: per-repo meta under the placeholder.
    crate::index::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "source_root", "/src/repo").unwrap();
    crate::index::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms", "9").unwrap();

    register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/repo"), 1).unwrap();
    // Adoption re-pointed the meta to the real id and `sole_repo_id` resolves it.
    assert_eq!(
        schema::sole_repo_id(&conn).unwrap(),
        "repo-abc",
        "adopted: real id is the sole repo"
    );

    // The exact re-run `create_or_migrate` (hence `rebuild`) performs on an existing index.
    schema::apply(&conn).expect("re-apply is idempotent on an already-migrated DB");

    // Exactly one repos row (the real id) survives the re-apply — the conditional seed did NOT
    // resurrect the placeholder beside it (the resolver no longer carries a one-row `debug_assert`,
    // so this asserts the invariant explicitly).
    let repos_total: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0)).unwrap();
    assert_eq!(repos_total, 1, "exactly one repos row after re-apply");
    assert_eq!(
        schema::sole_repo_id(&conn).unwrap(),
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
/// the placeholder and leaves the machine-level keys in their global tables. (The absolute
/// `LATEST_SCHEMA_VERSION` pin moved to `migration_040_*`, the new tip; this uses only the symbolic
/// `current_version == LATEST` check.)
#[test]
fn migration_039_relocates_per_repo_meta_and_leaves_global_keys() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
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
    // `git_commit` is a genuinely per-repo key V039 relocates; the reencode cursor moves too. (The
    // `fts_*` trio + `content_revision` are RECLASSIFIED GLOBAL by V040, so V039 no longer
    // relocates them — covered by `v039_leaves_reclassified_global_keys_in_index_meta`.)
    upsert_meta(&bare, "index_meta", "git_commit", "abc123");
    upsert_meta(&bare, "reconcile_meta", "vector_int8_reencode_cursor", "7\nm");

    schema::apply_move_per_repo_meta(&bare).expect("V039 relocation standalone");
    schema::apply_move_per_repo_meta(&bare).expect("V039 relocation is idempotent");

    assert_eq!(
        crate::index::meta::repo_meta(&bare, LEGACY_REPO_ID, "git_commit").unwrap().as_deref(),
        Some("abc123"),
    );
    assert_eq!(
        crate::index::meta::repo_meta(&bare, LEGACY_REPO_ID, "vector_int8_reencode_cursor")
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
    schema::apply_repos_registry(&bare).expect("V038 registry");
    // The 4 reclassified-global index_meta keys, plus one genuinely-per-repo key as a positive
    // control that DOES relocate.
    for key in ["content_revision", "fts_dirty", "fts_source_revision", "fts_synced_at_ms"] {
        upsert_meta(&bare, "index_meta", key, "global-val");
    }
    upsert_meta(&bare, "index_meta", "source_root", "/src/repo");

    schema::apply_move_per_repo_meta(&bare).expect("V039 relocation standalone");

    // The 4 global keys STAY in index_meta and never enter repo_meta.
    for key in ["content_revision", "fts_dirty", "fts_source_revision", "fts_synced_at_ms"] {
        assert!(meta_present(&bare, "index_meta", key), "{key} stays GLOBAL in index_meta");
        assert!(
            crate::index::meta::repo_meta(&bare, LEGACY_REPO_ID, key).unwrap().is_none(),
            "{key} did NOT relocate to repo_meta",
        );
    }
    // The per-repo control still relocated — V039's behavior for non-reclassified keys is intact.
    assert!(!meta_present(&bare, "index_meta", "source_root"), "per-repo key still relocates");
    assert_eq!(
        crate::index::meta::repo_meta(&bare, LEGACY_REPO_ID, "source_root").unwrap().as_deref(),
        Some("/src/repo"),
    );
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
        schema::sole_repo_id(&conn).unwrap(),
        LEGACY_REPO_ID,
        "the placeholder is the sole repo before adoption"
    );

    register_repo(&conn, &identity("repo-abc", "myrepo"), Path::new("/src/repo"), 1).unwrap();
    assert_eq!(
        schema::sole_repo_id(&conn).unwrap(),
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
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), "repo-abc");
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
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), "repo-abc");
    assert_eq!(
        crate::index::meta::repo_meta(&conn, "repo-abc", "source_root").unwrap().as_deref(),
        Some("/src/repo"),
        "meta carried over to the real id on the successful adoption",
    );
    assert_eq!(root_count(&conn, "repo-abc"), 1, "its root is recorded");
}

// --- V040: repo_id scoping on the core tables (memory-sync phase A3) ---

/// The pre-V040 (post-V039) shape of every core table [`schema::apply_repo_id_core_scoping`]
/// transforms, plus the V038 registry it relocates meta under — built in ISOLATION so the migration
/// is exercised against its own inputs, not the full ladder (the directory's "assert deferred
/// absence / rebuild behavior in isolation" rule). `files`/`packages`/… carry NO `repo_id`;
/// `parser_failures` is id-keyed; `git_commits` is `hash`-PK with `commit_fts` external content and
/// a `git_file_changes(commit_hash)` FK — exactly what V040 rebuilds.
fn seed_pre_v040_core_schema(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "
        CREATE TABLE index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE files(
            id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL, language TEXT NOT NULL,
            kind TEXT NOT NULL, sha256 TEXT NOT NULL, modified_at_ms INTEGER NOT NULL,
            generated INTEGER NOT NULL DEFAULT 0, indexed_at_ms INTEGER NOT NULL,
            indexed_revision TEXT NOT NULL DEFAULT '', commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '', has_test_code INTEGER NOT NULL DEFAULT 0,
            UNIQUE(path, commit_sha, worktree_id));
        CREATE TABLE packages(
            id INTEGER PRIMARY KEY AUTOINCREMENT, manifest_dir TEXT NOT NULL,
            commit_sha TEXT NOT NULL DEFAULT '', worktree_id TEXT NOT NULL DEFAULT '',
            local_roots_json TEXT NOT NULL DEFAULT '[]',
            UNIQUE(manifest_dir, commit_sha, worktree_id)) STRICT;
        CREATE TABLE logical_symbols(
            id INTEGER PRIMARY KEY AUTOINCREMENT, language TEXT NOT NULL, path TEXT NOT NULL,
            logical_name TEXT NOT NULL, qualified_name_id INTEGER, kind TEXT NOT NULL,
            variant_count INTEGER NOT NULL, group_reason TEXT NOT NULL);
        CREATE TABLE docs(
            id INTEGER PRIMARY KEY AUTOINCREMENT, chunk_id INTEGER NOT NULL,
            source_kind TEXT NOT NULL, heading_path TEXT);
        CREATE TABLE parser_failures(
            id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL, language TEXT NOT NULL,
            message TEXT NOT NULL);
        CREATE TABLE git_commits(
            hash TEXT PRIMARY KEY, author_name TEXT NOT NULL, author_email TEXT NOT NULL,
            authored_at_s INTEGER NOT NULL, committed_at_s INTEGER NOT NULL, subject TEXT NOT NULL,
            body TEXT NOT NULL, changed_file_count INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE git_file_changes(
            id INTEGER PRIMARY KEY AUTOINCREMENT, commit_hash TEXT NOT NULL, path TEXT NOT NULL,
            additions INTEGER, deletions INTEGER, change_kind TEXT NOT NULL DEFAULT 'modified',
            FOREIGN KEY(commit_hash) REFERENCES git_commits(hash) ON DELETE CASCADE);
        CREATE VIRTUAL TABLE commit_fts USING fts5(
            subject, body, content='git_commits', content_rowid='rowid', tokenize='porter');
        -- The logical-symbol companion tables a real pre-V040 DB carries (all baseline). V040's
        -- logical-symbol id realign (repo_id fold) reads name_strings / symbols /
        -- logical_symbol_members for the hash inputs and re-points logical_symbol_monikers +
        -- repo_memory_bindings / repo_memory_call_paths; the tables must exist for that pass to
        -- PREPARE even when (as here) logical_symbols is empty, so it is a no-op.
        CREATE TABLE name_strings(id INTEGER PRIMARY KEY, value TEXT NOT NULL UNIQUE) STRICT;
        CREATE TABLE symbols(id INTEGER PRIMARY KEY AUTOINCREMENT, signature TEXT);
        CREATE TABLE logical_symbol_members(
            logical_symbol_id INTEGER NOT NULL, symbol_id INTEGER NOT NULL, cfg_expr TEXT,
            signature_hash TEXT, start_line INTEGER NOT NULL DEFAULT 0,
            end_line INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(logical_symbol_id, symbol_id),
            -- The real ON DELETE CASCADE FK: it is exactly what forces the id realign to run with \
         FK
            -- OFF (V040) or DEFERRED (adoption), so the fixture carries it to exercise those \
         paths.
            FOREIGN KEY(logical_symbol_id) REFERENCES logical_symbols(id) ON DELETE CASCADE);
        CREATE TABLE logical_symbol_monikers(
            logical_symbol_id INTEGER NOT NULL, tool TEXT NOT NULL, tool_version TEXT NOT NULL,
            moniker TEXT NOT NULL, computed_at INTEGER NOT NULL,
            PRIMARY KEY(logical_symbol_id, tool)) STRICT;
        CREATE TABLE repo_memory_bindings(
            memory_id TEXT NOT NULL, binding_kind TEXT NOT NULL, binding_id TEXT NOT NULL,
            logical_symbol_id INTEGER, PRIMARY KEY(memory_id, binding_kind, binding_id));
        CREATE TABLE repo_memory_call_paths(
            memory_id TEXT NOT NULL, start_logical_symbol_id INTEGER, end_logical_symbol_id \
         INTEGER,
            edge_sequence_hash TEXT NOT NULL, PRIMARY KEY(memory_id, edge_sequence_hash));
        ",
    )
    .unwrap();
    schema::apply_repos_registry(conn).expect("V038 registry seeds the placeholder");
}

/// Seed one git commit + its file change + its FTS entry into a pre-V040 fixture, returning the
/// hash.
fn seed_pre_v040_commit(conn: &rusqlite::Connection, hash: &str, subject: &str) {
    conn.execute(
        "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, committed_at_s, \
         subject, body, changed_file_count) VALUES (?1, 'a', 'a@b', 1, 1, ?2, 'body', 1)",
        rusqlite::params![hash, subject],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind) \
         VALUES (?1, 'src/lib.rs', 1, 0, 'modified')",
        [hash],
    )
    .unwrap();
    // Populate the external-content FTS from the seeded commit rows so it starts in sync (as a real
    // pre-V040 index would be) — V040's rebuild then has a consistent index to re-point.
    conn.execute_batch("INSERT INTO commit_fts(commit_fts) VALUES('rebuild');").unwrap();
}

/// Fresh `apply` runs V040: every direct-scoped core table gains `repo_id`, and the widened
/// UNIQUE/PK keys make same-path/same-hash rows distinct across repos. (The absolute
/// `LATEST_SCHEMA_VERSION` pin moved to `migration_041_*`, the new tip; this uses only the symbolic
/// `current_version == LATEST` check.)
#[test]
fn migration_040_scopes_core_tables() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    for table in [
        "files",
        "packages",
        "logical_symbols",
        "docs",
        "parser_failures",
        "git_commits",
        "git_file_changes",
    ] {
        assert!(
            conn_table_columns(&conn, table).contains(&"repo_id".to_string()),
            "{table} gains a direct repo_id column"
        );
    }
    // `parser_failures` dropped its bare autoincrement id for the `(repo_id, path)` PK.
    assert!(
        !conn_table_columns(&conn, "parser_failures").contains(&"id".to_string()),
        "parser_failures PK is (repo_id, path), no id column"
    );

    // files UNIQUE is now repo-scoped: the SAME (path, commit_sha, worktree_id) in two repos is
    // fine.
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, repo_id) \
         VALUES ('src/lib.rs', 'rust', 'source', 'a', 0, 0, 'repo-a')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, repo_id) \
         VALUES ('src/lib.rs', 'rust', 'source', 'a', 0, 0, 'repo-b')",
        [],
    )
    .expect("same path/commit/worktree in a DIFFERENT repo does not collide");
    let dup = conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, repo_id) \
         VALUES ('src/lib.rs', 'rust', 'source', 'a', 0, 0, 'repo-a')",
        [],
    );
    assert!(dup.is_err(), "the SAME repo/path/commit/worktree still conflicts");
}

/// V040's `git_commits` PK rebuild + `git_file_changes` composite FK + `commit_fts` re-point,
/// driven against the pre-V040 fixture IN ISOLATION: rows survive the rebuild, `commit_fts` still
/// MATCHes after the desync-safe `'rebuild'` (#51), and the migration RE-CONVERGES from a torn
/// intermediate state (a leftover scratch table from a crashed prior pass). Then `register_repo`
/// adoption re-points the placeholder rows — carrying `git_file_changes` along via the FK's ON
/// UPDATE CASCADE.
#[test]
fn migration_040_git_rebuild_preserves_rows_commit_fts_and_reconverges_from_torn_state() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v040_core_schema(&conn);
    seed_pre_v040_commit(&conn, "cafef00d", "alpha subject token");
    conn.execute(
        "INSERT INTO parser_failures(path, language, message) VALUES ('x.rs','rust','boom')",
        [],
    )
    .unwrap();

    // TORN STATE: a prior V040 pass crashed after creating a scratch table. The rebuild must drop
    // it and re-converge rather than fail on CREATE.
    conn.execute_batch(
        "CREATE TABLE files_new(bogus INTEGER); CREATE TABLE git_commits_new(bogus INTEGER);",
    )
    .unwrap();

    schema::apply_repo_id_core_scoping(&conn).expect("V040 converges from the torn state");

    // The scratch tables are gone; the transform completed.
    assert!(!conn_table_exists(&conn, "files_new"));
    assert!(!conn_table_exists(&conn, "git_commits_new"));
    assert!(conn_table_columns(&conn, "git_commits").contains(&"repo_id".to_string()));

    // The commit row survived, backfilled to the placeholder, and commit_fts still MATCHes it.
    let (hash, repo_id): (String, String) = conn
        .query_row("SELECT hash, repo_id FROM git_commits", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(hash, "cafef00d");
    assert_eq!(repo_id, LEGACY_REPO_ID, "existing rows backfill to the placeholder");
    let matched: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM commit_fts JOIN git_commits ON git_commits.rowid = \
             commit_fts.rowid WHERE commit_fts MATCH 'alpha'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(matched, 1, "commit_fts still MATCHes after the git_commits rebuild + 'rebuild'");
    // The composite FK holds: the file change carries the same placeholder repo_id.
    let fc_repo: String =
        conn.query_row("SELECT repo_id FROM git_file_changes", [], |r| r.get(0)).unwrap();
    assert_eq!(fc_repo, LEGACY_REPO_ID);

    // Idempotent re-run (the all-or-nothing sentinel short-circuits once files carries repo_id).
    schema::apply_repo_id_core_scoping(&conn).expect("re-apply is a clean no-op");

    // Adoption re-points git_commits.repo_id → real, and the ON UPDATE CASCADE carries the change
    // row.
    register_repo(&conn, &identity("repo-real", "r"), Path::new("/src/r"), 1).unwrap();
    let (gc_repo, fc_repo2): (String, String) = conn
        .query_row(
            "SELECT (SELECT repo_id FROM git_commits), (SELECT repo_id FROM git_file_changes)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(gc_repo, "repo-real", "adoption re-points git_commits");
    assert_eq!(fc_repo2, "repo-real", "ON UPDATE CASCADE carries git_file_changes along");
    // parser_failures was re-pointed too.
    let pf_repo: String =
        conn.query_row("SELECT repo_id FROM parser_failures", [], |r| r.get(0)).unwrap();
    assert_eq!(pf_repo, "repo-real", "adoption re-points parser_failures");
}

/// V040 in ISOLATION reunites the two active-model provenance stragglers (`active_embedding_model_
/// provisional`, `active_embedding_remote_config`) with their family in `repo_meta` — the keys A2's
/// V039 sweep left behind in `index_meta`.
#[test]
fn migration_040_reunites_active_model_provenance_meta_into_repo_meta() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    seed_pre_v040_core_schema(&conn);
    upsert_meta(&conn, "index_meta", "active_embedding_model_provisional", "1");
    upsert_meta(&conn, "index_meta", "active_embedding_remote_config", "{\"endpoint\":\"x\"}");
    upsert_meta(&conn, "index_meta", "generated_flags_version", "1"); // machine-level, stays

    schema::apply_repo_id_core_scoping(&conn).unwrap();

    for key in ["active_embedding_model_provisional", "active_embedding_remote_config"] {
        assert!(
            crate::index::meta::repo_meta(&conn, LEGACY_REPO_ID, key).unwrap().is_some(),
            "{key} relocated to repo_meta"
        );
        assert!(!meta_present(&conn, "index_meta", key), "{key} removed from index_meta");
    }
    assert!(
        meta_present(&conn, "index_meta", "generated_flags_version"),
        "machine key stays global"
    );
}

/// A V039 index forward-migrates to V040 on `migrate_forward` — reaching LATEST.
#[test]
fn migration_040_forward_migrates_a_v039_index() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    truncate_schema_to(&conn, 39);
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);
    schema::migrate_forward(&conn).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
}

/// P1 regression (the V039/e2a2bc5 class): on an ALREADY-ADOPTED pre-V040 DB (`repos` holds only
/// the REAL id; the placeholder row is gone), V040's rebuilds stamp existing rows with the STATIC
/// `__unassigned__` column DEFAULT — and the next `register_repo` takes the already-registered
/// fast path that never re-points, so without the in-migration backfill every row would orphan
/// under the placeholder and the real repo's scope view would see an EMPTY index after the
/// upgrade. `apply_repo_id_core_scoping` therefore resolves the SOLE `repos` row (the established
/// [`sole_repo_id`] pattern) and backfills every direct-scoped table to it — `git_file_changes`
/// explicitly, because FK enforcement is OFF inside the migration transaction so the `ON UPDATE
/// CASCADE` does not fire there.
#[test]
fn migration_040_backfills_an_adopted_pre_v040_db_under_the_real_repo_id() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v040_core_schema(&conn);
    seed_pre_v040_commit(&conn, "cafef00d", "alpha subject token");
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
         VALUES ('src/lib.rs', 'rust', 'source', 'sha', 0, 0)",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO packages(manifest_dir) VALUES ('.')", []).unwrap();
    conn.execute(
        "INSERT INTO logical_symbols(id, language, path, logical_name, kind, variant_count, \
         group_reason)
         VALUES (7, 'rust', 'src/lib.rs', 'f', 'function', 1, 'single')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO docs(chunk_id, source_kind) VALUES (1, 'doc_comment')", []).unwrap();
    conn.execute(
        "INSERT INTO parser_failures(path, language, message) VALUES ('x.rs', 'rust', 'boom')",
        [],
    )
    .unwrap();
    // A straggler meta key: the V040 relocation must land it under the real id too.
    upsert_meta(&conn, "index_meta", "active_embedding_model_provisional", "1");

    // Adopt as the PRE-V040 binary's `register_repo` left it (there were no direct-scoped tables
    // to re-point yet): real `repos` row in, `repo_meta` re-pointed, placeholder deleted.
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-adopted', 'r', \
         1)",
        [],
    )
    .unwrap();
    conn.execute("UPDATE repo_meta SET repo_id = 'repo-adopted' WHERE repo_id = ?1", [
        LEGACY_REPO_ID,
    ])
    .unwrap();
    conn.execute("DELETE FROM repos WHERE repo_id = ?1", [LEGACY_REPO_ID]).unwrap();

    schema::apply_repo_id_core_scoping(&conn).expect("V040 applies on an adopted DB");

    for table in [
        "files",
        "packages",
        "logical_symbols",
        "docs",
        "parser_failures",
        "git_commits",
        "git_file_changes",
    ] {
        let (total, under_real): (i64, i64) = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM(repo_id = 'repo-adopted'), 0) FROM {table}"
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(total > 0, "{table}: the fixture seeded at least one row");
        assert_eq!(under_real, total, "{table}: every row backfilled under the REAL repo id");
    }
    // The straggler relocation targeted the sole (real) repos row, never the vanished placeholder.
    assert_eq!(
        crate::index::meta::repo_meta(&conn, "repo-adopted", "active_embedding_model_provisional")
            .unwrap()
            .as_deref(),
        Some("1"),
        "V040 meta relocation lands under the real id on an adopted DB"
    );

    // The runtime fast path (next open re-registers the same repo) stays a no-op and leaves the
    // backfill intact.
    register_repo(&conn, &identity("repo-adopted", "r"), Path::new("/src/r"), 2).unwrap();
    let stranded: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE repo_id = ?1", [LEGACY_REPO_ID], |r| r.get(0))
        .unwrap();
    assert_eq!(stranded, 0, "nothing remains stranded under the placeholder");
}

// --- Logical-symbol id realign across the repo_id fold (A3, #413 finding #1) ---

/// Seed ONE logical symbol (`logical_id`) with a fully-recoverable key — a member symbol carrying
/// the signature, an interned qualified name — plus every kind of reference that must follow it: a
/// per-tool moniker, a memory binding, and a memory call-path (start + end). The next realign (V040
/// or adoption) re-derives the id under the folded hash and must carry ALL of them along.
fn seed_pre_v040_logical_symbol_with_a_bound_memory(conn: &rusqlite::Connection, logical_id: i64) {
    conn.execute("INSERT INTO name_strings(id, value) VALUES (1, 'mymod::my_fn')", []).unwrap();
    conn.execute("INSERT INTO symbols(id, signature) VALUES (100, 'fn my_fn()')", []).unwrap();
    conn.execute(
        "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, kind, \
         variant_count, group_reason)
         VALUES (?1, 'rust', 'src/lib.rs', 'my_fn', 1, 'function', 1, 'single')",
        [logical_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, end_line) \
         VALUES (?1, 100, 1, 3)",
        [logical_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO logical_symbol_monikers(logical_symbol_id, tool, tool_version, moniker, \
         computed_at) VALUES (?1, 'scip-rust', 'v1', 'moniker', 0)",
        [logical_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, logical_symbol_id) \
         VALUES ('mem-1', 'logical_symbol', 'b1', ?1)",
        [logical_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_call_paths(memory_id, start_logical_symbol_id, \
         end_logical_symbol_id, edge_sequence_hash) VALUES ('mem-1', ?1, ?1, 'h1')",
        [logical_id],
    )
    .unwrap();
}

/// The `logical_symbol_id` every reference table now points at (binding / call-path start+end /
/// moniker / member), asserted equal so a single value proves they all followed the realign.
fn bound_logical_symbol_id(conn: &rusqlite::Connection) -> i64 {
    let binding: i64 = conn
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = 'mem-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for (sql, label) in [
        ("SELECT logical_symbol_id FROM logical_symbol_members", "member"),
        ("SELECT logical_symbol_id FROM logical_symbol_monikers", "moniker"),
        ("SELECT start_logical_symbol_id FROM repo_memory_call_paths", "call-path start"),
        ("SELECT end_logical_symbol_id FROM repo_memory_call_paths", "call-path end"),
    ] {
        let other: i64 = conn.query_row(sql, [], |r| r.get(0)).unwrap();
        assert_eq!(other, binding, "the {label} reference must match the binding after realign");
    }
    // "Resolves to the SAME symbol": the id joins to a live logical symbol with the seeded content.
    let name: String = conn
        .query_row(
            "SELECT ls.logical_name FROM repo_memory_bindings b
               JOIN logical_symbols ls ON ls.id = b.logical_symbol_id
              WHERE b.memory_id = 'mem-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name, "my_fn", "the bound memory resolves to the same logical symbol");
    binding
}

/// #413 finding #1, migration half: folding `repo_id` into the logical-symbol id derivation (A3)
/// changes every id the next `rebuild_logical_symbols` produces, so a pre-V040 memory/oracle handle
/// would dangle. V040 must migrate the ids IN PLACE and carry every reference along, so a bound
/// memory still resolves to the same symbol under the new id.
#[test]
fn migration_040_realigns_logical_symbol_ids_and_carries_bound_memories() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v040_core_schema(&conn);
    let old_id = 424_242_i64; // an arbitrary pre-fold id
    seed_pre_v040_logical_symbol_with_a_bound_memory(&conn, old_id);

    schema::apply_repo_id_core_scoping(&conn).expect("V040 applies");

    // The symbol's id was re-derived (repo_id folded in), so it CHANGED from the pre-fold value.
    let new_id: i64 = conn.query_row("SELECT id FROM logical_symbols", [], |r| r.get(0)).unwrap();
    assert_ne!(new_id, old_id, "the id was re-derived under the repo_id-folded hash");
    // Every reference followed the symbol to its new id, and no orphan is left at the old id.
    assert_eq!(bound_logical_symbol_id(&conn), new_id, "all references follow the symbol");
    let stale: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM repo_memory_bindings WHERE logical_symbol_id = ?1",
            [old_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stale, 0, "no reference dangles at the pre-fold id");
}

/// #413 finding #1, adoption half: on a NOT-yet-adopted DB the V040 realign lands the id under the
/// PLACEHOLDER repo_id; adoption re-points `logical_symbols.repo_id` to the real id, which changes
/// the derived id AGAIN — so `register_repo` must realign a SECOND time (with FK checks deferred).
/// A bound memory must survive BOTH transitions — the common real-world upgrade path (a pre-V038 DB
/// with memories, adopted on first config-bearing open).
#[test]
fn adoption_realigns_logical_symbol_ids_so_pre_v040_memories_survive() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v040_core_schema(&conn);
    let old_id = 999_001_i64;
    seed_pre_v040_logical_symbol_with_a_bound_memory(&conn, old_id);

    schema::apply_repo_id_core_scoping(&conn).expect("V040 applies (unadopted → placeholder id)");
    let placeholder_id = bound_logical_symbol_id(&conn);
    assert_ne!(placeholder_id, old_id, "V040 already realigned under the placeholder repo_id");

    // Adopt the placeholder as a real repo — the id derivation changes with the real repo_id.
    register_repo(&conn, &identity("real-repo", "r"), Path::new("/src/r"), 1).unwrap();

    let real_id: i64 = conn
        .query_row("SELECT id FROM logical_symbols WHERE repo_id = 'real-repo'", [], |r| r.get(0))
        .unwrap();
    assert_ne!(real_id, placeholder_id, "adoption re-derived the id under the real repo_id");
    assert_eq!(
        bound_logical_symbol_id(&conn),
        real_id,
        "the pre-V040 memory survives adoption, still bound to the symbol under its real-repo id"
    );
}

// --- LocalOnly → Portable id upgrade (deepened shallow clone; #413 round-4 finding #4) ---

/// A DB first indexed under a machine-local `local:` id (a cut shallow clone) must UPGRADE in place
/// when the caller deepens it (`git fetch --unshallow` — our own remedy) and re-opens under a
/// portable id: every scoped row, `repo_meta`, `repo_roots`, and logical-symbol id re-points from
/// the local id to the portable one, and a bound memory survives (its `logical_symbol_id` follows
/// the realign). Without this the deepened clone would hit the "different real repo" refusal and
/// the existing index could never open again without deletion — a dead end.
#[test]
fn register_repo_upgrades_a_local_only_id_to_a_portable_id_in_place() {
    // A REAL git repo supplies the upgrade PROOF (round-6 P2 #4): the incoming deepened clone's
    // HEAD must reach the incumbent's recorded shallow boundary. The DB (in-memory) and the
    // repo are decoupled — register_repo re-points rows in `conn` while verifying ancestry
    // against `repo`.
    let repo = real_git_repo("upgrade-in-place");
    let boundary = head_commit_hash(&repo);

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v040_core_schema(&conn);
    // A file + a logical symbol with a bound memory — the rows the upgrade must re-point + realign.
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms) VALUES \
         ('src/lib.rs', 'rust', 'source', 'a', 0, 0)",
        [],
    )
    .unwrap();
    seed_pre_v040_logical_symbol_with_a_bound_memory(&conn, 777_001);

    schema::apply_repo_id_core_scoping(&conn).expect("V040 applies (unadopted → placeholder id)");

    // First registration: a cut shallow clone adopts under a machine-local id AND records its
    // shallow boundary (the proof material a later upgrade verifies against).
    let local_id = "local:deadbeefcafef00d";
    register_repo(&conn, &identity_local(local_id, "shallow", vec![boundary]), repo.as_path(), 1)
        .unwrap();
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), local_id, "adopted under the LocalOnly id");
    let local_symbol_id = bound_logical_symbol_id(&conn);

    // Deepen + re-open: the incoming identity is now Portable and its HEAD reaches the recorded
    // boundary → PROVEN → UPGRADE, not a refusal.
    let portable_id = "0abc123root";
    register_repo(&conn, &identity(portable_id, "deepened"), repo.as_path(), 2)
        .expect("a PROVEN Portable id against a local: incumbent UPGRADES in place, not refused");

    // The portable id now solely owns the DB; the local id is gone.
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), portable_id, "portable id owns the DB");
    assert_eq!(repo_row_count(&conn, local_id), 0, "the machine-local repos row is gone");
    assert_eq!(repo_row_count(&conn, portable_id), 1, "exactly the portable repos row remains");
    // Scoped rows + the recorded root re-pointed off the local id.
    assert_eq!(
        conn.query_row("SELECT repo_id FROM files", [], |r| r.get::<_, String>(0)).unwrap(),
        portable_id,
        "the file row re-pointed to the portable id",
    );
    assert_eq!(root_count(&conn, local_id), 0, "no root left under the local id");
    assert_eq!(root_count(&conn, portable_id), 1, "the root moved to the portable id");
    // The logical id re-derived under the portable fold, and the bound memory followed it.
    let portable_symbol_id: i64 =
        conn.query_row("SELECT id FROM logical_symbols", [], |r| r.get(0)).unwrap();
    assert_ne!(portable_symbol_id, local_symbol_id, "the id re-derived under the portable repo_id");
    assert_eq!(
        bound_logical_symbol_id(&conn),
        portable_symbol_id,
        "the bound memory survives the upgrade, still resolving to the same symbol",
    );
    let _ = fs::remove_dir_all(&repo);
}

/// A real git repo (two empty commits) for the upgrade-proof tests — its HEAD is a commit a genuine
/// deepened clone would reach.
fn real_git_repo(tag: &str) -> PathBuf {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["commit", "-q", "--allow-empty", "-m", &format!("{tag}-one")]);
    run_git(&root, &["commit", "-q", "--allow-empty", "-m", &format!("{tag}-two")]);
    root
}

/// The full HEAD commit hash — a commit reachable from HEAD, used as a recorded shallow boundary.
fn head_commit_hash(root: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git rev-parse HEAD failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// PROOF is REQUIRED, not just a `local:` prefix (round-6 P2 #4): a DB first indexed from a cut
/// shallow clone of repo X, later opened from an UNRELATED full repo Y at the same DB path, must be
/// REFUSED — Y's HEAD reaches NONE of X's boundary commits, so re-pointing the index onto Y's id
/// would migrate one repo's data onto another. The refusal names the pin escape hatch.
#[test]
fn register_repo_refuses_a_portable_upgrade_from_an_unrelated_repo() {
    let repo_x = real_git_repo("origin-x");
    let x_boundary = head_commit_hash(&repo_x);
    let repo_y = real_git_repo("unrelated-y"); // an independent root — no shared history with X.

    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    let local_id = "local:beefbeefcafe";
    register_repo(
        &conn,
        &identity_local(local_id, "shallow", vec![x_boundary]),
        repo_x.as_path(),
        1,
    )
    .expect("the shallow clone of X adopts under its local id, recording X's boundary");

    let err = register_repo(&conn, &identity("y-portable-root", "y"), repo_y.as_path(), 2)
        .expect_err(
            "an unrelated repo's HEAD does not reach X's boundary → the upgrade is refused",
        );
    let msg = err.to_string();
    assert!(msg.contains("could not prove"), "refusal explains the missing proof: {msg}");
    assert!(msg.contains("repo_id"), "refusal names the pin escape hatch: {msg}");
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), local_id, "the incumbent is untouched");
    assert_eq!(repo_row_count(&conn, "y-portable-root"), 0, "the unrelated id never landed");
    let _ = fs::remove_dir_all(&repo_x);
    let _ = fs::remove_dir_all(&repo_y);
}

/// No recorded shallow boundary ⇒ no proof available ⇒ refuse. A `local:` incumbent registered
/// before the proof gate (or with an unknown boundary) cannot be upgraded on faith, even by a
/// genuine deepened clone; the actionable error points at the `[index] repo_id` pin to force it.
#[test]
fn register_repo_refuses_a_local_upgrade_without_a_recorded_boundary() {
    let repo = real_git_repo("no-boundary");
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    // Register the local incumbent with an EMPTY boundary — nothing to prove against.
    let local_id = "local:nobound00";
    register_repo(&conn, &identity_local(local_id, "shallow", vec![]), repo.as_path(), 1).expect(
        "a LocalOnly registration succeeds even without a boundary — the gate is at UPGRADE",
    );

    let err = register_repo(&conn, &identity("would-be-portable", "p"), repo.as_path(), 2)
        .expect_err("no recorded boundary ⇒ no proof ⇒ the upgrade is refused");
    assert!(err.to_string().contains("repo_id"), "refusal names the pin remedy: {err}");
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), local_id, "the incumbent is untouched");
    let _ = fs::remove_dir_all(&repo);
}

/// The upgrade is NARROW: only a `local:` incumbent upgrades. Two genuinely-different PORTABLE
/// repos still trip the single-repo refusal — a Portable incoming must never silently re-point a
/// Portable incumbent (that would be data loss across two real repos).
#[test]
fn register_repo_refuses_a_portable_incoming_against_a_portable_incumbent() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    register_repo(&conn, &identity("portable-a", "a"), Path::new("/src/a"), 1).unwrap();

    let err = register_repo(&conn, &identity("portable-b", "b"), Path::new("/src/b"), 2)
        .expect_err("a second portable repo is refused — only a local: incumbent upgrades");
    assert!(err.to_string().contains("portable-a"), "refusal names the incumbent: {err}");
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), "portable-a", "incumbent untouched");
    assert_eq!(repo_row_count(&conn, "portable-b"), 0, "the intruder never landed");
}

/// A `LocalOnly` incoming against an existing REAL repo is refused — a deepened clone (or a second
/// shallow clone) must never DOWNGRADE a portable id back to machine-local. The upgrade only runs
/// in the Portable direction.
#[test]
fn register_repo_refuses_a_local_only_incoming_against_a_real_incumbent() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    register_repo(&conn, &identity("portable-a", "a"), Path::new("/src/a"), 1).unwrap();

    let err = register_repo(
        &conn,
        &identity_local("local:beef", "shallow", vec![]),
        Path::new("/src/s"),
        2,
    )
    .expect_err("a LocalOnly incoming must not downgrade a portable incumbent");
    assert!(err.to_string().contains("portable-a"), "refusal names the incumbent: {err}");
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), "portable-a", "portable id stays");
    assert_eq!(repo_row_count(&conn, "local:beef"), 0, "the local id never landed");
}

/// End-to-end: a real cut shallow clone is indexed under a `local:` id, then `git fetch
/// --unshallow` makes the portable root reachable, and re-opening UPGRADES the existing index in
/// place — the registered id flips to the portable root hash, the `local:` id is gone, and a memory
/// bound to an indexed symbol still resolves. This is the exact path the LocalOnly warning tells
/// the user to take; it must not strand their index.
#[test]
fn unshallow_upgrades_a_shallow_clone_index_from_local_to_portable_in_place() {
    let base = unique_temp_root();
    let _ = fs::remove_dir_all(&base);
    let origin = base.join("origin");
    fs::create_dir_all(origin.join("src")).unwrap();
    fs::write(origin.join("src/lib.rs"), "pub fn shallow_anchor() {}\n").unwrap();
    run_git(&origin, &["init", "-q", "-b", "main"]);
    run_git(&origin, &["config", "user.email", "t@e"]);
    run_git(&origin, &["config", "user.name", "t"]);
    run_git(&origin, &["add", "."]);
    run_git(&origin, &["commit", "-q", "-m", "one"]);
    run_git(&origin, &["commit", "-q", "--allow-empty", "-m", "two"]);
    // The portable id the deepened clone must resolve to: origin has full history, so its identity
    // is the (Portable) root-commit hash — the exact id `git fetch --unshallow` makes reachable
    // again.
    let origin_root = crate::repo_identity::resolve_repo_identity(&origin, None).unwrap().repo_id;

    // --depth 1 < history: a genuinely CUT shallow clone (root unreachable → LocalOnly id).
    let url = format!("file://{}", origin.display());
    run_git(&base, &["clone", "-q", "--depth", "1", &url, "clone"]);
    let clone_root = base.join("clone");

    let config = source_config(clone_root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).expect("index the shallow clone under a LocalOnly id");
    let local_id = db.active_repo_id.clone();
    assert!(
        local_id.starts_with("local:"),
        "shallow clone indexes under a local: id, got {local_id}"
    );

    // Bind a memory to an indexed logical symbol so we can prove it survives the id realign.
    let symbol_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols LIMIT 1", [], |r| r.get(0))
        .expect("the shallow clone indexed at least one logical symbol");
    let symbol_name: String = db
        .storage
        .connection()
        .query_row("SELECT logical_name FROM logical_symbols WHERE id = ?1", [symbol_id], |r| {
            r.get(0)
        })
        .unwrap();
    // A real memory row + a binding to the indexed symbol (the FK + NOT NULL columns the actual
    // schema carries). The upgrade's realign must re-point this binding as `logical_symbols.id`
    // changes under the portable fold.
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version) VALUES ('mem-upgrade', 'Invariant', 't', 'b', \
             'high', 'active', 0, 0, 'agent', 'v1')",
            [],
        )
        .unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, \
             logical_symbol_id, anchor_status, created_at_ms) VALUES ('mem-upgrade', \
             'logical_symbol', 'b1', ?1, 'current', 0)",
            [symbol_id],
        )
        .unwrap();
    drop(db);

    // Deepen the clone: the real root becomes reachable, so identity is now Portable (the root
    // hash).
    run_git(&clone_root, &["fetch", "-q", "--unshallow"]);

    // Re-open the EXISTING index (not a rebuild): register_repo detects local: incumbent + Portable
    // incoming → upgrade in place.
    let reopened = IndexDatabase::open_config(&config)
        .expect("re-opening a deepened clone upgrades the index, it does not refuse");
    assert_eq!(reopened.active_repo_id, origin_root, "upgraded to the portable root-commit id");
    assert!(!reopened.active_repo_id.starts_with("local:"), "no longer a machine-local id");
    assert_eq!(
        repo_row_count(reopened.storage.connection(), &local_id),
        0,
        "the machine-local repos row is gone after the upgrade",
    );
    // The bound memory survived: its (realigned) logical_symbol_id still resolves to the same
    // symbol.
    let resolved_name: String = reopened
        .storage
        .connection()
        .query_row(
            "SELECT ls.logical_name FROM repo_memory_bindings b
               JOIN logical_symbols ls ON ls.id = b.logical_symbol_id
              WHERE b.memory_id = 'mem-upgrade'",
            [],
            |r| r.get(0),
        )
        .expect("the memory binding still resolves after the in-place upgrade");
    assert_eq!(resolved_name, symbol_name, "the memory resolves to the same symbol post-upgrade");
    let _ = fs::remove_dir_all(base);
}

// --- Read-path repo resolution without registering (#413 round-4 findings #1 + #2) ---

/// `resolve_config_repo_id` (the read-path resolver behind the read-only open + the raw scope-view
/// hooks) binds the repo a config's ROOT is recorded under, NOT the config-blind sole repo. In a
/// consolidated DB the sole pick could be a sibling; the recorded-root route keeps a read scoped to
/// the config's own repo even for a non-git root that has no derivable identity.
#[test]
fn resolve_config_repo_id_binds_a_recorded_root_over_the_sole_pick() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    // Repo A adopted (sorts first). A sibling repo B seeded directly with its recorded root — the
    // A7 consolidated shape (register_repo forbids a second real repo before A7).
    register_repo(&conn, &identity("repo-a", "a"), Path::new("/src/a"), 1).unwrap();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('sibling-b', 'b', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_roots(repo_id, root, registered_at_ms) VALUES ('sibling-b', ?1, 0)",
        [Path::new("/src/b").to_string_lossy().as_ref()],
    )
    .unwrap();

    // The config-blind sole pick is repo-a (lexicographically smallest) — the WRONG repo for
    // /src/b.
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), "repo-a");
    // The resolver binds the repo the ROOT is recorded under (a non-git root → the by-root route).
    assert_eq!(
        schema::resolve_config_repo_id(&conn, Path::new("/src/b"), None).unwrap().as_deref(),
        Some("sibling-b"),
        "a recorded root binds its own repo, not the smaller sole pick",
    );
    // A single-repo DB with an unrecorded, non-git root still falls back to the sole repo
    // (preserves the pre-A3 read fast path) — asserted here by an unknown root resolving to
    // None on this CONSOLIDATED DB (>1 real repo → cannot prove, bind nothing rather than a
    // sibling).
    assert_eq!(
        schema::resolve_config_repo_id(&conn, Path::new("/src/unknown"), None).unwrap(),
        None,
        "an unprovable root on a consolidated DB resolves to None (never a sibling)",
    );
}

/// A `Rejected` config (a reserved `[index] repo_id` pin) resolves to `None` on the read path — it
/// must NOT silently bind a repo; the read-write open surfaces the actionable error instead.
#[test]
fn resolve_config_repo_id_returns_none_for_a_rejected_pin() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["commit", "-q", "--allow-empty", "-m", "genesis"]);

    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    register_repo(&conn, &identity("repo-a", "a"), root.as_path(), 1).unwrap();

    // A reserved pin is the Rejected class → None, even though the root is a real registered repo.
    assert_eq!(
        schema::resolve_config_repo_id(&conn, &root, Some(LEGACY_REPO_ID)).unwrap(),
        None,
        "a reserved-id pin does not resolve on the read path — it surfaces via the read-write open",
    );
    let _ = fs::remove_dir_all(root);
}

/// #413 round-5: a NEW, unregistered `[index] repo_id` pin resolves to `None` on the read path —
/// NOT to the repo the root was previously registered under. A changed identity must adopt/surface
/// on the read-write open; the read-only path declining is what forces that, instead of silently
/// serving the old scope under the new pin.
#[test]
fn resolve_config_repo_id_returns_none_for_a_new_unregistered_pin() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["commit", "-q", "--allow-empty", "-m", "genesis"]);

    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    // The root is registered (and recorded) under repo-a — so the pre-fix by-root fallback would
    // resolve the new pin to repo-a.
    register_repo(&conn, &identity("repo-a", "a"), root.as_path(), 1).unwrap();

    assert_eq!(
        schema::resolve_config_repo_id(&conn, &root, Some("brand-new-pin")).unwrap(),
        None,
        "a new unregistered pin declines on the read path — it does not bind the old (repo-a) \
         scope",
    );
    let _ = fs::remove_dir_all(root);
}

/// #413 round-5, the shallow-upgrade sibling case: a repo registered under a `local:` id whose
/// full-history root now derives a DIFFERENT (portable) id. `resolve_config_repo_id` with no pin
/// derives the unregistered portable id and must return `None` — the LocalOnly→Portable upgrade
/// belongs on the read-write path (`register_repo`), not a silent read-path rebind to the old
/// `local:` scope.
#[test]
fn resolve_config_repo_id_returns_none_for_a_newly_portable_local_incumbent() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "one"]);

    // The portable id the full-history root derives (what a deepened clone would resolve to).
    let portable_id = crate::repo_identity::resolve_repo_identity(&root, None).unwrap().repo_id;
    assert!(!portable_id.starts_with("local:"), "full history → a portable root id");

    let conn = rusqlite::Connection::open_in_memory().expect("open");
    schema::apply(&conn).expect("apply");
    // Incumbent: registered (and root recorded) under a machine-local id, as a prior shallow index.
    register_repo(&conn, &identity_local("local:beef", "shallow", vec![]), root.as_path(), 1)
        .unwrap();
    assert!(!repo_id_is_registered_probe(&conn, &portable_id), "portable id is not yet registered");

    // No pin: the identity route derives the portable id (unregistered) → None. The pre-fix by-root
    // fallback would rebind to the incumbent `local:beef`.
    assert_eq!(
        schema::resolve_config_repo_id(&conn, &root, None).unwrap(),
        None,
        "a newly-portable identity declines on the read path — upgrade happens on the write path",
    );
    let _ = fs::remove_dir_all(root);
}

/// Test-local probe: is `repo_id` a registered real repo? (mirrors the private
/// `repo_id_is_registered`, kept here so the test above needn't expose it.)
fn repo_id_is_registered_probe(conn: &rusqlite::Connection, repo_id: &str) -> bool {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM repos WHERE repo_id = ?1)", [repo_id], |r| r.get(0))
        .unwrap()
}

// --- Identity-resolution error classes at the open_config boundary (A3) ---

/// A pinned RESERVED `[index] repo_id` must SURFACE from `open_config` — the `Rejected` class of
/// `RepoIdentityError`. The old blanket fallback silently scoped the DB to the placeholder, hiding
/// the configuration problem and leaving every row unadopted under the legacy id.
#[test]
fn open_config_surfaces_a_reserved_repo_id_pin() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let mut config = source_config(root.clone(), Language::Rust);
    config.repo_id_override = Some(LEGACY_REPO_ID.to_string());
    // `open_config` opens an EXISTING index (a Missing schema refuses); create it first, exactly
    // like the `rag-rat index` → later plain opens sequence.
    IndexDatabase::migrate(&config.database).unwrap();

    let err = IndexDatabase::open_config(&config)
        .expect_err("a reserved-id pin is a rejection, never a silent placeholder fallback");
    assert!(err.to_string().contains("reserved"), "error names the rejection: {err}");
    let _ = fs::remove_dir_all(root);
}

/// A cut shallow clone (its root commit unreachable, so a derived id would be depth-dependent) does
/// NOT fail through `open_config`: it adopts under a deterministic `local:`-prefixed LocalOnly id
/// and proceeds. Blocking a `--depth 1` checkout would break CI fixtures for no benefit — the id is
/// stable on this machine, only not portable across machines (the sync layer enforces that later).
#[test]
fn open_config_adopts_a_shallow_clone_under_a_local_only_id() {
    let base = unique_temp_root();
    let _ = fs::remove_dir_all(&base);
    let origin = base.join("origin");
    fs::create_dir_all(origin.join("src")).unwrap();
    fs::write(origin.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    run_git(&origin, &["init", "-q", "-b", "main"]);
    run_git(&origin, &["config", "user.email", "t@e"]);
    run_git(&origin, &["config", "user.name", "t"]);
    run_git(&origin, &["add", "."]);
    run_git(&origin, &["commit", "-q", "-m", "one"]);
    run_git(&origin, &["commit", "-q", "--allow-empty", "-m", "two"]);
    // --depth 1 < history: the clone's root commit is unreachable (a genuinely CUT shallow clone).
    let url = format!("file://{}", origin.display());
    run_git(&base, &["clone", "-q", "--depth", "1", &url, "clone"]);
    let clone_root = base.join("clone");

    let config = source_config(clone_root, Language::Rust);
    IndexDatabase::migrate(&config.database).unwrap();
    let db = IndexDatabase::open_config(&config)
        .expect("a cut shallow clone adopts under a LocalOnly id, it does not fail");
    assert!(
        db.active_repo_id.starts_with("local:"),
        "a cut shallow clone adopts under a LocalOnly id, got {}",
        db.active_repo_id
    );
    // Adopted as a real repo: the placeholder is gone and the LocalOnly id owns the registry.
    assert_eq!(repo_row_count(db.storage.connection(), LEGACY_REPO_ID), 0, "placeholder adopted");
    assert_eq!(
        repo_row_count(db.storage.connection(), &db.active_repo_id),
        1,
        "LocalOnly repo row"
    );
    let _ = fs::remove_dir_all(base);
}

/// A NON-git root (no identity to derive at all) is the EXPECTED-absence class: `open_config`
/// still opens, scoped to the sole repo of the single-repo DB (the placeholder on a fresh one) —
/// the pre-A3 behavior every bare temp-dir index relies on.
#[test]
fn open_config_falls_back_to_the_sole_repo_on_a_non_git_root() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let config = source_config(root.clone(), Language::Rust);
    IndexDatabase::migrate(&config.database).unwrap();

    let db = IndexDatabase::open_config(&config)
        .expect("expected absence (not a git repo) falls back, it does not error");
    assert_eq!(
        db.active_repo_id, LEGACY_REPO_ID,
        "the un-adopted single-repo DB scopes to the placeholder"
    );
    let _ = fs::remove_dir_all(root);
}

// --- V041: repo_id scoping on the GitHub papertrail tables (memory-sync phase A4) ---

/// The pre-V041 shape of the seven GitHub tables + `github_fts` (no `repo_id`) plus the V038
/// registry, built in ISOLATION so [`schema::apply_github_repo_id_scoping`] is exercised against
/// its own inputs (the directory's "assert deferred absence / rebuild behavior in isolation" rule).
fn seed_pre_v041_github_schema(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "
        CREATE TABLE github_refs(
            id INTEGER PRIMARY KEY AUTOINCREMENT, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, ref_kind TEXT NOT NULL DEFAULT 'unknown',
            source_kind TEXT NOT NULL, source_path TEXT, source_commit TEXT,
            source_text TEXT NOT NULL, discovered_at_ms INTEGER NOT NULL);
        CREATE TABLE github_issues(
            id INTEGER PRIMARY KEY AUTOINCREMENT, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, html_url TEXT NOT NULL, state TEXT NOT NULL, title TEXT NOT \
         NULL,
            body TEXT NOT NULL, author TEXT, created_at TEXT, updated_at TEXT,
            is_pull_request INTEGER NOT NULL DEFAULT 0, synced_at_ms INTEGER NOT NULL,
            UNIQUE(owner, repo, number));
        CREATE TABLE github_comments(
            id INTEGER PRIMARY KEY, owner TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT \
         NULL,
            html_url TEXT NOT NULL, body TEXT NOT NULL, author TEXT, created_at TEXT, updated_at \
         TEXT,
            synced_at_ms INTEGER NOT NULL);
        CREATE TABLE github_pull_requests(
            id INTEGER PRIMARY KEY AUTOINCREMENT, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, html_url TEXT NOT NULL, state TEXT NOT NULL, title TEXT NOT \
         NULL,
            body TEXT NOT NULL, author TEXT, created_at TEXT, updated_at TEXT, merged_at TEXT,
            synced_at_ms INTEGER NOT NULL, UNIQUE(owner, repo, number));
        CREATE TABLE github_reviews(
            id INTEGER PRIMARY KEY, owner TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT \
         NULL,
            html_url TEXT, state TEXT NOT NULL, body TEXT NOT NULL, author TEXT, submitted_at TEXT,
            synced_at_ms INTEGER NOT NULL);
        CREATE TABLE github_review_comments(
            id INTEGER PRIMARY KEY, owner TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT \
         NULL,
            path TEXT, html_url TEXT NOT NULL, body TEXT NOT NULL, author TEXT, created_at TEXT,
            updated_at TEXT, synced_at_ms INTEGER NOT NULL);
        CREATE TABLE github_ref_sync(
            owner TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT NULL, status TEXT NOT NULL,
            synced_at_ms INTEGER NOT NULL, last_error TEXT, PRIMARY KEY(owner, repo, number));
        CREATE VIRTUAL TABLE github_fts USING fts5(
            owner, repo, number UNINDEXED, item_kind UNINDEXED, item_id UNINDEXED, url UNINDEXED,
            title, body, classification, tokenize='porter');
        ",
    )
    .unwrap();
    schema::apply_repos_registry(conn).expect("V038 registry seeds the placeholder");
}

/// Fresh `apply` runs V041: the seven GitHub tables and `github_fts` gain a direct `repo_id`. (The
/// absolute `LATEST_SCHEMA_VERSION` pin moved to `migration_042_*`, the new tip; this uses only the
/// symbolic `current_version == LATEST` check.)
#[test]
fn migration_041_scopes_github() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    for table in [
        "github_refs",
        "github_issues",
        "github_comments",
        "github_pull_requests",
        "github_reviews",
        "github_review_comments",
        "github_ref_sync",
        "github_fts",
    ] {
        assert!(
            conn_table_columns(&conn, table).contains(&"repo_id".to_string()),
            "{table} gains a direct repo_id column"
        );
    }
}

/// V041's `github_fts` REBUILD is driven against the pre-V041 fixture IN ISOLATION: the base tables
/// gain `repo_id`, the FTS row survives the rebuild (backfilled to the placeholder) and still
/// MATCHes, and the migration RE-CONVERGES from a torn intermediate (a leftover `github_fts_new`
/// scratch table from a crashed prior pass). Then `register_repo` adoption re-points every
/// placeholder row — the base tables AND the derived FTS mirror.
#[test]
fn migration_041_github_rebuild_preserves_rows_and_reconverges_from_torn_state() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v041_github_schema(&conn);
    // A synced issue + its FTS row, as a pre-V041 index carries them.
    conn.execute(
        "INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, synced_at_ms)
         VALUES ('o', 'r', 7, 'http://i', 'open', 'zebra title', 'zebra body', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification)
         VALUES ('o', 'r', 7, 'issue', '1', 'http://i', 'zebra title', 'zebra body', 'other')",
        [],
    )
    .unwrap();

    // TORN STATE: a prior V041 pass crashed after creating the scratch FTS table. The rebuild must
    // drop it and re-converge rather than fail on CREATE.
    conn.execute_batch("CREATE TABLE github_fts_new(bogus INTEGER);").unwrap();

    schema::apply_github_repo_id_scoping(&conn).expect("V041 converges from the torn state");

    assert!(!conn_table_exists(&conn, "github_fts_new"), "scratch table gone");
    assert!(
        conn_table_columns(&conn, "github_fts").contains(&"repo_id".to_string()),
        "github_fts rebuilt with repo_id"
    );
    assert!(
        conn_table_columns(&conn, "github_issues").contains(&"repo_id".to_string()),
        "github_issues gained repo_id"
    );

    // The FTS row survived, backfilled to the placeholder, and still MATCHes.
    let (repo_id, matched): (String, i64) = conn
        .query_row(
            "SELECT repo_id, (SELECT COUNT(*) FROM github_fts WHERE github_fts MATCH 'zebra')
             FROM github_fts WHERE github_fts MATCH 'zebra'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(repo_id, LEGACY_REPO_ID, "existing FTS rows backfill to the placeholder");
    assert_eq!(matched, 1, "github_fts still MATCHes after the rebuild");

    // Idempotent re-run (the sentinel short-circuits once github_fts carries repo_id).
    schema::apply_github_repo_id_scoping(&conn).expect("re-apply is a clean no-op");
}

/// Full-schema adoption of the V041 GitHub papertrail: `register_repo` re-points the placeholder
/// github rows (a base table AND the `github_fts` mirror) onto the real id. Kept SEPARATE from the
/// `migration_041_github_rebuild_*` isolation test above because adoption now runs
/// `realign_logical_symbol_ids`, which needs the full core schema the github-only isolation fixture
/// omits (it would trip `no such table: logical_symbols`). The full ladder gives adoption every
/// table it touches.
#[test]
fn register_repo_repoints_github_papertrail_rows_to_the_real_id() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    schema::apply(&conn).unwrap();
    // A synced issue + its FTS mirror seeded under the placeholder, as a pre-adoption index carries
    // them (both explicitly stamped LEGACY_REPO_ID so adoption's placeholder re-point matches).
    conn.execute(
        "INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, \
         synced_at_ms, repo_id)
         VALUES ('o', 'r', 7, 'http://i', 'open', 'zebra title', 'zebra body', 0, ?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
         VALUES ('o', 'r', 7, 'issue', '1', 'http://i', 'zebra title', 'zebra body', 'other', ?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();

    register_repo(&conn, &identity("repo-real", "r"), Path::new("/src/r"), 1).unwrap();

    let issue_repo: String =
        conn.query_row("SELECT repo_id FROM github_issues", [], |r| r.get(0)).unwrap();
    assert_eq!(issue_repo, "repo-real", "adoption re-points github_issues");
    let fts_repo: String = conn
        .query_row("SELECT repo_id FROM github_fts WHERE github_fts MATCH 'zebra'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fts_repo, "repo-real", "adoption re-points the github_fts mirror in place");
}

/// P1 backfill (the V040 class): applying V041 on an ALREADY-ADOPTED DB (a real `repos` row, the
/// placeholder gone) must re-point the existing github rows — base tables AND the `github_fts`
/// mirror — onto the real id via `sole_repo_id`, NOT strand them under the static
/// `'__unassigned__'` column default where a scoped papertrail read would never see them until the
/// next sync.
#[test]
fn migration_041_backfills_an_adopted_pre_v041_db_under_the_real_repo_id() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v041_github_schema(&conn);
    conn.execute(
        "INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, synced_at_ms)
         VALUES ('o', 'r', 7, 'http://i', 'open', 't', 'b', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO github_refs(owner, repo, number, source_kind, source_text, discovered_at_ms)
         VALUES ('o', 'r', 7, 'file', 'reftext', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification)
         VALUES ('o', 'r', 7, 'issue', '1', 'http://i', 't', 'b', 'other')",
        [],
    )
    .unwrap();
    // Adopt as a pre-V041 binary's `register_repo` left it: a real `repos` row, placeholder gone.
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-adopted', 'r', \
         1)",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM repos WHERE repo_id = ?1", [LEGACY_REPO_ID]).unwrap();

    schema::apply_github_repo_id_scoping(&conn).expect("V041 applies on an adopted DB");

    for table in ["github_refs", "github_issues", "github_fts"] {
        let (total, under_real): (i64, i64) = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM(repo_id = 'repo-adopted'), 0) FROM {table}"
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(total > 0, "{table}: the fixture seeded at least one row");
        assert_eq!(under_real, total, "{table}: every row backfilled under the REAL repo id");
    }
    let stranded: i64 = conn
        .query_row("SELECT COUNT(*) FROM github_issues WHERE repo_id = ?1", [LEGACY_REPO_ID], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(stranded, 0, "nothing remains stranded under the placeholder");
}

/// V041 on a CONSOLIDATED (multi-repo) V040-level DB: there is no single owner for the backfill to
/// re-point onto, so the migration must SUCCEED — not abort on `sole_repo_id`'s one-row hard error
/// — and leave the github rows under the placeholder. That is safe: the papertrail is a
/// refetchable cache, every scoped reader filters `repo_id = <active>` (placeholder rows are
/// invisible, never misattributed), and each repo's next github sync re-populates its slice under
/// the proper stamp.
#[test]
fn migration_041_leaves_github_rows_at_the_placeholder_on_a_consolidated_db() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v041_github_schema(&conn);
    conn.execute(
        "INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, synced_at_ms)
         VALUES ('o', 'r', 7, 'http://i', 'open', 't', 'b', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO github_refs(owner, repo, number, source_kind, source_text, discovered_at_ms)
         VALUES ('o', 'r', 7, 'file', 'reftext', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification)
         VALUES ('o', 'r', 7, 'issue', '1', 'http://i', 't', 'b', 'other')",
        [],
    )
    .unwrap();
    // The consolidated end-state: TWO real repos, placeholder row gone.
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-a', 'a', 1), \
         ('repo-b', 'b', 2)",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM repos WHERE repo_id = ?1", [LEGACY_REPO_ID]).unwrap();

    schema::apply_github_repo_id_scoping(&conn)
        .expect("V041 must not abort the upgrade on a consolidated DB");

    for table in ["github_refs", "github_issues", "github_fts"] {
        let (total, under_placeholder): (i64, i64) = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM(repo_id = '{LEGACY_REPO_ID}'), 0) FROM {table}"
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(total > 0, "{table}: the fixture seeded at least one row");
        assert_eq!(
            under_placeholder, total,
            "{table}: every row stays under the placeholder — no arbitrary owner is picked"
        );
        // The shape every V041 reader uses: a scoped read filters `repo_id = <active>`, so the
        // placeholder rows are invisible to BOTH repos.
        for repo in ["repo-a", "repo-b"] {
            let visible: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE repo_id = ?1"),
                    [repo],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(visible, 0, "{table}: placeholder rows must be invisible to {repo}");
        }
    }
}

/// The reclamation half of the consolidated-DB gate: the github tables keep natural keys WITHOUT
/// `repo_id`, so a placeholder-stranded row OCCUPIES its key — a conflict-ignoring writer could
/// never repopulate it and the cache would be stranded forever. The writers upsert-reclaim
/// instead: the next sync that touches a stranded key re-stamps `repo_id`, refreshes the content,
/// and the sync-tail `rebuild_fts` re-derives the mirror. Rows the sync does NOT touch stay under
/// the placeholder (they wait for their own repo's sync).
#[test]
fn v041_placeholder_github_rows_are_reclaimed_by_the_next_sync() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // The state the consolidated-DB gate leaves: github rows stranded under the placeholder on a
    // two-real-repo DB.
    conn.execute_batch(&format!(
        "INSERT INTO github_refs(owner, repo, number, ref_kind, source_kind, source_path, \
         source_commit, source_text, discovered_at_ms, repo_id)
         VALUES ('o', 'r', 7, 'unknown', 'manual', NULL, NULL, 'o/r#7', 1, '{p}');
         INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, \
         synced_at_ms, repo_id)
         VALUES ('o', 'r', 7, 'http://stale', 'open', 'stale title', 'stale body', 1, '{p}');
         INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, \
         synced_at_ms, repo_id)
         VALUES ('o', 'r', 8, 'http://other', 'open', 'other stale', 'other body', 1, '{p}');
         INSERT INTO github_ref_sync(owner, repo, number, status, synced_at_ms, repo_id)
         VALUES ('o', 'r', 7, 'synced', 1, '{p}');
         INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-a', 'a', 1), \
         ('repo-b', 'b', 2);
         DELETE FROM repos WHERE repo_id = '{p}';",
        p = LEGACY_REPO_ID
    ))
    .unwrap();
    // Mirror state before any reclaim: derived from the stranded base rows.
    crate::index::github::rebuild_fts(&conn).unwrap();

    // Pin the connection's active repo to repo-a (what `set_context` installs on a real open).
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);
         INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', 'repo-a');",
    )
    .unwrap();

    // repo-a's sync touches o/r#7: re-discovers the ref (same natural key as the stranded row),
    // refetches the issue with fresh content, and marks the sync cursor.
    let reference = crate::index::github::GitHubRef {
        owner: "o".to_string(),
        repo: "r".to_string(),
        number: 7,
        ref_kind: "unknown".to_string(),
        source_kind: "manual".to_string(),
        source_path: None,
        source_commit: None,
        source_text: "o/r#7".to_string(),
    };
    crate::index::github::store_ref(&conn, &reference).unwrap();
    crate::index::github::store_issue(&conn, &crate::index::github::GitHubIssue {
        owner: "o".to_string(),
        repo: "r".to_string(),
        number: 7,
        html_url: "http://fresh".to_string(),
        state: "closed".to_string(),
        title: "fresh title".to_string(),
        body: "fresh body".to_string(),
        author: None,
        created_at: None,
        updated_at: None,
        is_pull_request: false,
    })
    .unwrap();
    crate::index::github::mark_ref_sync(&conn, &reference, "synced", None).unwrap();
    // The sync tail: the whole-table mirror rebuild follows the reclaimed base rows.
    crate::index::github::rebuild_fts(&conn).unwrap();

    // The touched rows are re-stamped to repo-a with refreshed content…
    let (issue_repo, issue_title): (String, String) = conn
        .query_row("SELECT repo_id, title FROM github_issues WHERE number = 7", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(issue_repo, "repo-a", "the stranded issue row is reclaimed by the syncing repo");
    assert_eq!(issue_title, "fresh title", "reclaim refreshes the content, not just the stamp");
    for table in ["github_refs", "github_ref_sync"] {
        let repo: String = conn
            .query_row(&format!("SELECT repo_id FROM {table} WHERE number = 7"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(repo, "repo-a", "{table}: stranded row reclaimed");
    }
    // …the FTS mirror is consistent with the reclaimed base rows…
    let fts_repo: String = conn
        .query_row("SELECT repo_id FROM github_fts WHERE number = 7", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fts_repo, "repo-a", "the mirror row follows the reclaimed base row");
    // …a scoped read now sees the papertrail…
    let visible: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM github_issues WHERE repo_id = 'repo-a' AND number = 7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(visible, 1, "the reclaimed row is visible to repo-a's scoped reads");
    // …and the row the sync did NOT touch stays under the placeholder for its own repo's sync.
    for table in ["github_issues", "github_fts"] {
        let repo: String = conn
            .query_row(&format!("SELECT repo_id FROM {table} WHERE number = 8"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(repo, LEGACY_REPO_ID, "{table}: untouched stranded row stays placeholder");
    }
}

/// A V040 index forward-migrates to V041 on `migrate_forward` — reaching LATEST.
#[test]
fn migration_041_forward_migrates_a_v040_index() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    truncate_schema_to(&conn, 40);
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);
    schema::migrate_forward(&conn).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
}

/// The eleven periphery tables A5 scopes — the direct-column set plus the standalone FTS mirror.
const V042_PERIPHERY_TABLES: &[&str] = &[
    "clone_graph_generations",
    "clone_token_df",
    "clone_refinements",
    "oracle_runs",
    "edge_oracle",
    "logical_symbol_monikers",
    "reconcile_attempts",
    "dream_findings",
    "repo_memories",
    "repo_memory_bindings",
    "repo_memory_fts",
];

/// Whether `table`'s PRIMARY KEY includes a `repo_id` column (the rebuilt-PK set).
fn pk_includes_repo_id(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = 'repo_id' AND pk > 0"
        ),
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

/// The pre-V042 shape of the clone / oracle / reconcile / memory periphery tables (NO `repo_id`),
/// built by calling the real table-creating DDL fns in ISOLATION — so
/// [`schema::apply_repo_id_periphery_scoping`] runs against its own genuine inputs (the directory's
/// "assert deferred absence / rebuild behavior in isolation, not the full ladder" rule). Calling
/// the real DDL fns (not hand-copied `CREATE TABLE`s) keeps the fixture drift-free as those tables
/// evolve. The core / GitHub tables are deliberately NOT scoped here (V040 / V041 are not run):
/// this fixture drives only V042, so [`register_repo`] adoption is exercised in the full-ladder
/// test below, where every direct-scoped column exists.
fn seed_pre_v042_periphery_schema(conn: &rusqlite::Connection) {
    schema::apply_baseline(conn)
        .expect("baseline: repo_memories/bindings/tags/fts + reconcile_attempts + core tables");
    schema::apply_oracle_tables(conn).expect("oracle_runs + edge_oracle");
    schema::apply_scip_moniker_anchors(conn).expect("logical_symbol_monikers");
    schema::apply_clone_fingerprint_tables(conn).expect("clone_token_df + clone_refinements");
    schema::apply_clone_graph_tables(conn).expect("clone_graph_generations");
    schema::apply_dream_findings(conn).expect("dream_findings");
    schema::apply_repos_registry(conn).expect("V038 registry seeds the placeholder");
}

/// Fresh `apply` runs V042 (the schema tip): every periphery table gains a direct `repo_id` and the
/// content-keyed tables rebuild their PK to lead with it. Owns the absolute `LATEST_SCHEMA_VERSION`
/// pin.
#[test]
fn migration_042_is_the_latest_tip_and_scopes_periphery() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    assert_eq!(schema::LATEST_SCHEMA_VERSION, 42, "V042 is the schema tip");
    assert_eq!(schema::status(&conn).unwrap().current_version, 42, "schema at LATEST after apply");

    for table in V042_PERIPHERY_TABLES {
        assert!(
            conn_table_columns(&conn, table).contains(&"repo_id".to_string()),
            "{table} gains a direct repo_id column"
        );
    }
    // The content-keyed tables lead their PK with repo_id (df must not pool; class-keys, moniker
    // keys, and the edge-oracle content key collide across repos otherwise).
    for table in ["clone_token_df", "clone_refinements", "edge_oracle", "logical_symbol_monikers"] {
        assert!(pk_includes_repo_id(&conn, table), "{table} PK leads with repo_id");
    }
}

/// V042's rebuilds are driven against the pre-V042 fixture IN ISOLATION: the periphery tables gain
/// `repo_id`, a seeded memory's row survives into the rebuilt `repo_memory_fts` (backfilled to the
/// placeholder and still MATCHing), a seeded `clone_token_df` row survives its PK rebuild, and the
/// migration RE-CONVERGES from a torn intermediate (a leftover `clone_token_df_new` scratch from a
/// crashed prior pass). Then a re-run short-circuits on the sentinel. This is the retained
/// direct-fn isolation test (adoption is covered by the full-ladder test).
#[test]
fn migration_042_periphery_rebuild_preserves_rows_and_reconverges_from_torn_state() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v042_periphery_schema(&conn);

    // Deferred-absence in isolation: repo_id is absent on every periphery table before V042 runs.
    for table in V042_PERIPHERY_TABLES {
        assert!(
            !conn_table_columns(&conn, table).contains(&"repo_id".to_string()),
            "{table} must NOT carry repo_id before V042"
        );
    }

    // A memory (the FTS rebuild's source) + a df row (a rebuilt-PK table) as a pre-V042 index
    // holds.
    conn.execute(
        "INSERT INTO repo_memories(
             id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
             memory_version)
         VALUES ('m1', 'Invariant', 'zebra invariant', 'zebra body', 'high', 'active', 0, 0, \
         'manual', 'v1')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_token_df(normalizer_kind, token_hash, df) VALUES ('baseline', 42, 7)",
        [],
    )
    .unwrap();

    // TORN STATE: a prior V042 pass crashed after creating a rebuild scratch table.
    conn.execute_batch("CREATE TABLE clone_token_df_new(bogus INTEGER);").unwrap();

    schema::apply_repo_id_periphery_scoping(&conn).expect("V042 converges from the torn state");

    assert!(!conn_table_exists(&conn, "clone_token_df_new"), "scratch table swept");
    for table in V042_PERIPHERY_TABLES {
        assert!(
            conn_table_columns(&conn, table).contains(&"repo_id".to_string()),
            "{table} gained repo_id"
        );
    }
    assert!(pk_includes_repo_id(&conn, "clone_token_df"), "clone_token_df PK leads with repo_id");

    // The df row survived its PK rebuild, backfilled to the placeholder.
    let (df, df_repo): (i64, String) = conn
        .query_row("SELECT df, repo_id FROM clone_token_df WHERE token_hash = 42", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(df, 7, "df value preserved through the PK rebuild");
    assert_eq!(df_repo, LEGACY_REPO_ID, "existing df rows backfill to the placeholder");

    // The memory survived into the rebuilt FTS, backfilled to the placeholder and still MATCHing.
    let (fts_repo, matched): (String, i64) = conn
        .query_row(
            "SELECT repo_id, (SELECT COUNT(*) FROM repo_memory_fts WHERE repo_memory_fts MATCH \
             'zebra')
             FROM repo_memory_fts WHERE repo_memory_fts MATCH 'zebra'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(fts_repo, LEGACY_REPO_ID, "the rebuilt FTS row carries the placeholder repo_id");
    assert_eq!(matched, 1, "repo_memory_fts still MATCHes the seeded memory after the rebuild");

    // Idempotent re-run (the repo_memories.repo_id sentinel short-circuits).
    schema::apply_repo_id_periphery_scoping(&conn).expect("re-apply is a clean no-op");
}

/// P1 backfill (the V040 class): applying V042 on an ALREADY-ADOPTED DB (a real `repos` row, the
/// placeholder gone) must re-point the existing periphery rows — an additive-column table, a
/// PK-rebuilt table, AND the `repo_memory_fts` mirror — onto the real id via `sole_repo_id`, NOT
/// strand them under the static `'__unassigned__'` default where the scoped periphery reads would
/// never see them.
#[test]
fn migration_042_backfills_an_adopted_pre_v042_db_under_the_real_repo_id() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v042_periphery_schema(&conn);
    conn.execute(
        "INSERT INTO repo_memories(
             id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
             memory_version)
         VALUES ('m1', 'Invariant', 'zebra invariant', 'zebra body', 'high', 'active', 0, 0, \
         'manual', 'v1')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_token_df(normalizer_kind, token_hash, df) VALUES ('baseline', 42, 7)",
        [],
    )
    .unwrap();
    // Adopt as a pre-V042 binary's `register_repo` left it: a real `repos` row, placeholder gone.
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-adopted', 'r', \
         1)",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM repos WHERE repo_id = ?1", [LEGACY_REPO_ID]).unwrap();

    schema::apply_repo_id_periphery_scoping(&conn).expect("V042 applies on an adopted DB");

    let df_repo: String = conn
        .query_row("SELECT repo_id FROM clone_token_df WHERE token_hash = 42", [], |r| r.get(0))
        .unwrap();
    assert_eq!(df_repo, "repo-adopted", "clone_token_df backfilled under the real repo id");
    let mem_repo: String = conn
        .query_row("SELECT repo_id FROM repo_memories WHERE id = 'm1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mem_repo, "repo-adopted", "repo_memories backfilled under the real repo id");
    let fts_repo: String = conn
        .query_row(
            "SELECT repo_id FROM repo_memory_fts WHERE repo_memory_fts MATCH 'zebra'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        fts_repo, "repo-adopted",
        "repo_memory_fts mirror backfilled under the real repo id"
    );
    let stranded: i64 = conn
        .query_row("SELECT COUNT(*) FROM repo_memories WHERE repo_id = ?1", [LEGACY_REPO_ID], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(stranded, 0, "no periphery row remains under the placeholder");
}

/// V042 on a CONSOLIDATED (multi-repo) V041-level DB — the V041 github-backfill twin: there is no
/// single owner for the backfill to re-point onto, so the migration must SUCCEED (not abort on
/// `sole_repo_id`'s one-row hard error) and leave the periphery rows under the placeholder,
/// invisible to every scoped reader. Unlike the github cache, a placeholder-stranded
/// `repo_memories` row is user-authored data — `memory doctor` surfaces it as a
/// `placeholder_repo` entry rather than letting it vanish silently (covered by
/// `memory_doctor_surfaces_placeholder_scoped_memories` in repo_memory.rs).
#[test]
fn migration_042_leaves_periphery_rows_at_the_placeholder_on_a_consolidated_db() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v042_periphery_schema(&conn);
    conn.execute(
        "INSERT INTO repo_memories(
             id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
             memory_version)
         VALUES ('m1', 'Invariant', 'zebra invariant', 'zebra body', 'high', 'active', 0, 0, \
         'manual', 'v1')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_token_df(normalizer_kind, token_hash, df) VALUES ('baseline', 42, 7)",
        [],
    )
    .unwrap();
    // The consolidated end-state: TWO real repos, placeholder row gone.
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-a', 'a', 1), \
         ('repo-b', 'b', 2)",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM repos WHERE repo_id = ?1", [LEGACY_REPO_ID]).unwrap();

    schema::apply_repo_id_periphery_scoping(&conn)
        .expect("V042 must not abort the upgrade on a consolidated DB");

    for table in ["repo_memories", "clone_token_df", "repo_memory_fts"] {
        let (total, under_placeholder): (i64, i64) = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM(repo_id = '{LEGACY_REPO_ID}'), 0) FROM {table}"
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(total > 0, "{table}: the fixture seeded at least one row");
        assert_eq!(
            under_placeholder, total,
            "{table}: every row stays under the placeholder — no arbitrary owner is picked"
        );
        // The shape every scoped periphery reader uses: placeholder rows are invisible to BOTH
        // repos.
        for repo in ["repo-a", "repo-b"] {
            let visible: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE repo_id = ?1"),
                    [repo],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(visible, 0, "{table}: placeholder rows must be invisible to {repo}");
        }
    }
}

/// A V041 index forward-migrates to V042 on `migrate_forward` — reaching LATEST.
#[test]
fn migration_042_forward_migrates_a_v041_index() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    truncate_schema_to(&conn, 41);
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);
    schema::migrate_forward(&conn).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
}

/// Cross-workstream: a legacy single-repo index carrying BOTH A4's GitHub papertrail rows and A5's
/// clone / oracle / memory rows migrates through the WHOLE A-phase ladder (rolled back to the
/// pre-A-phase V037 tip, then `migrate_forward` re-runs V038..V042 idempotently) with every row
/// intact and repo-scoped to the placeholder — then ONE `register_repo` adoption re-points both
/// workstreams' rows to the real id at once. Pins that the two parallel scoping workstreams
/// compose: the ledger advances 37 -> 42 (so both the V041 and V042 `known_version` arms are
/// load-bearing) and adoption spans both tables sets.
#[test]
fn full_ladder_v037_to_v042_scopes_both_workstreams_data() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    schema::apply(&conn).unwrap();

    // Seed A4 (GitHub) + A5 (memory / oracle / clone) rows under the LEGACY placeholder — the shape
    // a legacy single-repo index carries before adoption.
    conn.execute(
        "INSERT INTO github_issues(
             owner, repo, number, html_url, state, title, body, \
         is_pull_request, synced_at_ms,
             repo_id)
         VALUES ('o', 'r', 7, 'http://i', \
         'open', 'zebra title', 'zebra body', 0, 0, ?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO github_fts(
             owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
         VALUES ('o', 'r', 7, 'issue', '1', 'http://i', 'zebra title', 'zebra body', 'other', ?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memories(
             id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
             memory_version, repo_id)
         VALUES ('m1', 'Invariant', 'llama invariant', 'body', 'high', 'active', 0, 0, 'manual', \
         'v1', ?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO oracle_runs(
             repo_id, tool, tool_version, commit_sha, worktree_id, started_at, status, stats_json)
         VALUES (?1, 'scip', 'v1', 'c', '', 0, 'ok', '{}')",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_graph_generations(
             generation, status, theta_floor, normalizer_kind, normalizer_version, source_revision,
             cursor_symbol_id, edges_written, postings_written, started_at_ms, finished_at_ms,
             repo_id)
         VALUES (1, 'Complete', 0.7, 'baseline', ?1, 'rev', 0, 0, 1, 0, 0, ?2)",
        rusqlite::params![crate::index::clones::NORM_VERSION, LEGACY_REPO_ID],
    )
    .unwrap();

    // Roll the ledger back to the pre-A-phase tip and forward-migrate the WHOLE A-phase ladder.
    truncate_schema_to(&conn, 37);
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);
    schema::migrate_forward(&conn).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        42,
        "the A-phase ladder reached V042"
    );

    // Every seeded row survived the ladder, still under the placeholder (scoped to the legacy id).
    let placeholder_count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        placeholder_count(&format!(
            "SELECT COUNT(*) FROM github_issues WHERE repo_id = '{LEGACY_REPO_ID}'"
        )),
        1,
        "A4's github issue survived under the placeholder"
    );
    assert_eq!(
        placeholder_count(
            "SELECT COUNT(*) FROM github_fts WHERE github_fts MATCH 'zebra' AND repo_id = \
             '__unassigned__'"
        ),
        1,
        "A4's github_fts row still MATCHes under the placeholder"
    );
    for (table, id_pred) in [
        ("repo_memories", "id = 'm1'"),
        ("oracle_runs", "tool = 'scip'"),
        ("clone_graph_generations", "generation = 1"),
    ] {
        assert_eq!(
            placeholder_count(&format!(
                "SELECT COUNT(*) FROM {table} WHERE {id_pred} AND repo_id = '{LEGACY_REPO_ID}'"
            )),
            1,
            "A5's {table} row survived under the placeholder"
        );
    }

    // ONE adoption re-points BOTH workstreams' rows onto the real id.
    register_repo(&conn, &identity("repo-real", "r"), std::path::Path::new("/src/r"), 1).unwrap();
    let real_repo = |sql: &str| -> String { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        real_repo("SELECT repo_id FROM github_issues WHERE number = 7"),
        "repo-real",
        "adoption re-points A4's github_issues"
    );
    assert_eq!(
        real_repo("SELECT repo_id FROM github_fts WHERE github_fts MATCH 'zebra'"),
        "repo-real",
        "adoption re-points A4's github_fts mirror"
    );
    assert_eq!(
        real_repo("SELECT repo_id FROM repo_memories WHERE id = 'm1'"),
        "repo-real",
        "adoption re-points A5's memory"
    );
    assert_eq!(
        real_repo("SELECT repo_id FROM oracle_runs WHERE tool = 'scip'"),
        "repo-real",
        "adoption re-points A5's oracle run"
    );
    assert_eq!(
        real_repo("SELECT repo_id FROM clone_graph_generations WHERE generation = 1"),
        "repo-real",
        "adoption re-points A5's clone generation"
    );
}
