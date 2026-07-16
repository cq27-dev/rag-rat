use super::*;

#[test]
fn configless_discovery_keeps_self_contained_github_refs_from_files_and_commits() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("docs")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);
    fs::write(root.join("docs/search.md"), "File rationale cq27-dev/rag-rat#7\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Commit rationale cq27-dev/rag-rat#8"]);

    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();
    let ctx = papertrail::PapertrailContext::default();
    let report = sync_from_refs_blocking::<MockGitHubClient>(
        db.storage.connection(),
        &root,
        None,
        true,
        &ctx,
    )
    .unwrap();
    assert_eq!(report.discovered_refs, 2);

    let refs = papertrail::refs(db.storage.connection()).unwrap();
    for (source_kind, key) in [("file", "7"), ("commit", "8")] {
        assert!(refs.iter().any(|reference| {
            reference.tracker == papertrail::Tracker::Github
                && reference.project == "cq27-dev/rag-rat"
                && reference.item_key == key
                && reference.source_kind == source_kind
        }));
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn manual_sync_validates_client_and_routes_only_the_requested_github_identity() {
    let (root, config) = markdown_config("GitLab group/repo#42\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let ctx = test_gh_ctx();

    let invalid = papertrail::block_on(papertrail::sync_issue::<MockGitHubClient>(
        db.storage.connection(),
        "not-a-ref",
        None,
        false,
        &ctx,
    ))
    .unwrap_err()
    .to_string();
    assert!(invalid.contains("invalid tracker item reference"), "{invalid}");

    let missing_client = papertrail::block_on(papertrail::sync_issue::<MockGitHubClient>(
        db.storage.connection(),
        "cq27-dev/rag-rat#42",
        None,
        false,
        &ctx,
    ))
    .unwrap_err()
    .to_string();
    assert!(missing_client.contains("requires a client"), "{missing_client}");

    papertrail::store_ref(db.storage.connection(), &papertrail::PapertrailRef {
        tracker: papertrail::Tracker::Gitlab,
        project: "group/repo".to_string(),
        item_kind: Some(papertrail::ItemKind::Issue),
        item_key: "42".to_string(),
        ref_kind: "reference".to_string(),
        source_kind: "manual".to_string(),
        source_path: None,
        source_commit: None,
        source_text: "group/repo#42".to_string(),
    })
    .unwrap();

    let live = papertrail::block_on(papertrail::sync_issue(
        db.storage.connection(),
        "cq27-dev/rag-rat#42",
        Some(&MockGitHubClient),
        false,
        &ctx,
    ))
    .unwrap();
    assert_eq!(live.synced_items, 5);
    assert_eq!(live.failed_refs, 0);

    let gitlab_only_ctx = papertrail::PapertrailContext {
        trackers: vec![papertrail::ResolvedTracker {
            provider: papertrail::Tracker::Gitlab,
            project: "group/repo".to_string(),
            base_url: None,
            auth: None,
            authentication: papertrail::TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        }],
        ..papertrail::PapertrailContext::default()
    };
    let explicit_without_github_binding = papertrail::block_on(papertrail::sync_issue(
        db.storage.connection(),
        "cq27-dev/rag-rat#43",
        Some(&MockGitHubClient),
        false,
        &gitlab_only_ctx,
    ))
    .unwrap();
    assert_eq!(explicit_without_github_binding.synced_items, 5);
    assert_eq!(explicit_without_github_binding.failed_refs, 0);

    let offline = papertrail::block_on(papertrail::sync_issue::<MockGitHubClient>(
        db.storage.connection(),
        "cq27-dev/rag-rat#43",
        None,
        true,
        &ctx,
    ))
    .unwrap();
    assert!(offline.offline);
    assert_eq!(offline.synced_items, 0);

    let gitlab_cached: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM papertrail_items WHERE tracker = 'gitlab' AND project = \
             'group/repo'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(gitlab_cached, 0, "manual GitHub sync must not dispatch the same-key GitLab ref");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn rationale_lookup_keeps_self_contained_github_refs_without_a_binding() {
    let (root, config) = markdown_config("# Decision\nalpha\n");
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    let reference = papertrail::PapertrailRef {
        tracker: papertrail::Tracker::Github,
        project: "cq27-dev/rag-rat".to_string(),
        item_kind: None,
        item_key: "42".to_string(),
        ref_kind: "reference".to_string(),
        source_kind: "manual".to_string(),
        source_path: None,
        source_commit: None,
        source_text: "cq27-dev/rag-rat#42".to_string(),
    };
    papertrail::store_ref(db.storage.connection(), &reference).unwrap();
    papertrail::block_on(papertrail::sync_refs(
        db.storage.connection(),
        &MockGitHubClient,
        std::iter::once(&reference),
        &mut |_| {},
    ))
    .unwrap();
    db.set_papertrail_context(None);

    let evidence = db.rationale_search("cq27-dev/rag-rat#42", 10).unwrap();
    assert!(
        evidence.iter().any(|item| item.project == "cq27-dev/rag-rat" && item.item_key == "42")
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn production_discovery_persists_refs_for_every_configured_tracker() {
    let (root, config) = markdown_config(
        "# Links\nGitLab group/sub/repo#7 and group/sub/repo!7 plus Jira PROJ-42 annotate this \
         file.\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    let ctx = papertrail::PapertrailContext {
        trackers: vec![
            papertrail::ResolvedTracker {
                provider: papertrail::Tracker::Gitlab,
                project: "group/sub/repo".to_string(),
                base_url: None,
                auth: Some(rag_rat_base::config::TrackerAuth::Env("GITLAB_TOKEN".to_string())),
                authentication: papertrail::TrackerAuthentication::AuthConfigured,
                tags: Vec::new(),
            },
            papertrail::ResolvedTracker {
                provider: papertrail::Tracker::Jira,
                project: "PROJ".to_string(),
                base_url: Some("https://example.atlassian.net".to_string()),
                auth: None,
                authentication: papertrail::TrackerAuthentication::AuthMissing,
                tags: Vec::new(),
            },
        ],
        ..papertrail::PapertrailContext::default()
    };

    sync_from_refs_blocking(db.storage.connection(), &root, Some(&MockGitHubClient), true, &ctx)
        .unwrap();

    let status = papertrail::status(db.storage.connection(), &ctx).unwrap();
    assert_eq!(status.capabilities.len(), 2);
    assert_eq!(
        status.capabilities[0].authentication,
        papertrail::TrackerAuthentication::AuthConfigured
    );
    assert_eq!(status.capabilities[0].synchronization, papertrail::TrackerSynchronization::Native);
    assert_eq!(
        status.capabilities[1].authentication,
        papertrail::TrackerAuthentication::AuthMissing
    );

    let refs = db.papertrail_refs_for_path("docs/search.md", 10).unwrap();
    assert!(refs.iter().any(|reference| {
        reference.tracker == papertrail::Tracker::Gitlab
            && reference.project == "group/sub/repo"
            && reference.item_key == "7"
            && reference.item_kind == Some(papertrail::ItemKind::Issue)
    }));
    assert!(refs.iter().any(|reference| {
        reference.tracker == papertrail::Tracker::Gitlab
            && reference.project == "group/sub/repo"
            && reference.item_key == "7"
            && reference.item_kind == Some(papertrail::ItemKind::ChangeRequest)
    }));
    assert!(refs.iter().any(|reference| {
        reference.tracker == papertrail::Tracker::Jira
            && reference.project == "PROJ"
            && reference.item_key == "PROJ-42"
    }));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn papertrail_for_commit_prefers_commit_sourced_tracker_refs() {
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
    sync_from_refs_blocking(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
        .unwrap();

    let papertrail = db.papertrail_for_commit(&commit[..7], 10).unwrap();
    assert_eq!(papertrail.evidence.first().map(|item| item.item_key.as_str()), Some("42"));
    assert_eq!(
        papertrail.evidence.first().map(|item| item.evidence_kind),
        Some("literal_tracker_ref")
    );
    assert!(
        papertrail.fallback_evidence.is_empty(),
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
    sync_from_refs_blocking(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
        .unwrap();
    let papertrail = db
        .papertrail_for_symbol("tracked_symbol", Some(Language::Rust), 10)
        .unwrap()
        .expect("tracked symbol papertrail");

    assert_eq!(
        papertrail
            .evidence
            .iter()
            .filter(|item| item.item_key == "42" && item.doc_kind == "item")
            .count(),
        1,
        "duplicate #42 refs in one file should collapse to one item evidence row: {papertrail:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn papertrail_sync_keeps_the_synced_item_and_retries_a_404_ref() {
    let (root, config) = markdown_config(
        "# Decision\nRefs cq27-dev/rag-rat#42 and cq27-dev/rag-rat#404\nwe will keep sqlite\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    let mock = PartiallyFailingGitHubClient;

    let report =
        sync_from_refs_blocking(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
            .unwrap();
    assert_eq!(report.discovered_refs, 2);
    // The mock change request stores ONE item row (no issue-shadow duplication in the
    // provider-neutral schema) plus its 4 comments.
    assert_eq!(report.synced_items, 5);
    assert_eq!(report.failed_refs, 1);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].item_key, "404");
    assert_eq!(report.errors[0].status, "not_found");

    let issue_hits = db.papertrail_issue_search("sqlite", 10).unwrap();
    assert_eq!(issue_hits.len(), 1);
    assert_eq!(issue_hits[0].item_key, "42");

    // The per-ref state machine is gone: the synced item skips via its cached row, while the 404
    // ref has no cached item and RETRIES (and fails again) on every sync — no not_found memo.
    let second =
        sync_from_refs_blocking(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
            .unwrap();
    assert_eq!(second.synced_items, 0);
    assert_eq!(second.skipped_refs, 1);
    assert_eq!(second.failed_refs, 1);

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
        trackers: Vec::new(),
        papertrail: Default::default(),
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

// --- V060: provider-neutral papertrail schema ---

/// Fresh `schema::apply` produces the provider-neutral papertrail tables DIRECTLY (the baseline
/// creates them; no legacy github_* tables are ever created), and re-apply is idempotent.
#[test]
fn v060_creates_the_papertrail_tables_on_fresh_apply() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "fresh apply reaches the current schema tip",
    );
    for table in [
        "papertrail_items",
        "papertrail_comments",
        "papertrail_refs",
        "papertrail_sync_cursor",
        "papertrail_item_tags",
        "papertrail_fts",
    ] {
        assert!(conn_table_exists(&conn, table), "{table} exists on a fresh apply");
    }
    // HARD RENAME: no legacy github_* table (or the V009-created ref-sync residue) survives a
    // fresh apply.
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
        assert!(!conn_table_exists(&conn, table), "legacy {table} must not exist");
    }
    assert!(conn_index_exists(&conn, "idx_papertrail_items_natural_key"), "items natural key");
    assert!(
        conn_index_exists(&conn, "idx_papertrail_comments_natural_key"),
        "comments natural key"
    );
    let cursor_columns = conn_table_columns(&conn, "papertrail_sync_cursor");
    assert!(cursor_columns.contains(&"comment_high_mark_at".to_string()));
    assert!(cursor_columns.contains(&"comment_page_token".to_string()));
    schema::apply(&conn).unwrap();
    assert!(conn_table_exists(&conn, "papertrail_items"), "survives re-apply");

    // A V059 ledger advances through both papertrail steps and reaches the current tip.
    truncate_schema_to(&conn, 59);
    schema::migrate_forward(&conn).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
}

#[test]
fn migration_063_persists_mirror_resume_state() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    let columns = conn_table_columns(&conn, "papertrail_sync_cursor");
    assert!(columns.contains(&"comment_high_mark_at".to_string()));
    assert!(columns.contains(&"comment_page_token".to_string()));
    for column in [
        "comment_scan_since",
        "comment_stream_cursors",
        "item_delta_page_token",
        "item_delta_scan_since",
        "item_delta_high_mark_at",
        "backfill_page_cursor",
        "item_thread_cursor",
        "item_delta_in_progress",
        "item_delta_replay_required",
        "delta_processed_keys",
        "backfill_processed_keys",
        "full_rewalk",
    ] {
        assert!(columns.contains(&column.to_string()), "cursor has {column}");
    }
    assert!(
        conn_table_columns(&conn, "papertrail_items").contains(&"full_rewalk_seen".to_string())
    );
    schema::apply_papertrail_mirror_resume_state(&conn).unwrap();
    assert_eq!(conn_table_columns(&conn, "papertrail_sync_cursor"), columns, "V063 is idempotent");
}

#[test]
fn migration_067_persists_binding_health() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    let columns = conn_table_columns(&conn, "papertrail_sync_cursor");
    for name in [
        "last_attempt_ms",
        "last_successful_probe_ms",
        "last_successful_mirror_ms",
        "retry_not_before_ms",
        "error_class",
        "error_detail",
    ] {
        assert!(columns.contains(&name.to_string()), "missing {name}");
    }
    conn.execute(
        "INSERT INTO papertrail_sync_cursor(
             tracker, project, last_probe_ms, backfill_done, repo_id
         ) VALUES ('github', 'o/complete', 1234, 1, '__unassigned__'),
                  ('github', 'o/incomplete', 5678, 0, '__unassigned__')",
        [],
    )
    .unwrap();
    schema::apply_papertrail_binding_health(&conn).unwrap();
    let (probe, mirror, full): (Option<i64>, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT last_successful_probe_ms, last_successful_mirror_ms, last_full_sync_ms
             FROM papertrail_sync_cursor WHERE project='o/complete'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(probe, Some(1234));
    assert_eq!(mirror, Some(1234));
    assert_eq!(full, Some(1234));
    let incomplete_full: Option<i64> = conn
        .query_row(
            "SELECT last_full_sync_ms FROM papertrail_sync_cursor WHERE project='o/incomplete'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(incomplete_full, None);
}

#[test]
fn migration_063_checksum_replays_the_pre_replay_flag_shape() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn.execute("ALTER TABLE papertrail_sync_cursor DROP COLUMN item_delta_replay_required", [])
        .unwrap();
    conn.execute(
        "UPDATE schema_version
         SET checksum = 'sha256:rag-rat-papertrail-mirror-resume-state-v63d'
         WHERE id = '063_papertrail_mirror_resume_state'",
        [],
    )
    .unwrap();

    schema::apply(&conn).unwrap();

    assert!(
        conn_table_columns(&conn, "papertrail_sync_cursor")
            .contains(&"item_delta_replay_required".to_string())
    );
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Compatible);
}

/// The load-bearing multi-repo invariant carried over from V044/V045: two repos can each cache
/// the SAME external item/comment — the natural keys lead with `repo_id` — while a same-repo
/// duplicate is still rejected (what the writers' `ON CONFLICT` relies on). `item_kind` is part
/// of the item identity: the same key may exist under BOTH kinds within one repo (namespaced
/// providers), but not twice under one kind.
#[test]
fn v060_keys_let_two_repos_cache_the_same_item_and_fold_item_kind() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    let insert_item = |repo_id: &str, kind: &str| {
        conn.execute(
            "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, \
             title, body, synced_at_ms, repo_id)
             VALUES ('github', 'o/r', ?1, '7', 'u', 'open', 't', 'b', 0, ?2)",
            [kind, repo_id],
        )
    };
    let insert_comment = |repo_id: &str| {
        conn.execute(
            "INSERT INTO papertrail_comments(tracker, project, item_kind, item_key, comment_id, \
             url, body, synced_at_ms, repo_id)
             VALUES ('github', 'o/r', 'issue', '7', '9', 'u', 'b', 0, ?1)",
            [repo_id],
        )
    };
    for repo in ["repo-a", "repo-b"] {
        insert_item(repo, "issue").unwrap();
        insert_comment(repo).unwrap();
    }
    // Kind folds into the identity: the same key under the OTHER kind coexists within one repo.
    insert_item("repo-a", "change_request").unwrap();
    let items: i64 = conn
        .query_row("SELECT COUNT(*) FROM papertrail_items WHERE item_key = '7'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(items, 3, "two repos' issue copies + one change-request twin coexist");
    assert!(insert_item("repo-a", "issue").is_err(), "same-repo same-kind duplicate rejected");
    assert!(insert_comment("repo-a").is_err(), "same-repo duplicate comment id rejected");
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
        r#"
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
        INSERT INTO github_pull_requests(owner, repo, number, html_url, state, title, body,
         synced_at_ms, repo_id)
        VALUES ('o','r',1,'u','open','t','b',0,'repo-a'),
         ('o','r',1,'u','open','t','b',0,'repo-b');
        INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, synced_at_ms,
         repo_id)
        VALUES ('o','r',1,'u','open','t','b',0,'repo-a');

        -- One comment / review / review-comment, all stamped by the LAST syncer (repo-b).
        INSERT INTO github_comments(id, owner, repo, number, html_url, body, synced_at_ms, repo_id)
        VALUES (10, 'o', 'r', 1, 'u', 'cbody', 0, 'repo-b');
        INSERT INTO github_reviews(id, owner, repo, number, state, body, synced_at_ms, repo_id)
        VALUES (20, 'o', 'r', 1, 'ok', 'rbody', 0, 'repo-b');
        INSERT INTO github_review_comments(id, owner, repo, number, html_url, body, synced_at_ms,
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
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body,
         classification, repo_id)
        VALUES ('o', 'r', 1, 'comment', '10', 'u', '', 'cbody', 'decision', 'repo-b');
        "#,
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

/// The end-to-end oscillation regression (Codex batch 6, carried onto the provider-neutral
/// schema): two repos sync the SAME external item's comment — both scoped papertrails keep it,
/// and a re-sync by either repo replaces ITS OWN copy in place without evicting the sibling's.
#[test]
fn both_repos_keep_a_shared_items_comments_across_syncs() {
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
        crate::index::papertrail::store_comment(
            &conn,
            crate::index::papertrail::Tracker::Github,
            &crate::index::papertrail::PapertrailComment {
                project: "o/r".into(),
                item_kind: crate::index::papertrail::ItemKind::Issue,
                item_key: "1".into(),
                comment_id: "7".into(),
                url: Some("http://c".into()),
                body: body.into(),
                author: None,
                created_at: None,
                updated_at: None,
                review_state: None,
                anchor_path: None,
            },
        )
        .unwrap();
    };

    sync_as("repo-a", "from-a");
    sync_as("repo-b", "from-b");
    // Repo A re-syncs with fresh content: replaces ITS copy, never B's.
    sync_as("repo-a", "from-a-v2");

    let body_for = |repo: &str| -> String {
        conn.query_row(
            "SELECT body FROM papertrail_comments WHERE comment_id = '7' AND repo_id = ?1",
            [repo],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(body_for("repo-a"), "from-a-v2", "A's re-sync refreshed A's own copy");
    assert_eq!(body_for("repo-b"), "from-b", "B's copy survived A's re-sync — no oscillation");
    // The incremental FTS mirror followed the same ownership: one comment row per repo, each with
    // its own body.
    let fts_body_for = |repo: &str| -> String {
        conn.query_row(
            "SELECT body FROM papertrail_fts WHERE comment_id = '7' AND repo_id = ?1",
            [repo],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(fts_body_for("repo-a"), "from-a-v2");
    assert_eq!(fts_body_for("repo-b"), "from-b");
}

/// A client whose comment fetch fails ONCE with a non-404 error (rate limit) and then recovers —
/// the shape that leaves a partial cache behind: the item row stored, the comments missing.
struct RecoveringCommentsClient {
    failed_once: std::cell::Cell<bool>,
}

impl papertrail::PapertrailClient for RecoveringCommentsClient {
    async fn item(
        &self,
        project: &str,
        kind: papertrail::ItemKind,
        key: &str,
    ) -> anyhow::Result<papertrail::PapertrailItem> {
        MockGitHubClient.item(project, kind, key).await
    }

    async fn item_comments(
        &self,
        project: &str,
        kind: papertrail::ItemKind,
        key: &str,
    ) -> anyhow::Result<Vec<papertrail::PapertrailComment>> {
        if !self.failed_once.replace(true) {
            anyhow::bail!("gh: HTTP 429 rate limited");
        }
        MockGitHubClient.item_comments(project, kind, key).await
    }

    async fn items_page(
        &self,
        project: &str,
        cursor: &papertrail::PageCursor,
    ) -> anyhow::Result<papertrail::ItemsPage> {
        MockGitHubClient.items_page(project, cursor).await
    }

    async fn comments_page(
        &self,
        project: &str,
        cursor: &papertrail::PageCursor,
    ) -> anyhow::Result<papertrail::CommentsPage> {
        MockGitHubClient.comments_page(project, cursor).await
    }

    async fn freshness_probe(
        &self,
        project: &str,
        probe: &papertrail::FreshnessProbe,
    ) -> anyhow::Result<papertrail::FreshnessResult> {
        MockGitHubClient.freshness_probe(project, probe).await
    }
}

#[test]
fn papertrail_sync_retries_a_failed_ref_instead_of_caching_a_partial_item() {
    let (root, config) =
        markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let client = RecoveringCommentsClient { failed_once: std::cell::Cell::new(false) };

    // First pass: the comment fetch fails. With the per-ref state machine gone, the cached item
    // is the ONLY skip signal — so the sync stores NOTHING on a partial failure (fetch-then-store
    // ordering), keeping the ref retryable.
    let first = sync_from_refs_blocking(
        db.storage.connection(),
        &root,
        Some(&client),
        false,
        &test_gh_ctx(),
    )
    .unwrap();
    assert_eq!(first.failed_refs, 1);
    assert_eq!(first.errors[0].status, "failed");
    assert_eq!(
        first.status.change_requests, 0,
        "a partial sync must cache nothing — a cached item would masquerade as a completed sync \
         and skip the ref with its comments missing forever"
    );
    assert_eq!(first.status.comments, 0);

    // Second pass: the ref retries and completes — one item + its 4 unified comments.
    let second = sync_from_refs_blocking(
        db.storage.connection(),
        &root,
        Some(&client),
        false,
        &test_gh_ctx(),
    )
    .unwrap();
    assert_eq!(second.skipped_refs, 0, "a failed ref must retry, not trust a partial cache");
    assert_eq!(second.failed_refs, 0);
    assert_eq!(second.synced_items, 5);
    assert_eq!(second.status.change_requests, 1);
    assert_eq!(second.status.comments, 4);

    // Third pass: now genuinely synced — skipped via the cached item.
    let third = sync_from_refs_blocking(
        db.storage.connection(),
        &root,
        Some(&client),
        false,
        &test_gh_ctx(),
    )
    .unwrap();
    assert_eq!(third.skipped_refs, 1);
    assert_eq!(third.synced_items, 0);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn papertrail_sync_rolls_back_the_item_when_a_comment_store_fails() {
    let (root, config) =
        markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    conn.execute_batch(
        "CREATE TRIGGER reject_papertrail_comment
         BEFORE INSERT ON papertrail_comments
         BEGIN SELECT RAISE(ABORT, 'injected comment failure'); END;",
    )
    .unwrap();

    let failed =
        sync_from_refs_blocking(conn, &root, Some(&MockGitHubClient), false, &test_gh_ctx())
            .unwrap();
    assert_eq!(failed.failed_refs, 1);
    assert_eq!(failed.status.change_requests, 0, "the item completion marker rolls back");
    assert_eq!(failed.status.comments, 0);

    conn.execute_batch("DROP TRIGGER reject_papertrail_comment;").unwrap();
    let retried =
        sync_from_refs_blocking(conn, &root, Some(&MockGitHubClient), false, &test_gh_ctx())
            .unwrap();
    assert_eq!(retried.failed_refs, 0);
    assert_eq!(retried.status.change_requests, 1);
    assert_eq!(retried.status.comments, 4);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn papertrail_client_batch_surface_round_trips_through_the_trait() {
    use crate::index::papertrail::PapertrailClient;

    let cursor = papertrail::PageCursor::default();
    let items = papertrail::block_on(MockGitHubClient.items_page("o/r", &cursor)).unwrap();
    assert_eq!(items.items.len(), 1);
    assert_eq!(items.items[0].item_kind, papertrail::ItemKind::ChangeRequest);

    let comments = papertrail::block_on(MockGitHubClient.comments_page("o/r", &cursor)).unwrap();
    assert_eq!(comments.comments.len(), 4);
    assert_eq!(comments.comments.iter().filter(|c| c.review_state.is_some()).count(), 1);
    assert_eq!(comments.comments.iter().filter(|c| c.anchor_path.is_some()).count(), 1);

    let moved = papertrail::block_on(
        MockGitHubClient.freshness_probe("o/r", &papertrail::FreshnessProbe::default()),
    )
    .unwrap();
    assert_eq!(moved.latest.as_deref(), Some("2026-01-02T00:00:00Z"));
    let quiet_probe = papertrail::FreshnessProbe {
        updated_since: Some("2026-01-02T00:00:00Z".into()),
        etag: None,
    };
    let quiet =
        papertrail::block_on(MockGitHubClient.freshness_probe("o/r", &quiet_probe)).unwrap();
    assert_eq!(quiet.latest, None, "a probe at the cursor position reports no movement");
}

// --- V060: the github_* -> papertrail_* backfill against a POPULATED pre-migration DB ---

/// Build the exact V045-state legacy shape (the seven github_* tables + github_fts +
/// old-shape repo_memory_bindings + repo_meta), populated: a plain issue, a PR with a shadow
/// issue row / review / review comment / thread comment, refs, a ref-sync row, a sibling repo's
/// issue, and a `github`-kind memory binding.
fn seeded_pre_v060_legacy_db() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "
        CREATE TABLE github_refs(
            id INTEGER PRIMARY KEY AUTOINCREMENT, owner TEXT           NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, ref_kind TEXT NOT NULL DEFAULT           'unknown',
            source_kind TEXT NOT NULL, source_path TEXT, source_commit TEXT,
                      source_text TEXT NOT NULL, discovered_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT           NULL DEFAULT '__unassigned__'
        );
        CREATE TABLE github_issues(
            id INTEGER           PRIMARY KEY AUTOINCREMENT, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER           NOT NULL, html_url TEXT NOT NULL, state TEXT NOT NULL,
            title TEXT NOT NULL, body           TEXT NOT NULL, author TEXT, created_at TEXT,
            updated_at TEXT, is_pull_request INTEGER           NOT NULL DEFAULT 0,
            synced_at_ms INTEGER NOT NULL, repo_id TEXT NOT NULL DEFAULT           '__unassigned__'
        );
        CREATE TABLE github_comments(
            id INTEGER PRIMARY           KEY, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, html_url           TEXT NOT NULL, body TEXT NOT NULL, author TEXT,
            created_at TEXT, updated_at TEXT,           synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
                  );
        CREATE TABLE github_pull_requests(
            id INTEGER PRIMARY KEY AUTOINCREMENT,           owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, html_url TEXT           NOT NULL, state TEXT NOT NULL,
            title TEXT NOT NULL, body TEXT NOT NULL, author           TEXT, created_at TEXT,
            updated_at TEXT, merged_at TEXT, synced_at_ms INTEGER NOT           NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        CREATE TABLE           github_reviews(
            id INTEGER PRIMARY KEY, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number INTEGER NOT NULL, html_url TEXT, state TEXT NOT NULL, body TEXT NOT NULL,
            author TEXT, submitted_at TEXT, synced_at_ms INTEGER NOT NULL,
            repo_id           TEXT NOT NULL DEFAULT '__unassigned__'
        );
        CREATE TABLE github_review_comments(
            id INTEGER PRIMARY KEY, owner TEXT NOT NULL, repo TEXT NOT NULL,
            number           INTEGER NOT NULL, path TEXT, html_url TEXT NOT NULL, body TEXT NOT NULL,
            author           TEXT, created_at TEXT, updated_at TEXT, synced_at_ms INTEGER NOT NULL,
            repo_id           TEXT NOT NULL DEFAULT '__unassigned__'
        );
        CREATE TABLE github_ref_sync(
                      owner TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT NULL,
            status TEXT NOT           NULL, synced_at_ms INTEGER NOT NULL, last_error TEXT,
            repo_id TEXT NOT NULL DEFAULT           '__unassigned__',
            PRIMARY KEY(repo_id, owner, repo, number)
        );
        CREATE           VIRTUAL TABLE github_fts USING fts5(
            owner, repo, number UNINDEXED, item_kind UNINDEXED,           item_id UNINDEXED,
            url UNINDEXED, title, body, classification, repo_id UNINDEXED,           tokenize='porter'
        );
        CREATE TABLE repo_memories(id TEXT PRIMARY KEY);
                  CREATE TABLE repo_memory_bindings(
            memory_id TEXT NOT NULL, binding_kind TEXT NOT           NULL, binding_id TEXT NOT NULL,
            path TEXT, start_line INTEGER, end_line INTEGER,           logical_symbol_id INTEGER,
            symbol_id INTEGER, chunk_id INTEGER, edge_id INTEGER,           commit_hash TEXT,
            github_owner TEXT, github_repo TEXT, github_number INTEGER,
                      anchor_status TEXT NOT NULL, created_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL           DEFAULT '__unassigned__',
            PRIMARY KEY(memory_id, binding_kind, binding_id)
                  );
        CREATE TABLE repo_meta(
            repo_id TEXT NOT NULL, key TEXT NOT NULL, value           TEXT NOT NULL,
            PRIMARY KEY(repo_id, key)
        );

        -- repo-a: a plain issue           #1, a merged PR #2 (pulls row + its issues-endpoint shadow), a
        -- thread comment on           each, a review + a file-anchored review comment on the PR, a file
        -- ref to #1, and           a ref-sync state row (deleted by the migration, not carried).
        INSERT INTO github_issues(owner,           repo, number, html_url, state, title, body, author,
                                  is_pull_request,           synced_at_ms, repo_id)
        VALUES ('o','r',1,'http://i1','open','issue one','sqlite stays','alice',0,11,'repo-a'),
               ('o','r',2,'http://p2','closed','pr two','shadow body','bob',1,12,'repo-a'),
               ('o','r',3,'http://i3','open','partial issue','must retry','eve',0,19,'repo-a');
        INSERT INTO github_pull_requests(owner, repo, number, html_url, state, title, body,
                                         author, merged_at, synced_at_ms, repo_id)
                  VALUES ('o','r',2,'http://p2','closed','pr two','pull body','bob',
                '2026-01-05T00:00:00Z',12,'repo-a');
        INSERT INTO github_comments(id, owner, repo, number, html_url, body, author,
                                              synced_at_ms, repo_id)
        VALUES (10,'o','r',1,'http://c10','issue thread comment','carol',13,'repo-a'),
               (11,'o','r',2,'http://c11','pr thread comment','carol',13,'repo-a'),
               (12,'o','r',3,'http://c12','partial thread comment','eve',19,'repo-a');
                  INSERT INTO github_reviews(id, owner, repo, number, html_url, state, body, author,
                                             submitted_at, synced_at_ms, repo_id)
        VALUES (10,'o','r',2,NULL,'APPROVED','ship it','dave','2026-01-04T00:00:00Z',14,'repo-a');
        INSERT INTO github_review_comments(id, owner, repo, number, path, html_url, body,           author,
                                           synced_at_ms, repo_id)
        VALUES (10,'o','r',2,'src/lib.rs','http://rc10','anchored           nit','dave',15,'repo-a');
        INSERT INTO github_refs(owner, repo, number, ref_kind, source_kind,           source_path,
                                source_text, discovered_at_ms, repo_id)
                  VALUES ('o','r',1,'closing','file','docs/a.md','Fixes o/r#1',16,'repo-a');
        INSERT INTO           github_ref_sync(owner, repo, number, status, synced_at_ms, repo_id)
        VALUES ('o','r',1,'synced',17,'repo-a'),
               ('o','r',3,'failed',19,'repo-a');

        -- repo-b: its own copy of an external issue — repo_id must copy VERBATIM.
                  INSERT INTO github_issues(owner, repo, number, html_url, state, title, body,
                                            is_pull_request, synced_at_ms, repo_id)
        VALUES ('o','r',1,'http://i1','open','issue           one','sqlite stays',0,18,'repo-b');

        -- The old mirror (stale-ish content is fine: the           migration re-derives it wholesale).
        INSERT INTO github_fts(owner, repo, number, item_kind,           item_id, url, title, body,
                               classification, repo_id)
        VALUES           ('o','r',1,'issue','1','http://i1','issue one','sqlite stays','context','repo-a');

        -- A github-kind memory binding (old columns) + a path binding that must pass through
        -- untouched.
        INSERT INTO repo_memories(id) VALUES ('m1'), ('m2');
        INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, github_owner,
                                         github_repo, github_number, anchor_status,
                                         created_at_ms, repo_id)
        VALUES ('m1','github','o/r#7','o','r',7,'unverified',0,'repo-a');
        INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path,
                                         anchor_status, created_at_ms, repo_id)
        VALUES ('m2','path','src/x.rs','src/x.rs','current',0,'repo-a');

        INSERT INTO repo_meta(repo_id, key, value) VALUES ('repo-a','github_last_sync_ms','123');
        ",
    )
    .unwrap();
    conn
}

/// The V060 backfill end-to-end against the populated legacy fixture: items dedupe the PR shadow,
/// comments unify the three legacy shapes, refs copy verbatim, repo_id copies verbatim, the
/// legacy tables (including the ref-sync state machine) are DROPPED, the mirror is re-derived,
/// the memory binding hard-renames to the `tracker` kind, the meta key renames — and the scoped
/// papertrail readers work on the migrated data. Re-applying is a clean no-op.
#[test]
fn migration_060_backfills_papertrail_from_the_legacy_github_tables() {
    let conn = seeded_pre_v060_legacy_db();

    schema::apply_papertrail_provider_neutral_schema(&conn).unwrap();

    // Items: issue #1 (both repos, verbatim repo_id) + ONE change_request #2 (shadow deduped,
    // pulls copy wins so merged_at survives).
    let items: Vec<(String, String, String, Option<String>, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT item_kind, item_key, body, merged_at, repo_id FROM papertrail_items
                 ORDER BY repo_id, item_kind, item_key",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap();
        rows.map(Result::unwrap).collect()
    };
    assert_eq!(items, vec![
        (
            "change_request".to_string(),
            "2".to_string(),
            "pull body".to_string(),
            Some("2026-01-05T00:00:00Z".to_string()),
            "repo-a".to_string()
        ),
        (
            "issue".to_string(),
            "1".to_string(),
            "sqlite stays".to_string(),
            None,
            "repo-a".to_string()
        ),
        (
            "issue".to_string(),
            "1".to_string(),
            "sqlite stays".to_string(),
            None,
            "repo-b".to_string()
        ),
    ]);
    let projects: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM papertrail_items WHERE tracker = 'github' AND project = 'o/r'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(projects, 3, "tracker/project derive from the legacy owner/repo columns");

    let migrated_ref = |item_key: &str| papertrail::PapertrailRef {
        item_kind: None,
        tracker: papertrail::Tracker::Github,
        project: "o/r".to_string(),
        item_key: item_key.to_string(),
        ref_kind: "unknown".to_string(),
        source_kind: "file".to_string(),
        source_path: None,
        source_commit: None,
        source_text: String::new(),
    };
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);
         INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', 'repo-a');",
    )
    .unwrap();
    assert!(
        papertrail::papertrail_ref_synced(&conn, &migrated_ref("1")).unwrap(),
        "a successful legacy sync keeps its item and remains skippable"
    );
    assert!(
        !papertrail::papertrail_ref_synced(&conn, &migrated_ref("3")).unwrap(),
        "a failed legacy sync must remain retryable after its partial item is removed"
    );
    let failed_children: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM papertrail_comments
             WHERE repo_id = 'repo-a' AND project = 'o/r' AND item_key = '3'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failed_children, 0, "partial children from a failed sync are not exposed");

    // Comments: the three legacy shapes unified; the PR thread comment resolved its parent kind
    // through the cached parent rows.
    type MigratedCommentRow = (String, String, String, Option<String>, Option<String>);
    let comments: Vec<MigratedCommentRow> = {
        let mut stmt = conn
            .prepare(
                "SELECT comment_id, item_kind, item_key, review_state, anchor_path
                 FROM papertrail_comments ORDER BY comment_id",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap();
        rows.map(Result::unwrap).collect()
    };
    assert_eq!(comments, vec![
        ("comment:10".to_string(), "issue".to_string(), "1".to_string(), None, None),
        ("comment:11".to_string(), "change_request".to_string(), "2".to_string(), None, None),
        (
            "review:10".to_string(),
            "change_request".to_string(),
            "2".to_string(),
            Some("APPROVED".to_string()),
            None
        ),
        (
            "review_comment:10".to_string(),
            "change_request".to_string(),
            "2".to_string(),
            None,
            Some("src/lib.rs".to_string())
        ),
    ]);
    // The review's submitted_at fills both timestamps.
    let (created, updated): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT created_at, updated_at FROM papertrail_comments WHERE comment_id = 'review:10'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(created.as_deref(), Some("2026-01-04T00:00:00Z"));
    assert_eq!(updated, created);

    // Refs copied verbatim; the per-ref sync state machine is DELETED with its table, and the
    // cursor table starts empty.
    let (ref_kind, item_key): (String, String) = conn
        .query_row(
            "SELECT ref_kind, item_key FROM papertrail_refs WHERE source_path = 'docs/a.md'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((ref_kind.as_str(), item_key.as_str()), ("closing", "1"));
    let cursors: i64 =
        conn.query_row("SELECT COUNT(*) FROM papertrail_sync_cursor", [], |r| r.get(0)).unwrap();
    assert_eq!(cursors, 0, "the ref-sync state machine is deleted, not migrated to the cursor");

    // HARD RENAME: every legacy table is gone.
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
        assert!(!conn_table_exists(&conn, table), "legacy {table} must be dropped");
    }

    // The mirror was re-derived in-migration: scoped MATCHes serve the migrated cache
    // immediately, per repo, with the anchored comment's path in the title slot.
    let scoped_hits = |repo: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM papertrail_fts WHERE papertrail_fts MATCH 'sqlite' AND repo_id \
             = ?1",
            [repo],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(scoped_hits("repo-a"), 1);
    assert_eq!(scoped_hits("repo-b"), 1);
    let anchored_title: String = conn
        .query_row(
            "SELECT title FROM papertrail_fts WHERE comment_id = 'review_comment:10'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(anchored_title, "src/lib.rs");

    // The memory binding hard-renamed to the tracker kind; the path binding passed through.
    let (kind, id, tracker, project, item_key): (String, String, String, String, String) = conn
        .query_row(
            "SELECT binding_kind, binding_id, tracker, project, item_key FROM \
             repo_memory_bindings WHERE memory_id = 'm1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), id.as_str(), tracker.as_str(), project.as_str(), item_key.as_str()),
        ("tracker", "github:o/r#7", "github", "o/r", "7")
    );
    let github_kind_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM repo_memory_bindings WHERE binding_kind = 'github'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(github_kind_rows, 0, "the github binding kind ceases to exist");
    assert!(
        !conn_table_columns(&conn, "repo_memory_bindings").contains(&"github_owner".to_string()),
        "the legacy binding columns are dropped"
    );
    let path_binding: String = conn
        .query_row(
            "SELECT binding_kind FROM repo_memory_bindings WHERE memory_id = 'm2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(path_binding, "path");

    // The last-sync meta key renamed.
    let meta: String = conn
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = 'repo-a' AND key = \
             'papertrail_last_sync_ms'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(meta, "123");
    let old_meta: i64 = conn
        .query_row("SELECT COUNT(*) FROM repo_meta WHERE key = 'github_last_sync_ms'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(old_meta, 0);

    // Scoped readers work on the migrated data (the connection-context repo scope the production
    // reads resolve through).
    let hits = crate::index::papertrail::issue_search(&conn, "sqlite", 10).unwrap();
    assert_eq!(hits.len(), 1, "issue_search serves the migrated scoped cache");
    assert_eq!(hits[0].item_key, "1");
    let refs = crate::index::papertrail::refs_for_path(&conn, "docs/a.md", 10).unwrap();
    assert_eq!(refs.len(), 1, "refs_for_path serves the migrated scoped refs");
    assert_eq!(refs[0].item_key, "1");

    // Replay converges: a second apply is a clean no-op (nothing legacy left to backfill).
    schema::apply_papertrail_provider_neutral_schema(&conn).unwrap();
    let items_after: i64 =
        conn.query_row("SELECT COUNT(*) FROM papertrail_items", [], |r| r.get(0)).unwrap();
    assert_eq!(items_after, 3, "re-apply neither duplicates nor drops migrated rows");
}
