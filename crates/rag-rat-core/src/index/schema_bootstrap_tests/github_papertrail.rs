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
    github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
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
    github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
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
        github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
            .unwrap();
    assert_eq!(report.discovered_refs, 2);
    assert_eq!(report.synced_items, 5);
    assert_eq!(report.failed_refs, 1);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].number, 404);
    assert_eq!(report.errors[0].status, "not_found");

    let issue_hits = db.github_issue_search("sqlite", 10).unwrap();
    assert_eq!(issue_hits.len(), 1);
    assert_eq!(issue_hits[0].number, 42);

    let second =
        github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
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
        log: Default::default(),
    };

    let db = IndexDatabase::rebuild(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.parser_failures, 1);
    assert_eq!(status.parser_failure_paths[0].path, "src/broken.rs");

    let _ = fs::remove_dir_all(root);
}
