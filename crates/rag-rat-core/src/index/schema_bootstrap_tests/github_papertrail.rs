use super::*;

#[test]
fn papertrail_for_commit_prefers_commit_sourced_github_refs() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("docs")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);
    fs::write(root.join("docs/search.md"), "# Decision\nalpha\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Fix search rationale", "-m", "Fixes #42"]);

    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();
    let commit = db
        .storage
        .connection()
        .query_row("SELECT hash FROM git_commits LIMIT 1", [], |row| row.get::<_, String>(0))
        .unwrap();
    let mock = MockGitHubClient;
    papertrail::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
        .unwrap();

    let papertrail = db.papertrail_for_commit(&commit[..7], 10).unwrap();
    assert_eq!(papertrail.github_evidence.first().map(|item| item.number), Some(42));
    assert_eq!(
        papertrail.github_evidence.first().map(|item| item.evidence_kind),
        Some("literal_github_ref")
    );
    assert!(
        papertrail.fallback_github_evidence.is_empty(),
        "structured commit refs should suppress noisy fallback evidence: {papertrail:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn papertrail_for_symbol_dedupes_duplicate_file_refs() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "// First rationale (#42)\n// Second rationale (#42)\npub fn tracked_symbol() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let mock = MockGitHubClient;
    papertrail::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
        .unwrap();
    let papertrail = db
        .papertrail_for_symbol("tracked_symbol", Some(Language::Rust), 10)
        .unwrap()
        .expect("tracked symbol papertrail");

    assert_eq!(
        papertrail
            .github_evidence
            .iter()
            .filter(|item| item.number == 42 && item.item_kind == "issue")
            .count(),
        1,
        "duplicate #42 refs in one file should collapse to one issue evidence row: {papertrail:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn github_sync_keeps_partial_cache_and_skips_synced_refs_after_404() {
    let (root, config) = markdown_config(
        "# Decision\nRefs cq27-dev/rag-rat#42 and cq27-dev/rag-rat#404\nwe will keep sqlite\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    let mock = PartiallyFailingGitHubClient;

    let report =
        papertrail::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
            .unwrap();
    assert_eq!(report.discovered_refs, 2);
    assert_eq!(report.synced_items, 5);
    assert_eq!(report.failed_refs, 1);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].number, 404);
    assert_eq!(report.errors[0].status, "not_found");

    let issue_hits = db.papertrail_issue_search("sqlite", 10).unwrap();
    assert_eq!(issue_hits.len(), 1);
    assert_eq!(issue_hits[0].number, 42);

    let second =
        papertrail::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
            .unwrap();
    assert_eq!(second.synced_items, 0);
    assert_eq!(second.skipped_refs, 2);
    assert_eq!(second.failed_refs, 0);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_recovers_when_fts_is_marked_dirty() {
    let (root, config) = markdown_config("alpha token");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.mark_fts_dirty().unwrap();

    let dirty = db.status(&config.database).unwrap();
    assert!(dirty.fts_dirty);
    assert!(!dirty.fts_fresh);

    let hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].summary, "alpha token");
    let fresh = db.status(&config.database).unwrap();
    assert!(!fresh.fts_dirty);
    assert!(fresh.fts_fresh);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn read_chunk_relocates_small_line_drift_to_current_text() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let chunk_id = first_chunk_id(&db);
    fs::write(root.join("docs/search.md"), "inserted\n# Title\nalpha token\n").unwrap();

    let chunk = db.read_chunk(chunk_id).unwrap().unwrap();
    assert_eq!(chunk.start_line, 2);
    assert_eq!(chunk.end_line, 3);
    assert_eq!(chunk.text, "# Title\nalpha token\n");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn read_chunk_large_drift_reindexes_and_reports_stale_chunk() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let chunk_id = first_chunk_id(&db);
    fs::write(root.join("docs/search.md"), "# Replacement\nbeta token\n").unwrap();

    let err = db.read_chunk(chunk_id).unwrap_err().to_string();
    assert!(err.contains("StaleChunk"), "{err}");
    let hits = db.search("beta", 10, false).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(db.search("alpha", 10, false).unwrap().is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_retries_after_healing_stale_hit() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    fs::write(root.join("docs/search.md"), "# Title\nbeta token\n").unwrap();

    let hits = db.search("alpha", 10, false).unwrap();
    assert!(hits.is_empty());
    let beta_hits = db.search("beta", 10, false).unwrap();
    assert_eq!(beta_hits.len(), 1);
    assert!(beta_hits[0].summary.contains("beta"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_heals_relocated_hits_before_returning_line_spans() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    fs::write(root.join("docs/search.md"), "inserted\n# Title\nalpha token\n").unwrap();

    let hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].start_line, 2);
    assert_eq!(hits[0].end_line, 3);
    assert!(hits[0].summary.contains("alpha"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn read_chunk_deleted_source_reports_gone() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let chunk_id = first_chunk_id(&db);
    fs::remove_file(root.join("docs/search.md")).unwrap();

    let err = db.read_chunk(chunk_id).unwrap_err().to_string();
    assert!(err.contains("Gone"), "{err}");
    assert!(db.search("alpha", 10, false).unwrap().is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_returns_needs_reindex_when_heal_cap_is_exceeded() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    for index in 0..=MAX_AUTO_HEAL_FILES_PER_CALL {
        fs::write(docs.join(format!("doc-{index}.md")), "common stale token\n").unwrap();
    }
    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();
    for index in 0..=MAX_AUTO_HEAL_FILES_PER_CALL {
        fs::write(docs.join(format!("doc-{index}.md")), "fresh replacement token\n").unwrap();
    }

    let err = db.search("common", 20, false).unwrap_err().to_string();
    assert!(err.contains("needs_reindex"), "{err}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_drops_deleted_file_instead_of_erroring() {
    // Invariant: when a search hit references a source file that was deleted on disk
    // since indexing, heal_file treats the missing file as a DELETION (mark_file_deleted)
    // rather than propagating a raw ENOENT. search_with_heal then re-searches without it,
    // so search returns Ok with the surviving file only — never Err(NotFound).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("keep.md"), "shared marker token\n").unwrap();
    fs::write(docs.join("drop.md"), "shared marker token\n").unwrap();
    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let initial = db.search("marker", 10, false).unwrap();
    assert_eq!(initial.len(), 2);

    fs::remove_file(docs.join("drop.md")).unwrap();

    let hits = db.search("marker", 10, false).unwrap();
    assert!(hits.iter().all(|hit| !hit.path.ends_with("drop.md")), "{hits:?}");
    assert!(hits.iter().any(|hit| hit.path.ends_with("keep.md")), "{hits:?}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn heal_index_limit_does_not_warn_when_only_fresh_files_are_skipped() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("one.md"), "one fresh token\n").unwrap();
    fs::write(docs.join("two.md"), "two fresh token\n").unwrap();
    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let report = db.heal_index(Some(1)).unwrap();

    assert_eq!(report.healed_files, 0);
    assert_eq!(report.removed_files, 0);
    assert_eq!(report.skipped_files, 2);
    assert_eq!(report.message, None);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_recovers_when_fts_revision_is_stale() {
    let (root, config) = markdown_config("alpha token");
    let db = IndexDatabase::rebuild(&config).unwrap();
    // `fts_source_revision` is a GLOBAL freshness key (V040 reclassification) — stored in
    // `index_meta`, so stale it there, not in per-repo `repo_meta`.
    db.set_meta("fts_source_revision", "stale").unwrap();

    let stale = db.status(&config.database).unwrap();
    assert!(!stale.fts_dirty);
    assert!(!stale.fts_fresh);

    let hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(hits.len(), 1);
    let fresh = db.status(&config.database).unwrap();
    assert_eq!(fresh.fts_source_revision.as_deref(), Some(fresh.content_revision.as_str()));
    assert!(fresh.fts_fresh);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn parser_failures_report_paths() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("broken.rs"), "pub fn broken(").unwrap();
    let config = Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };

    let db = IndexDatabase::rebuild(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.parser_failures, 1);
    assert_eq!(status.parser_failure_paths[0].path, "src/broken.rs");

    let _ = fs::remove_dir_all(root);
}

// --- V044 (phase A7): GitHub natural-key widening ---

/// Fresh `schema::apply` (baseline's old `(owner, repo, number)` keys → V041 repo_id column → V044
/// widening) leaves the `(owner, repo, number)`-style keys folding `repo_id`: the named unique
/// indexes exist and `github_ref_sync`'s PK leads with `repo_id`. Re-applying is idempotent (the
/// sentinel short-circuits).
#[test]
fn v044_widens_the_github_natural_keys_to_include_repo_id() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // The ABSOLUTE schema-tip pin moved to the newest migration's test (V046 —
    // `migration_046_creates_the_verification_tables_on_fresh_apply`); here just assert
    // `apply` reaches LATEST symbolically (the hardcoded-LATEST footgun).
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    assert!(conn_index_exists(&conn, "idx_github_issues_repo_unique"), "issues unique index");
    assert!(conn_index_exists(&conn, "idx_github_pull_requests_repo_unique"), "pulls unique index");
    let ref_sync_pk_has_repo_id: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('github_ref_sync') WHERE name='repo_id' AND \
             pk > 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ref_sync_pk_has_repo_id > 0, "github_ref_sync PK now includes repo_id");
    // Re-apply is a no-op (the named issues index is the all-or-nothing sentinel).
    schema::apply(&conn).unwrap();
    assert!(conn_index_exists(&conn, "idx_github_issues_repo_unique"), "index survives re-apply");
}

/// The load-bearing multi-repo invariant: two repos can each cache the SAME external `(owner, repo,
/// number)` after V044 — the widened keys are per-repo. A same-repo duplicate of that natural key
/// is still rejected, so the uniqueness the writers' `ON CONFLICT` relies on is preserved.
#[test]
fn v044_lets_two_repos_cache_the_same_github_item() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    for repo in ["repo-a", "repo-b"] {
        conn.execute(
            "INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, \
             synced_at_ms, repo_id) VALUES ('o','r',7,'u','open','t','b',0,?1)",
            [repo],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO github_pull_requests(owner, repo, number, html_url, state, title, body, \
             synced_at_ms, repo_id) VALUES ('o','r',7,'u','open','t','b',0,?1)",
            [repo],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO github_ref_sync(owner, repo, number, status, synced_at_ms, repo_id) \
             VALUES ('o','r',7,'ok',0,?1)",
            [repo],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO github_refs(owner, repo, number, ref_kind, source_kind, source_text, \
             discovered_at_ms, repo_id) VALUES ('o','r',7,'closing','file','txt',0,?1)",
            [repo],
        )
        .unwrap();
    }
    for table in ["github_issues", "github_pull_requests", "github_ref_sync", "github_refs"] {
        let rows: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE owner='o' AND repo='r' AND number=7"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "both repos coexist in {table} for the same (owner, repo, number)");
    }
    // Within one repo the widened key still rejects a duplicate (what the writers' ON CONFLICT
    // needs).
    let dup = conn.execute(
        "INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, \
         synced_at_ms, repo_id) VALUES ('o','r',7,'u','open','t','b',0,'repo-a')",
        [],
    );
    assert!(dup.is_err(), "a same-repo duplicate of (owner, repo, number) is still rejected");
}

// --- V045 (phase A7): GitHub id-keyed child widening ---

/// V045 is the schema tip: the id-keyed child caches gain `(repo_id, id)` uniqueness, so two repos
/// sharing an external issue/PR each keep their own copy of its comments/reviews (the pre-V045
/// `INSERT OR REPLACE` restamped the single row to the last syncer, evicting the sibling's scoped
/// papertrail). Re-apply is idempotent (the named `github_comments` index is the sentinel).
#[test]
fn v045_widens_the_github_child_keys_to_include_repo_id() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // The ABSOLUTE schema-tip pin moved to V046's test (dream verification tables) once it became
    // the newest migration; this is now the symbolic `current_version == LATEST_SCHEMA_VERSION`.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    for index in [
        "idx_github_comments_repo_unique",
        "idx_github_reviews_repo_unique",
        "idx_github_review_comments_repo_unique",
    ] {
        assert!(conn_index_exists(&conn, index), "{index} exists");
    }

    // Two repos hold the SAME external comment id; a same-repo duplicate is still rejected.
    for repo in ["repo-a", "repo-b"] {
        conn.execute(
            "INSERT INTO github_comments(id, owner, repo, number, html_url, body, synced_at_ms, \
             repo_id) VALUES (7, 'o', 'r', 1, 'u', 'b', 0, ?1)",
            [repo],
        )
        .unwrap();
    }
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM github_comments WHERE id = 7", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2, "both repos keep their own copy of the shared comment");
    let dup = conn.execute(
        "INSERT INTO github_comments(id, owner, repo, number, html_url, body, synced_at_ms, \
         repo_id) VALUES (7, 'o', 'r', 1, 'u', 'b', 0, 'repo-a')",
        [],
    );
    assert!(dup.is_err(), "a same-repo duplicate id is still rejected");

    schema::apply(&conn).unwrap();
    assert!(conn_index_exists(&conn, "idx_github_comments_repo_unique"), "survives re-apply");
}

