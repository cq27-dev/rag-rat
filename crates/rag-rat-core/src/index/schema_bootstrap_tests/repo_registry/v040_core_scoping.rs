use super::*;

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
    let conn = fresh_conn();
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

    schema::migrations::apply_repo_id_core_scoping(&conn, &crate::index::migration_hooks())
        .expect("V040 converges from the torn state");

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
    schema::migrations::apply_repo_id_core_scoping(&conn, &crate::index::migration_hooks())
        .expect("re-apply is a clean no-op");

    // Adoption re-points git_commits.repo_id → real, and the ON UPDATE CASCADE carries the change
    // row.
    register_repo(
        &conn,
        &identity("repo-real", "r"),
        Path::new("/src/r"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
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

    schema::migrations::apply_repo_id_core_scoping(&conn, &crate::index::migration_hooks())
        .unwrap();

    for key in ["active_embedding_model_provisional", "active_embedding_remote_config"] {
        assert!(
            rag_rat_db::meta::repo_meta(&conn, LEGACY_REPO_ID, key).unwrap().is_some(),
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
    let conn = fresh_conn();
    truncate_schema_to(&conn, 39);
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
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

    schema::migrations::apply_repo_id_core_scoping(&conn, &crate::index::migration_hooks())
        .expect("V040 applies on an adopted DB");

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
        rag_rat_db::meta::repo_meta(&conn, "repo-adopted", "active_embedding_model_provisional")
            .unwrap()
            .as_deref(),
        Some("1"),
        "V040 meta relocation lands under the real id on an adopted DB"
    );

    // The runtime fast path (next open re-registers the same repo) stays a no-op and leaves the
    // backfill intact.
    register_repo(
        &conn,
        &identity("repo-adopted", "r"),
        Path::new("/src/r"),
        2,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    let stranded: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE repo_id = ?1", [LEGACY_REPO_ID], |r| r.get(0))
        .unwrap();
    assert_eq!(stranded, 0, "nothing remains stranded under the placeholder");
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

    schema::migrations::apply_repo_id_core_scoping(&conn, &crate::index::migration_hooks())
        .expect("V040 applies");

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

    schema::migrations::apply_repo_id_core_scoping(&conn, &crate::index::migration_hooks())
        .expect("V040 applies (unadopted → placeholder id)");
    let placeholder_id = bound_logical_symbol_id(&conn);
    assert_ne!(placeholder_id, old_id, "V040 already realigned under the placeholder repo_id");

    // Adopt the placeholder as a real repo — the id derivation changes with the real repo_id.
    register_repo(
        &conn,
        &identity("real-repo", "r"),
        Path::new("/src/r"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();

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

/// `realign_logical_symbol_ids` recomputes a group's content-derived id and moves every durable
/// reference onto it. It may only do that when the group's members AGREE on their owner scope.
///
/// A pre-upgrade group can hold the exact collision this version fixes — same-named,
/// same-signature methods under different impl owners merged into one logical symbol. Reading one
/// member's scope arbitrarily would realign the shared group, and every memory bound to it, onto
/// that single owner's identity; the drift heal would then see an unchanged reference and hand the
/// shared bindings to an arbitrary owner instead of treating the split as ambiguous.
#[test]
fn realign_skips_a_group_whose_members_disagree_on_their_owner() {
    let conn = fresh_conn();
    conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    conn.execute(
        "INSERT INTO files(repo_id, path, language, kind, sha256, modified_at_ms, indexed_at_ms,
             commit_sha, worktree_id)
         VALUES ('r', 'src/lib.rs', 'rust', 'source', 's', 0, 0, '', '')",
        [],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('src/lib.rs::run')", [])
        .unwrap();
    let qn: i64 = conn
        .query_row("SELECT id FROM name_strings WHERE value = 'src/lib.rs::run'", [], |r| r.get(0))
        .unwrap();

    // Two DISTINCT owners' methods, merged under one legacy logical symbol.
    let mut members = Vec::new();
    for (owner, line) in [("Alpha", 1), ("Beta", 5)] {
        conn.execute(
            "INSERT INTO symbols(file_id, name, kind, language, qualified_name_id, scope_path,
                 signature, start_line, end_line, start_byte, end_byte)
             VALUES (?1, 'run', 'function', 'rust', ?2, ?3, 'fn run(&self)', ?4, ?4, ?4, ?4)",
            rusqlite::params![file_id, qn, format!("{owner}::run"), line],
        )
        .unwrap();
        members.push(conn.last_insert_rowid());
    }
    conn.execute(
        "INSERT INTO logical_symbols(id, repo_id, language, path, logical_name, qualified_name_id,
             kind, variant_count, group_reason)
         VALUES (4242, 'r', 'rust', 'src/lib.rs', 'run', ?1, 'function', 2, 'cfg')",
        [qn],
    )
    .unwrap();
    for symbol_id in &members {
        conn.execute(
            "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, \
             end_line)
             VALUES (4242, ?1, 1, 1)",
            [symbol_id],
        )
        .unwrap();
    }

    let remapped = crate::index::graph_index::realign_logical_symbol_ids(&conn).unwrap();

    assert_eq!(remapped, 0, "a group with disagreeing member scopes must not be realigned");
    let still_there: i64 = conn
        .query_row("SELECT COUNT(*) FROM logical_symbols WHERE id = 4242", [], |r| r.get(0))
        .unwrap();
    assert_eq!(still_there, 1, "the merged group is left for the key-drift relocation path");
}

/// Binder spelling is canonicalized at EXTRACTION, so `impl<T> Foo<T>` and `impl<U> Foo<U>` both
/// store `Foo<_>` and are one legitimate group. It must still realign; deferring it would strand
/// the row and its durable references on an id hashed from the OLD repo id after adoption
/// re-points `repo_id`.
#[test]
fn realign_still_moves_a_group_of_canonically_equal_scopes() {
    let conn = fresh_conn();
    conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    conn.execute(
        "INSERT INTO files(repo_id, path, language, kind, sha256, modified_at_ms, indexed_at_ms,
             commit_sha, worktree_id)
         VALUES ('r', 'src/lib.rs', 'rust', 'source', 's', 0, 0, '', '')",
        [],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('src/lib.rs::run')", [])
        .unwrap();
    let qn: i64 = conn
        .query_row("SELECT id FROM name_strings WHERE value = 'src/lib.rs::run'", [], |r| r.get(0))
        .unwrap();
    for (scope, line) in [("Foo<_>::run", 1), ("Foo<_>::run", 5)] {
        conn.execute(
            "INSERT INTO symbols(file_id, name, kind, language, qualified_name_id, scope_path,
                 signature, start_line, end_line, start_byte, end_byte)
             VALUES (?1, 'run', 'function', 'rust', ?2, ?3, 'fn run(&self)', ?4, ?4, ?4, ?4)",
            rusqlite::params![file_id, qn, scope, line],
        )
        .unwrap();
        let symbol_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO logical_symbols(id, repo_id, language, path, logical_name,
                 qualified_name_id, kind, variant_count, group_reason)
             VALUES (4243, 'r', 'rust', 'src/lib.rs', 'run', ?1, 'function', 2, 'cfg')
             ON CONFLICT(id) DO NOTHING",
            [qn],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, \
             end_line)
             VALUES (4243, ?1, 1, 1)",
            [symbol_id],
        )
        .unwrap();
    }

    let remapped = crate::index::graph_index::realign_logical_symbol_ids(&conn).unwrap();

    assert_eq!(remapped, 1, "one canonical owner is one group and must be realigned");
}
