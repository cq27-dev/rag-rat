use super::*;

/// Fresh registration ADOPTS the placeholder in place: the `__unassigned__` row becomes the real
/// repo, no placeholder remains, and the working-tree root is recorded.
#[test]
fn register_repo_adopts_the_placeholder() {
    let conn = fresh_conn();

    let returned = register_repo(
        &conn,
        &identity("repo-abc", "myrepo"),
        Path::new("/src/myrepo"),
        123,
        &crate::index::migration_hooks(),
    )
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

/// Adoption re-points the distilled-record store (#703): a `papertrail_distill` row seeded under
/// the `__unassigned__` placeholder must carry the real repo id after registration, or the record
/// would strand under the retired id and vanish from the active scope.
#[test]
fn register_repo_repoints_distill_rows_from_the_placeholder() {
    let conn = fresh_conn();
    conn.execute(
        "INSERT INTO papertrail_distill
             (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
              fix_edge_source, thread_shape, distilled_at_ms, repo_id)
         VALUES ('github','o/r','issue','5','h',1,'provider','investigation',1,?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_sources
             (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
              source_item_kind, source_item_key, source_kind, source_part, source_id, exact_text,
              repo_id)
         VALUES \
         ('github','o/r','issue','5',0,'primary',NULL,'issue','5','item','body','5','body',?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_units
             (tracker, project, item_kind, item_key, unit_ordinal, source_ordinal, byte_start,
              byte_end, repo_id)
         VALUES ('github','o/r','issue','5',0,0,0,4,?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_fix_diffs
             (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
         VALUES ('github','o/r','issue','5','abc123','src/lib.rs','patch',?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_xrefs
             (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
              target_project, target_item_kind, target_item_key, ref_kind, title, opening,
              repo_id)
         VALUES ('github','o/r','issue','5',0,'github','o/r','issue','9','reference','t','o',?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();

    register_repo(
        &conn,
        &identity("repo-abc", "myrepo"),
        Path::new("/src/myrepo"),
        123,
        &crate::index::migration_hooks(),
    )
    .expect("register");

    let repo_id: String = conn
        .query_row("SELECT repo_id FROM papertrail_distill WHERE item_key='5'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(repo_id, "repo-abc", "the distill row is re-pointed to the adopted repo id");
    for table in [
        "papertrail_distill_sources",
        "papertrail_distill_units",
        "papertrail_distill_fix_diffs",
        "papertrail_distill_xrefs",
    ] {
        let adopted: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE repo_id='repo-abc'"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(adopted, 1, "the snapshot table `{table}` is re-pointed too");
    }
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
    let conn = fresh_conn();
    register_repo(
        &conn,
        &identity("repo-abc", "myrepo"),
        Path::new("/src/myrepo"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 0, "adopted: placeholder gone");

    // The exact re-run `create_or_migrate` (hence `rebuild`) performs on an existing index.
    schema::apply(&conn, &crate::index::migration_hooks())
        .expect("re-apply is idempotent on an already-migrated DB");

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
    let conn = fresh_conn();
    // As V039 leaves a not-yet-adopted DB: per-repo meta under the placeholder.
    rag_rat_db::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "source_root", "/src/repo").unwrap();
    rag_rat_db::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms", "9").unwrap();

    register_repo(
        &conn,
        &identity("repo-abc", "myrepo"),
        Path::new("/src/repo"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    // Adoption re-pointed the meta to the real id and `sole_repo_id` resolves it.
    assert_eq!(
        schema::sole_repo_id(&conn).unwrap(),
        "repo-abc",
        "adopted: real id is the sole repo"
    );

    // The exact re-run `create_or_migrate` (hence `rebuild`) performs on an existing index.
    schema::apply(&conn, &crate::index::migration_hooks())
        .expect("re-apply is idempotent on an already-migrated DB");

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
        rag_rat_db::meta::repo_meta(&conn, "repo-abc", "source_root").unwrap().as_deref(),
        Some("/src/repo"),
    );
    assert_eq!(
        rag_rat_db::meta::repo_meta(&conn, "repo-abc", "indexed_at_ms").unwrap().as_deref(),
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
    let conn = fresh_conn();

    let err = register_repo(
        &conn,
        &identity(LEGACY_REPO_ID, "myrepo"),
        Path::new("/src/myrepo"),
        1,
        &crate::index::migration_hooks(),
    )
    .expect_err("registering the reserved placeholder must be refused");
    assert!(err.to_string().contains(LEGACY_REPO_ID), "refusal names the reserved value: {err}");

    // DB unchanged: exactly the placeholder row, no real repo, no root recorded under the marker.
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 1, "placeholder untouched");
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 1, "no extra repos row minted");
    let roots: i64 = conn.query_row("SELECT COUNT(*) FROM repo_roots", [], |r| r.get(0)).unwrap();
    assert_eq!(roots, 0, "no root recorded under the marker");

    // A subsequent REAL registration adopts cleanly (the failed attempt left nothing behind).
    register_repo(
        &conn,
        &identity("repo-abc", "myrepo"),
        Path::new("/src/myrepo"),
        2,
        &crate::index::migration_hooks(),
    )
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
    let conn = fresh_conn();

    for blank in ["", "   "] {
        let err = register_repo(
            &conn,
            &identity(blank, "myrepo"),
            Path::new("/src/myrepo"),
            1,
            &crate::index::migration_hooks(),
        )
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
    let conn = fresh_conn();
    let id = identity("repo-abc", "myrepo");

    register_repo(&conn, &id, Path::new("/src/myrepo"), 1, &crate::index::migration_hooks())
        .unwrap();
    register_repo(&conn, &id, Path::new("/src/myrepo"), 2, &crate::index::migration_hooks())
        .unwrap();

    assert_eq!(repo_row_count(&conn, "repo-abc"), 1);
    assert_eq!(root_count(&conn, "repo-abc"), 1, "same root is not duplicated");
}

/// A second root path for the SAME repo (a worktree/clone on the same machine) appends a
/// `repo_roots` row without minting a new repo.
#[test]
fn register_repo_appends_a_second_root() {
    let conn = fresh_conn();
    let id = identity("repo-abc", "myrepo");

    register_repo(&conn, &id, Path::new("/src/myrepo"), 1, &crate::index::migration_hooks())
        .unwrap();
    register_repo(
        &conn,
        &id,
        Path::new("/src/myrepo-worktree"),
        2,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    assert_eq!(repo_row_count(&conn, "repo-abc"), 1, "still one repo");
    assert_eq!(root_count(&conn, "repo-abc"), 2, "both roots recorded");
}

/// A7: once a real repo owns the DB, registering a DIFFERENT real repo at an unclaimed root
/// REGISTERS IT — several repos sharing one global database is the multi-repo default (replacing
/// phase A's single-repo "refuse a second real repo" invariant). Both repos coexist; neither is
/// re-pointed.
#[test]
fn register_repo_registers_a_second_repo_in_a_consolidated_db() {
    let conn = fresh_conn();
    register_repo(
        &conn,
        &identity("repo-abc", "a"),
        Path::new("/src/a"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    let registered = register_repo(
        &conn,
        &identity("repo-xyz", "b"),
        Path::new("/src/b"),
        2,
        &crate::index::migration_hooks(),
    )
    .expect("a different real repo at an unclaimed root registers as a second repo");
    assert_eq!(registered, "repo-xyz");
    // Both repos are real, each with its own recorded root; neither was re-pointed.
    assert_eq!(repo_row_count(&conn, "repo-abc"), 1);
    assert_eq!(repo_row_count(&conn, "repo-xyz"), 1);
    assert_eq!(root_count(&conn, "repo-abc"), 1);
    assert_eq!(root_count(&conn, "repo-xyz"), 1);
    assert!(schema::multiple_real_repos(&conn).unwrap(), "the DB now holds two real repos");
}

/// V039 leaves the relocated meta under the placeholder repo_id; `register_repo` adoption MUST
/// carry those rows over to the real repo_id (Step 4), so a post-migration open does not orphan
/// them.
#[test]
fn register_repo_adoption_relocates_repo_meta_rows() {
    let conn = fresh_conn();
    // As V039 leaves it: per-repo meta under the placeholder.
    rag_rat_db::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "source_root", "/src/repo").unwrap();
    rag_rat_db::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms", "9").unwrap();

    register_repo(
        &conn,
        &identity("repo-abc", "myrepo"),
        Path::new("/src/repo"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    assert_eq!(
        rag_rat_db::meta::repo_meta(&conn, "repo-abc", "source_root").unwrap().as_deref(),
        Some("/src/repo"),
    );
    assert_eq!(
        rag_rat_db::meta::repo_meta(&conn, "repo-abc", "indexed_at_ms").unwrap().as_deref(),
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

/// The #427 same-identity-join hint's two read-only lookups: whether a given root is already one
/// of `repo_id`'s recorded checkouts, and which recorded root is the earliest-registered (the
/// "home" checkout the hint names).
#[test]
fn recorded_root_helpers_report_membership_and_earliest() {
    use rag_rat_db::schema::{earliest_recorded_root, repo_has_recorded_root};
    let conn = fresh_conn();
    let id = identity("repo-x", "repo-x");
    // First root A (registered_at 100), then a second checkout B (registered_at 200).
    register_repo(&conn, &id, Path::new("/checkout-a"), 100, &crate::index::migration_hooks())
        .unwrap();
    register_repo(&conn, &id, Path::new("/checkout-b"), 200, &crate::index::migration_hooks())
        .unwrap();

    assert!(repo_has_recorded_root(&conn, &id.repo_id, "/checkout-a").unwrap());
    assert!(repo_has_recorded_root(&conn, &id.repo_id, "/checkout-b").unwrap());
    assert!(!repo_has_recorded_root(&conn, &id.repo_id, "/checkout-c").unwrap());
    assert_eq!(
        earliest_recorded_root(&conn, &id.repo_id).unwrap().as_deref(),
        Some("/checkout-a"),
        "earliest by registered_at_ms wins",
    );

    // Tiebreak: two roots registered at the SAME instant → the lexicographically-smaller root wins
    // deterministically (the `ORDER BY registered_at_ms, root` secondary key).
    let tied = identity("repo-tied", "repo-tied");
    register_repo(&conn, &tied, Path::new("/z-checkout"), 500, &crate::index::migration_hooks())
        .unwrap();
    register_repo(&conn, &tied, Path::new("/a-checkout"), 500, &crate::index::migration_hooks())
        .unwrap();
    assert_eq!(
        earliest_recorded_root(&conn, &tied.repo_id).unwrap().as_deref(),
        Some("/a-checkout"),
        "equal registered_at_ms → lexicographically-smaller root breaks the tie",
    );
}

/// `single_repo_id` returns the sole `repos` row — the placeholder before adoption, the real id
/// after — the connection-level stand-in the per-repo accessors resolve until A3.
#[test]
fn single_repo_id_returns_the_sole_repo() {
    let conn = fresh_conn();
    assert_eq!(
        schema::sole_repo_id(&conn).unwrap(),
        LEGACY_REPO_ID,
        "the placeholder is the sole repo before adoption"
    );

    register_repo(
        &conn,
        &identity("repo-abc", "myrepo"),
        Path::new("/src/repo"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    assert_eq!(
        schema::sole_repo_id(&conn).unwrap(),
        "repo-abc",
        "the adopted real id is the sole repo after registration"
    );
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
    schema::apply(&conn, &crate::index::migration_hooks()).expect("apply");
    // As V039 leaves a not-yet-adopted DB: per-repo meta under the placeholder.
    rag_rat_db::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "source_root", "/src/repo").unwrap();
    rag_rat_db::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms", "9").unwrap();

    // Fail the adoption at its LAST mutation (the placeholder delete), mid-transaction.
    conn.execute_batch(
        "CREATE TRIGGER fail_placeholder_delete BEFORE DELETE ON repos
         WHEN OLD.repo_id = '__unassigned__'
         BEGIN SELECT RAISE(ABORT, 'injected adoption failure'); END;",
    )
    .expect("install failure trigger");

    let err = register_repo(
        &conn,
        &identity("repo-abc", "myrepo"),
        Path::new("/src/repo"),
        1,
        &crate::index::migration_hooks(),
    )
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
        rag_rat_db::meta::repo_meta(&conn, LEGACY_REPO_ID, "source_root").unwrap().as_deref(),
        Some("/src/repo"),
        "repo_meta stays under the placeholder (the re-point rolled back)",
    );
    assert_eq!(
        rag_rat_db::meta::repo_meta(&conn, LEGACY_REPO_ID, "indexed_at_ms").unwrap().as_deref(),
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
    register_repo(
        &conn,
        &identity("repo-abc", "myrepo"),
        Path::new("/src/repo"),
        2,
        &crate::index::migration_hooks(),
    )
    .expect("adoption succeeds once the fault is removed");
    assert_eq!(repo_row_count(&conn, LEGACY_REPO_ID), 0, "placeholder adopted away");
    assert_eq!(repo_row_count(&conn, "repo-abc"), 1, "the real repo owns the DB");
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), "repo-abc");
    assert_eq!(
        rag_rat_db::meta::repo_meta(&conn, "repo-abc", "source_root").unwrap().as_deref(),
        Some("/src/repo"),
        "meta carried over to the real id on the successful adoption",
    );
    assert_eq!(root_count(&conn, "repo-abc"), 1, "its root is recorded");
}

/// Adoption re-points write the REAL tables even when the connection carries a temp `files` scope
/// view — the incremental pass's bare open installs one BEFORE `adopt_repo_from_config` runs, and
/// an unqualified `UPDATE files` resolves to the view ("cannot modify files because it is a
/// view"), aborting the first index of a fresh keyless DB in an identity-bearing repo.
#[test]
fn adoption_repoints_files_through_a_connection_carrying_the_scope_view() {
    let conn = fresh_conn();
    // A placeholder-scoped file row awaiting adoption.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation)
         VALUES ('src/a.rs', 'rust', 'source', 'h', 0, 0, '', '', '__unassigned__', 0)",
        [],
    )
    .unwrap();
    // The temp `files` view a bare open installs (shape irrelevant — its EXISTENCE is what
    // shadows the table name for unqualified writes).
    conn.execute_batch(
        "CREATE TEMP VIEW files AS SELECT * FROM main.files WHERE repo_id = '__unassigned__'",
    )
    .unwrap();

    register_repo(
        &conn,
        &identity("repo-viewed", "v"),
        Path::new("/src/v"),
        1,
        &crate::index::migration_hooks(),
    )
    .expect("adoption must write main.files through the shadowing temp view");
    let adopted: i64 = conn
        .query_row("SELECT COUNT(*) FROM main.files WHERE repo_id = 'repo-viewed'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(adopted, 1, "the placeholder file row re-pointed to the adopted id");
}