/// V045's BACKFILL rule in isolation, against the exact V044-state shape (`id INTEGER PRIMARY
/// KEY` + the V041 `repo_id` column — reproduced here because a fresh `apply` is already
/// post-V045): a child row is duplicated once PER OWNING-PARENT repo (comments parent = issues ∪
/// pulls; reviews/review comments parent = pulls), and an orphan with no cached parent survives
/// under its own stamped repo_id. No row is lost.
#[test]
fn migration_045_duplicates_children_per_owning_repo_and_keeps_orphans() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "
        CREATE TABLE github_issues(
            id INTEGER PRIMARY KEY AUTOINCREMENT, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, html_url TEXT NOT NULL, state TEXT NOT NULL,
            title TEXT NOT NULL, body TEXT NOT NULL, author TEXT, created_at TEXT,
            updated_at TEXT, is_pull_request INTEGER NOT NULL DEFAULT 0,
            synced_at_ms INTEGER NOT NULL, repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        CREATE TABLE github_pull_requests(
            id INTEGER PRIMARY KEY AUTOINCREMENT, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, html_url TEXT NOT NULL, state TEXT NOT NULL,
            title TEXT NOT NULL, body TEXT NOT NULL, author TEXT, created_at TEXT,
            updated_at TEXT, merged_at TEXT, synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        CREATE TABLE github_comments(
            id INTEGER PRIMARY KEY, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, html_url TEXT NOT NULL, body TEXT NOT NULL, author TEXT,
            created_at TEXT, updated_at TEXT, synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        CREATE TABLE github_reviews(
            id INTEGER PRIMARY KEY, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, html_url TEXT, state TEXT NOT NULL, body TEXT NOT NULL,
            author TEXT, submitted_at TEXT, synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        CREATE TABLE github_review_comments(
            id INTEGER PRIMARY KEY, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, path TEXT, html_url TEXT NOT NULL, body TEXT NOT NULL,
            author TEXT, created_at TEXT, updated_at TEXT, synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );

        -- The shared PR exists under BOTH repos (the post-V044 state); its issue mirror too.
        INSERT INTO github_pull_requests(owner, repo, number, html_url, state, title, body, \
         synced_at_ms, repo_id)
        VALUES ('o','r',1,'u','open','t','b',0,'repo-a'), \
         ('o','r',1,'u','open','t','b',0,'repo-b');
        INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, synced_at_ms, \
         repo_id)
        VALUES ('o','r',1,'u','open','t','b',0,'repo-a');

        -- One comment / review / review-comment, all stamped by the LAST syncer (repo-b).
        INSERT INTO github_comments(id, owner, repo, number, html_url, body, synced_at_ms, repo_id)
        VALUES (10, 'o', 'r', 1, 'u', 'cbody', 0, 'repo-b');
        INSERT INTO github_reviews(id, owner, repo, number, state, body, synced_at_ms, repo_id)
        VALUES (20, 'o', 'r', 1, 'ok', 'rbody', 0, 'repo-b');
        INSERT INTO github_review_comments(id, owner, repo, number, html_url, body, synced_at_ms, \
         repo_id)
        VALUES (30, 'o', 'r', 1, 'u', 'rcbody', 0, 'repo-b');

        -- An ORPHAN comment: no cached parent anywhere — must survive under its own repo_id.
        INSERT INTO github_comments(id, owner, repo, number, html_url, body, synced_at_ms, repo_id)
        VALUES (99, 'o', 'r', 42, 'u', 'orphan', 0, 'repo-x');

        -- The standalone FTS mirror in its LAST-SYNCER state: the comment is findable only from
        -- repo-b. V045 must re-derive it in-migration, or repo-a's scoped rationale/papertrail
        -- search still cannot see the duplicated row until some later sync rebuilds the mirror.
        CREATE VIRTUAL TABLE github_fts USING fts5(
            owner, repo, number UNINDEXED, item_kind UNINDEXED, item_id UNINDEXED,
            url UNINDEXED, title, body, classification, repo_id UNINDEXED, tokenize='porter'
        );
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        VALUES ('o', 'r', 1, 'comment', '10', 'u', '', 'cbody', 'decision', 'repo-b');
        ",
    )
    .unwrap();

    schema::apply_github_child_key_widening(&conn).unwrap();

    // The shared children now exist once per OWNING repo.
    for (table, id) in
        [("github_comments", 10i64), ("github_reviews", 20), ("github_review_comments", 30)]
    {
        let owners: Vec<String> = {
            let mut stmt = conn
                .prepare(&format!("SELECT repo_id FROM {table} WHERE id = {id} ORDER BY repo_id"))
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        assert_eq!(
            owners,
            vec!["repo-a".to_string(), "repo-b".to_string()],
            "{table} row {id} duplicated per owning-parent repo",
        );
    }
    // The orphan survives exactly once, under its own stamped repo.
    let (orphan_rows, orphan_repo): (i64, String) = conn
        .query_row("SELECT COUNT(*), MAX(repo_id) FROM github_comments WHERE id = 99", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((orphan_rows, orphan_repo.as_str()), (1, "repo-x"), "orphan kept, not dropped");

    // The FTS mirror was re-derived IN-MIGRATION: the widened comment is scoped-searchable from
    // BOTH owning repos immediately (no sync required), and the Rust-derived `classification`
    // label was carried from the old mirror by (item_kind, item_id).
    for repo in ["repo-a", "repo-b"] {
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM github_fts WHERE github_fts MATCH 'cbody' AND repo_id = ?1",
                [repo],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "{repo}: the duplicated comment is FTS-findable without a sync");
    }
    let carried_class: String = conn
        .query_row(
            "SELECT classification FROM github_fts WHERE item_kind = 'comment' AND item_id = '10' \
             AND repo_id = 'repo-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(carried_class, "decision", "the classification label carried across the rebuild");
}

/// The end-to-end oscillation regression (Codex batch 6): two repos sync the SAME external PR's
/// comment — both scoped papertrails keep it, and a re-sync by either repo replaces ITS OWN copy
/// in place without evicting the sibling's (pre-V045, `INSERT OR REPLACE` on the bare id
/// restamped the single row to the last syncer).
#[test]
fn both_repos_keep_a_shared_prs_comments_across_syncs() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    let sync_as = |repo_id: &str, body: &str| {
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [repo_id],
        )
        .unwrap();
        crate::index::papertrail::store_comment(&conn, &crate::index::papertrail::GitHubComment {
            id: 7,
            owner: "o".into(),
            repo: "r".into(),
            number: 1,
            html_url: "http://c".into(),
            body: body.into(),
            author: None,
            created_at: None,
            updated_at: None,
        })
        .unwrap();
    };

    sync_as("repo-a", "from-a");
    sync_as("repo-b", "from-b");
    // Repo A re-syncs with fresh content: replaces ITS copy, never B's.
    sync_as("repo-a", "from-a-v2");

    let body_for = |repo: &str| -> String {
        conn.query_row(
            "SELECT body FROM github_comments WHERE id = 7 AND repo_id = ?1",
            [repo],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(body_for("repo-a"), "from-a-v2", "A's re-sync refreshed A's own copy");
    assert_eq!(body_for("repo-b"), "from-b", "B's copy survived A's re-sync — no oscillation");
}
