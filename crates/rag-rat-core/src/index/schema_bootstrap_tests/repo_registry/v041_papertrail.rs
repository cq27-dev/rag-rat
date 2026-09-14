use super::*;

// --- V041: repo_id scoping on the GitHub papertrail tables (memory-sync phase A4) ---

/// The pre-V041 shape of the seven GitHub tables + `github_fts` (no `repo_id`) plus the V038
/// registry, built in ISOLATION so [`schema::migrations::apply_github_repo_id_scoping`] is
/// exercised against its own inputs (the directory's "assert deferred absence / rebuild behavior in
/// isolation" rule).
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
    schema::migrations::apply_repos_registry(conn).expect("V038 registry seeds the placeholder");
}

/// Fresh `apply` produces the repo-scoped papertrail tables directly (the V060 baseline shape —
/// V041's github scoping only ever runs on legacy DBs now, exercised by the isolation fixtures
/// below). Every papertrail table carries a direct `repo_id` from birth.
#[test]
fn fresh_apply_scopes_the_papertrail_tables_by_repo_id() {
    let conn = fresh_conn();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    for table in [
        "papertrail_refs",
        "papertrail_items",
        "papertrail_comments",
        "papertrail_sync_cursor",
        "papertrail_item_tags",
        "papertrail_fts",
    ] {
        assert!(
            conn_table_columns(&conn, table).contains(&"repo_id".to_string()),
            "{table} carries a direct repo_id column"
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

    schema::migrations::apply_github_repo_id_scoping(&conn)
        .expect("V041 converges from the torn state");

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
    schema::migrations::apply_github_repo_id_scoping(&conn).expect("re-apply is a clean no-op");
}

/// Full-schema adoption of the papertrail: `register_repo` re-points the placeholder papertrail
/// rows (a base table AND the `papertrail_fts` mirror) onto the real id. Kept SEPARATE from the
/// `migration_041_github_rebuild_*` isolation test above because adoption now runs
/// `realign_logical_symbol_ids`, which needs the full core schema the papertrail-only isolation
/// fixture omits (it would trip `no such table: logical_symbols`). The full ladder gives adoption
/// every table it touches.
#[test]
fn register_repo_repoints_papertrail_rows_to_the_real_id() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // A synced item + its FTS mirror seeded under the placeholder, as a pre-adoption index carries
    // them (both explicitly stamped LEGACY_REPO_ID so adoption's placeholder re-point matches).
    conn.execute(
        "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, title, \
         body, synced_at_ms, repo_id)
         VALUES ('github', 'o/r', 'issue', '7', 'http://i', 'open', 'zebra title', 'zebra body', \
         0, ?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_fts(tracker, project, item_kind, item_key, doc_kind, comment_id, \
         url, title, body, classification, repo_id)
         VALUES ('github', 'o/r', 'issue', '7', 'item', '', 'http://i', 'zebra title', \
         'zebra body', 'other', ?1)",
        [LEGACY_REPO_ID],
    )
    .unwrap();

    register_repo(
        &conn,
        &identity("repo-real", "r"),
        Path::new("/src/r"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    let item_repo: String =
        conn.query_row("SELECT repo_id FROM papertrail_items", [], |r| r.get(0)).unwrap();
    assert_eq!(item_repo, "repo-real", "adoption re-points papertrail_items");
    let fts_repo: String = conn
        .query_row(
            "SELECT repo_id FROM papertrail_fts WHERE papertrail_fts MATCH 'zebra'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_repo, "repo-real", "adoption re-points the papertrail_fts mirror in place");
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

    schema::migrations::apply_github_repo_id_scoping(&conn).expect("V041 applies on an adopted DB");

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

    schema::migrations::apply_github_repo_id_scoping(&conn)
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

/// The papertrail natural keys fold `repo_id` (the V044 discipline carried into V060), so a
/// placeholder-stranded row never OCCUPIES a real repo's key: a syncing repo writes its OWN row
/// (fresh content, its `repo_id`) ALONGSIDE the stranded placeholder row rather than clobbering
/// or reclaiming it. Cross-repo isolation is the win — the placeholder rows are re-pointed
/// separately at ADOPTION (see `register_repo_repoints_papertrail_rows_to_the_real_id`).
#[test]
fn a_syncing_repo_is_isolated_from_stranded_placeholder_papertrail_rows() {
    let conn = fresh_conn();
    // The state the consolidated-DB gate leaves: papertrail rows stranded under the placeholder
    // on a two-real-repo DB.
    conn.execute_batch(&format!(
        "INSERT INTO papertrail_refs(tracker, project, item_key, ref_kind, source_kind, \
         source_path, source_commit, source_text, discovered_at_ms, repo_id)
         VALUES ('github', 'o/r', '7', 'unknown', 'manual', NULL, NULL, 'o/r#7', 1, '{p}');
         INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, title, \
         body, synced_at_ms, repo_id)
         VALUES ('github', 'o/r', 'issue', '7', 'http://stale', 'open', 'stale title', \
         'stale body', 1, '{p}');
         INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, title, \
         body, synced_at_ms, repo_id)
         VALUES ('github', 'o/r', 'issue', '8', 'http://other', 'open', 'other stale', \
         'other body', 1, '{p}');
         INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-a', 'a', 1), \
         ('repo-b', 'b', 2);
         DELETE FROM repos WHERE repo_id = '{p}';",
        p = LEGACY_REPO_ID
    ))
    .unwrap();
    // Mirror state before any sync: derived from the stranded base rows.
    rag_rat_papertrail::rebuild_fts(&conn).unwrap();

    // Pin the connection's active repo to repo-a (what `set_context` installs on a real open).
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);
         INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', 'repo-a');",
    )
    .unwrap();

    // repo-a's sync touches o/r#7: re-discovers the ref (same natural key as the stranded row)
    // and refetches the item with fresh content. The incremental FTS writer refreshes only
    // repo-a's own mirror row.
    let reference = rag_rat_papertrail::PapertrailRef {
        item_kind: None,
        tracker: rag_rat_papertrail::Tracker::Github,
        project: "o/r".to_string(),
        item_key: "7".to_string(),
        ref_kind: rag_rat_papertrail::RefKind::Unknown.as_db_str().to_string(),
        source_kind: rag_rat_papertrail::RefSourceKind::Manual.as_db_str().to_string(),
        source_path: None,
        source_commit: None,
        source_text: "o/r#7".to_string(),
    };
    rag_rat_papertrail::store_ref(&conn, &reference).unwrap();
    rag_rat_papertrail::store_item(
        &conn,
        rag_rat_papertrail::Tracker::Github,
        &rag_rat_papertrail::PapertrailItem {
            project: "o/r".to_string(),
            item_kind: rag_rat_papertrail::ItemKind::Issue,
            item_key: "7".to_string(),
            url: "http://fresh".to_string(),
            state: "closed".to_string(),
            title: "fresh title".to_string(),
            body: "fresh body".to_string(),
            author: None,
            created_at: None,
            updated_at: None,
            merged_at: None,
            closed_at: None,
            resolution: None,
            merge_commit_sha: None,
            author_kind: None,
            author_association: None,
            tags: Vec::new(),
        },
    )
    .unwrap();

    // repo-a's sync wrote its OWN item row with fresh content, under its own repo_id…
    let a_title: String = conn
        .query_row(
            "SELECT title FROM papertrail_items WHERE item_key = '7' AND repo_id = 'repo-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(a_title, "fresh title", "repo-a's sync wrote its own fresh row");
    // …and the stranded placeholder row for the SAME item identity is UNTOUCHED — the widened
    // key keeps them distinct instead of one clobbering the other.
    let placeholder_title: String = conn
        .query_row(
            &format!(
                "SELECT title FROM papertrail_items WHERE item_key = '7' AND repo_id = \
                 '{LEGACY_REPO_ID}'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(placeholder_title, "stale title", "the stranded placeholder row is not reclaimed");
    // Both base tables now hold TWO rows for o/r#7 — repo-a's and the placeholder's — coexisting,
    // and the incremental mirror write left the placeholder's mirror row alone.
    for table in ["papertrail_items", "papertrail_refs", "papertrail_fts"] {
        let rows: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE item_key = '7'"), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(rows, 2, "{table}: repo-a's row coexists with the stranded placeholder row");
    }
    // A scoped read for repo-a sees exactly its own fresh row, never the placeholder's.
    let visible: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM papertrail_items WHERE repo_id = 'repo-a' AND item_key = '7'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(visible, 1, "repo-a's scoped read sees exactly its own row");
    // The row the sync did NOT touch (o/r#8) stays under the placeholder for its own repo's sync.
    let untouched: String = conn
        .query_row("SELECT repo_id FROM papertrail_items WHERE item_key = '8'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(untouched, LEGACY_REPO_ID, "the untouched stranded row stays placeholder");
}

/// A V040 index forward-migrates to V041 on `migrate_forward` — reaching LATEST.
#[test]
fn migration_041_forward_migrates_a_v040_index() {
    let conn = fresh_conn();
    truncate_schema_to(&conn, 40);
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
}
