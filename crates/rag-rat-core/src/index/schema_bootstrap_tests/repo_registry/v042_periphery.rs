use super::*;

/// The pre-V042 shape of the clone / oracle / reconcile / memory periphery tables (NO `repo_id`),
/// built by calling the real table-creating DDL fns in ISOLATION — so
/// [`schema::migrations::apply_repo_id_periphery_scoping`] runs against its own genuine inputs (the
/// directory's "assert deferred absence / rebuild behavior in isolation, not the full ladder"
/// rule). Calling the real DDL fns (not hand-copied `CREATE TABLE`s) keeps the fixture drift-free
/// as those tables evolve. The core / GitHub tables are deliberately NOT scoped here (V040 / V041
/// are not run): this fixture drives only V042, so [`register_repo`] adoption is exercised in the
/// full-ladder test below, where every direct-scoped column exists.
fn seed_pre_v042_periphery_schema(conn: &rusqlite::Connection) {
    schema::apply_baseline(conn)
        .expect("baseline: repo_memories/bindings/tags/fts + reconcile_attempts + core tables");
    schema::migrations::apply_oracle_tables(conn).expect("oracle_runs + edge_oracle");
    schema::migrations::apply_scip_moniker_anchors(conn).expect("logical_symbol_monikers");
    schema::migrations::apply_clone_fingerprint_tables(conn)
        .expect("clone_token_df + clone_refinements");
    schema::migrations::apply_clone_graph_tables(conn).expect("clone_graph_generations");
    schema::migrations::apply_dream_findings(conn).expect("dream_findings");
    schema::migrations::apply_repos_registry(conn).expect("V038 registry seeds the placeholder");
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

/// Fresh `apply` runs V042 (the schema tip): every periphery table gains a direct `repo_id` and the
/// content-keyed tables rebuild their PK to lead with it. Owns the absolute `LATEST_SCHEMA_VERSION`
/// pin.
#[test]
fn migration_042_is_the_latest_tip_and_scopes_periphery() {
    let conn = fresh_conn();
    // V042 is no longer the ABSOLUTE tip (V043 adds files.generation, V044 widens the github keys);
    // the tip pin lives with the newest migration's test (`v044_widens_the_github_natural_keys_...`
    // in github_papertrail). Here just assert `apply` reaches LATEST — V042 is on the way to the
    // tip.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

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

    schema::migrations::apply_repo_id_periphery_scoping(&conn, &crate::index::migration_hooks())
        .expect("V042 converges from the torn state");

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
    schema::migrations::apply_repo_id_periphery_scoping(&conn, &crate::index::migration_hooks())
        .expect("re-apply is a clean no-op");
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

    schema::migrations::apply_repo_id_periphery_scoping(&conn, &crate::index::migration_hooks())
        .expect("V042 applies on an adopted DB");

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

    schema::migrations::apply_repo_id_periphery_scoping(&conn, &crate::index::migration_hooks())
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
    let conn = fresh_conn();
    truncate_schema_to(&conn, 41);
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    // Seed A4 (papertrail) + A5 (memory / oracle / clone) rows under the LEGACY placeholder — the
    // shape a legacy single-repo index carries before adoption.
    conn.execute(
        "INSERT INTO papertrail_items(
             tracker, project, item_kind, item_key, url, state, \
         title, body, synced_at_ms,
             repo_id)
         VALUES ('github', 'o/r', 'issue', \
         '7', 'http://i', 'open', 'zebra title', 'zebra body', 0, ?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_fts(
             tracker, project, item_kind, item_key, doc_kind, comment_id, \
         url, title, body, classification, repo_id)
         VALUES ('github', 'o/r', 'issue', '7', \
         'item', '', 'http://i', 'zebra title', 'zebra body', 'other', ?1)",
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
        rusqlite::params![rag_rat_clones::NORM_VERSION, LEGACY_REPO_ID],
    )
    .unwrap();
    // A V046 dream-v2 verification sibling — repo_id-scoped from birth. Adoption must re-point it
    // like the rest of the A5 periphery set (it is NOT part of V042; the sibling landed later).
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, checked_at_ms)
         VALUES ('m1', ?1, 'bh', 'current', 0)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_model_failures(memory_id, repo_id, pass, content_hash, model_id, \
         prompt_version, reason, failed_at_ms)
         VALUES ('m1', ?1, 'compact', 'bh', 'model', 'prompt', 'summary_guard_rejected', 0)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    // A V056 derived change-coupling row — repo_id-scoped, no FK; adoption must re-point it too so
    // it stays consistent with the (separately relocated) git_coupling_stamp repo_meta.
    conn.execute(
        "INSERT INTO git_change_couplings(repo_id, path_a, path_b, co_change_count, \
         path_a_change_count, path_b_change_count, window_commit_count, last_co_change_at_s, \
         computed_at_ms) VALUES (?1, 'a.rs', 'b.rs', 2, 2, 2, 4, 100, 0)",
        [LEGACY_REPO_ID],
    )
    .unwrap();

    // Roll the ledger back to the pre-A-phase tip and forward-migrate the WHOLE A-phase ladder.
    truncate_schema_to(&conn, 37);
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "the A-phase ladder reached LATEST (V042's periphery scoping is on the way to the tip)"
    );

    // Every seeded row survived the ladder, still under the placeholder (scoped to the legacy id).
    let placeholder_count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        placeholder_count(&format!(
            "SELECT COUNT(*) FROM papertrail_items WHERE repo_id = '{LEGACY_REPO_ID}'"
        )),
        1,
        "A4's papertrail item survived under the placeholder"
    );
    assert_eq!(
        placeholder_count(
            "SELECT COUNT(*) FROM papertrail_fts WHERE papertrail_fts MATCH 'zebra' AND repo_id = \
             '__unassigned__'"
        ),
        1,
        "A4's papertrail_fts row still MATCHes under the placeholder"
    );
    for (table, id_pred) in [
        ("repo_memories", "id = 'm1'"),
        ("oracle_runs", "tool = 'scip'"),
        ("clone_graph_generations", "generation = 1"),
        ("memory_reality", "memory_id = 'm1'"),
        ("memory_model_failures", "memory_id = 'm1'"),
        ("git_change_couplings", "path_a = 'a.rs'"),
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
    register_repo(
        &conn,
        &identity("repo-real", "r"),
        std::path::Path::new("/src/r"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    let real_repo = |sql: &str| -> String { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        real_repo("SELECT repo_id FROM papertrail_items WHERE item_key = '7'"),
        "repo-real",
        "adoption re-points A4's papertrail_items"
    );
    assert_eq!(
        real_repo("SELECT repo_id FROM papertrail_fts WHERE papertrail_fts MATCH 'zebra'"),
        "repo-real",
        "adoption re-points A4's papertrail_fts mirror"
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
    assert_eq!(
        real_repo("SELECT repo_id FROM memory_reality WHERE memory_id = 'm1'"),
        "repo-real",
        "adoption re-points the V046 memory_reality verification sibling"
    );
    assert_eq!(
        real_repo("SELECT repo_id FROM memory_model_failures WHERE memory_id = 'm1'"),
        "repo-real",
        "adoption re-points the V047 memory_model_failures sibling"
    );
    assert_eq!(
        real_repo("SELECT repo_id FROM git_change_couplings WHERE path_a = 'a.rs'"),
        "repo-real",
        "adoption re-points the V056 git_change_couplings derived table"
    );
}

/// Codex #566 finding B: `git_change_couplings` is in `DIRECT_SCOPED_ADOPTION_TABLES`, so a
/// LocalOnly→Portable adoption re-points its rows TOGETHER with the `git_coupling_stamp` repo_meta
/// — keeping the derived table stamp-consistent. Without the registration the stamp would move but
/// the rows would strand, and `ensure_coupling_fresh` would then see a matching stamp and serve an
/// EMPTY section instead of the adopted rows.
#[test]
fn adoption_repoints_change_couplings_consistently_with_its_stamp() {
    let conn = fresh_conn();

    // A derived coupling row + a CONSISTENT freshness stamp (head h1 + params 1), both under the
    // placeholder — the shape a legacy single-repo index carries pre-adoption.
    conn.execute(
        "INSERT INTO git_change_couplings(repo_id, path_a, path_b, co_change_count, \
         path_a_change_count, path_b_change_count, window_commit_count, last_co_change_at_s, \
         computed_at_ms) VALUES (?1, 'a.rs', 'b.rs', 2, 2, 2, 4, 100, 50)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    // A stamp CONSISTENT with the seeded state: no git-history cursors set ⇒ empty history key,
    // plus the current params version. The stamp is pure git-history (`history_key:params`, no
    // files axis), so the post-adoption `ensure_coupling_fresh` sees a matching stamp and does
    // NOT recompute.
    let consistent_stamp = format!(":{}", crate::index::change_coupling::COUPLING_PARAMS_VERSION);
    rag_rat_db::meta::set_repo_meta(&conn, LEGACY_REPO_ID, "git_coupling_stamp", &consistent_stamp)
        .unwrap();

    register_repo(
        &conn,
        &identity("repo-real", "r"),
        Path::new("/src/r"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    // Rows re-pointed onto the real id, none stranded under the placeholder.
    let count_for = |repo: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM git_change_couplings WHERE repo_id = ?1",
            [repo],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(count_for("repo-real"), 1, "coupling row re-pointed onto the real id");
    assert_eq!(count_for(LEGACY_REPO_ID), 0, "no coupling row stranded under the placeholder");

    // Stamp + rows consistent (the stamp relocated to the real id too), so ensure_coupling_fresh is
    // a NO-OP — it must NOT recompute the freshly-adopted rows away to an empty section.
    crate::index::change_coupling::ensure_coupling_fresh(&conn, 999).unwrap();
    let computed_at: i64 = conn
        .query_row(
            "SELECT computed_at_ms FROM git_change_couplings WHERE repo_id = 'repo-real'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(computed_at, 50, "matching stamp => no recompute; the adopted rows survive intact");
}
