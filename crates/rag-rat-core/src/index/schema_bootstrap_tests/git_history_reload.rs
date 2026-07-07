use super::*;

#[test]
fn git_history_appends_after_a_new_commit() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = db.status(&config.database).unwrap().git_history.commit_count;
    insert_sentinel_commit(&db);
    drop(db);

    // Simulate the watcher path: first index the dirty file as this worktree's overlay, then
    // commit it. The follow-up pass has visible rows to heal and should append git history instead
    // of doing a full file rebuild. Give the new commit an older timestamp than the indexed tip: a
    // naive newest-first walk that stops when it sees the old head would miss this commit, while
    // `rev_walk(new).with_hidden(old)` still finds it.
    fs::write(root.join("docs/search.md"), "# Title\ngamma token\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 1, "dirty-file indexing must not reload history");
    drop(db);
    run_git(&root, &["add", "."]);
    run_git_with_env(&root, &["commit", "-m", "Add gamma docs"], &[
        ("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z"),
        ("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z"),
    ]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.git_history.commit_count, before + 1, "one new commit was appended");
    assert_eq!(sentinel_commit_count(&db), 1, "a fast-forward append must not wipe old rows");
    assert_eq!(db.commit_search("gamma", 10).unwrap().len(), 1, "new commit is indexed");
    assert_eq!(db.commit_search("beta", 10).unwrap().len(), 1, "old commit FTS rows remain live");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_append_rebuilds_desynced_commit_fts() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let repo_id = db.active_repo_id.clone();
    db.storage
        .connection()
        .execute(
            "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, \
             committed_at_s, subject, body, changed_file_count, repo_id)
             VALUES ('__desynced_commit__', 'Desync', 'desync@example.com', 0, 0,
                     'desyncunique subject', '', 0, ?1)",
            rusqlite::params![repo_id],
        )
        .unwrap();
    assert_eq!(
        db.commit_search("desyncunique", 10).unwrap().len(),
        0,
        "the synthetic existing commit starts absent from commit_fts"
    );
    drop(db);

    fs::write(root.join("docs/search.md"), "# Title\ngamma token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add gamma docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(db.commit_search("gamma", 10).unwrap().len(), 1, "new commit is indexed");
    assert_eq!(
        db.commit_search("desyncunique", 10).unwrap().len(),
        1,
        "fast-forward append rebuilds commit_fts and repairs pre-existing desync"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_falls_back_when_append_rows_are_already_present() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = db.status(&config.database).unwrap().git_history.commit_count;
    insert_sentinel_commit(&db);
    drop(db);

    fs::write(root.join("docs/search.md"), "# Title\ntorn token\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 1, "dirty-file indexing must not reload history");
    drop(db);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add torn docs"]);
    let new_head = git_output(&root, &["rev-parse", "HEAD"]);

    let db = IndexDatabase::open_config(&config).unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, \
             committed_at_s, subject, body, changed_file_count, repo_id)
             VALUES (?1, 'Torn', 'torn@example.com', 0, 0, 'torn partial row', '', 0, ?2)",
            rusqlite::params![new_head, db.active_repo_id],
        )
        .unwrap();
    drop(db);

    let db = IndexDatabase::index_changed(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.git_history.commit_count, before + 1, "full replacement reconverges rows");
    assert_eq!(sentinel_commit_count(&db), 0, "a torn append state must force full replacement");
    assert_eq!(db.commit_search("torn", 10).unwrap().len(), 1, "commit FTS is rebuilt");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_append_plan_falls_back_for_non_git_or_other_root() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);
    fs::write(root.join("docs/search.md"), "# Title\nforeign plan token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add foreign plan docs"]);

    let db = IndexDatabase::open_config(&config).unwrap();
    let append_plan = crate::index::git_history::prepare_plan(db.storage.connection(), &root);
    assert!(matches!(append_plan, crate::index::git_history::GitHistoryPreparePlan::Append { .. }));
    drop(db);

    let non_git = unique_temp_root();
    let _ = fs::remove_dir_all(&non_git);
    fs::create_dir_all(&non_git).unwrap();
    let _prepared =
        crate::index::git_history::prepare_with_plan(&non_git, append_plan.clone()).unwrap();

    let other = unique_temp_root();
    let _ = fs::remove_dir_all(&other);
    let other_config = git_history_test_config(&other);
    let other_db = IndexDatabase::rebuild(&other_config).unwrap();
    let prepared = crate::index::git_history::prepare_with_plan(&other, append_plan).unwrap();
    let status =
        crate::index::git_history::apply_prepared(other_db.storage.connection(), &other, prepared)
            .unwrap();
    let other_head = git_output(&other, &["rev-parse", "HEAD"]);
    assert_eq!(status.indexed_head.as_deref(), Some(other_head.as_str()));

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(non_git);
    let _ = fs::remove_dir_all(other);
}

#[test]
fn git_history_prepare_plan_fails_closed_on_incomplete_or_shallow_cursors() {
    enum CursorCase {
        MissingHead,
        MissingRoot,
        MissingShallow,
        MissingComplete,
        ShallowTrue,
        ShallowBad,
        CompleteFalse,
        CompleteBad,
    }
    for case in [
        CursorCase::MissingHead,
        CursorCase::MissingRoot,
        CursorCase::MissingShallow,
        CursorCase::MissingComplete,
        CursorCase::ShallowTrue,
        CursorCase::ShallowBad,
        CursorCase::CompleteFalse,
        CursorCase::CompleteBad,
    ] {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        let config = git_history_test_config(&root);
        let db = IndexDatabase::rebuild(&config).unwrap();
        match case {
            CursorCase::MissingHead => {
                db.storage
                    .connection()
                    .execute(
                        "DELETE FROM repo_meta WHERE repo_id = ?1 AND key = \
                         'git_history_indexed_head'",
                        [&db.active_repo_id],
                    )
                    .unwrap();
            },
            CursorCase::MissingRoot => {
                db.storage
                    .connection()
                    .execute(
                        "DELETE FROM repo_meta WHERE repo_id = ?1 AND key = \
                         'git_history_indexed_root'",
                        [&db.active_repo_id],
                    )
                    .unwrap();
            },
            CursorCase::MissingShallow => {
                db.storage
                    .connection()
                    .execute(
                        "DELETE FROM repo_meta WHERE repo_id = ?1 AND key = \
                         'git_history_indexed_shallow'",
                        [&db.active_repo_id],
                    )
                    .unwrap();
            },
            CursorCase::MissingComplete => {
                db.storage
                    .connection()
                    .execute(
                        "DELETE FROM repo_meta WHERE repo_id = ?1 AND key = \
                         'git_history_indexed_complete'",
                        [&db.active_repo_id],
                    )
                    .unwrap();
            },
            CursorCase::ShallowTrue => {
                db.storage
                    .connection()
                    .execute(
                        "UPDATE repo_meta SET value = '1'
                         WHERE repo_id = ?1 AND key = 'git_history_indexed_shallow'",
                        [&db.active_repo_id],
                    )
                    .unwrap();
            },
            CursorCase::ShallowBad => {
                db.storage
                    .connection()
                    .execute(
                        "UPDATE repo_meta SET value = 'not-a-flag'
                         WHERE repo_id = ?1 AND key = 'git_history_indexed_shallow'",
                        [&db.active_repo_id],
                    )
                    .unwrap();
            },
            CursorCase::CompleteFalse => {
                db.storage
                    .connection()
                    .execute(
                        "UPDATE repo_meta SET value = '0'
                         WHERE repo_id = ?1 AND key = 'git_history_indexed_complete'",
                        [&db.active_repo_id],
                    )
                    .unwrap();
            },
            CursorCase::CompleteBad => {
                db.storage
                    .connection()
                    .execute(
                        "UPDATE repo_meta SET value = 'not-a-flag'
                         WHERE repo_id = ?1 AND key = 'git_history_indexed_complete'",
                        [&db.active_repo_id],
                    )
                    .unwrap();
            },
        }

        let plan = crate::index::git_history::prepare_plan(db.storage.connection(), &root);
        assert!(
            matches!(plan, crate::index::git_history::GitHistoryPreparePlan::Full),
            "corrupt cursor metadata must fail closed to a full reload plan"
        );
        drop(db);
        let _ = fs::remove_dir_all(root);
    }
}

#[test]
fn incomplete_git_history_cursor_forces_full_reload_before_fast_forward_append() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = db.status(&config.database).unwrap().git_history.commit_count;
    insert_sentinel_commit(&db);
    db.storage
        .connection()
        .execute(
            "UPDATE repo_meta SET value = '0'
             WHERE repo_id = ?1 AND key = 'git_history_indexed_complete'",
            [&db.active_repo_id],
        )
        .unwrap();
    drop(db);

    fs::write(root.join("docs/search.md"), "# Title\ncomplete marker token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add complete marker docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.git_history.commit_count, before + 1, "full reload indexes the new commit");
    assert_eq!(
        sentinel_commit_count(&db),
        0,
        "an incomplete cursor must force full replacement, not append"
    );
    assert_eq!(db.commit_search("complete marker", 10).unwrap().len(), 1);
    assert_eq!(
        db.repo_meta("git_history_indexed_complete").unwrap().as_deref(),
        Some("1"),
        "a complete full reload restores the append-safe cursor marker"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn stale_prepared_append_is_noop_after_another_pass_catches_up() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);
    fs::write(root.join("docs/search.md"), "# Title\nalready current token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add already current docs"]);
    let new_head = git_output(&root, &["rev-parse", "HEAD"]);

    let db = IndexDatabase::open_config(&config).unwrap();
    let plan = crate::index::git_history::prepare_plan(db.storage.connection(), &root);
    let prepared = crate::index::git_history::prepare_with_plan(&root, plan).unwrap();
    drop(db);

    let db = IndexDatabase::index_changed(&config).unwrap();
    insert_sentinel_commit(&db);
    let (status, cursors) = crate::index::git_history::apply_prepared_deferring_cursors(
        db.storage.connection(),
        &root,
        prepared,
    )
    .unwrap();
    assert!(cursors.is_some(), "the no-op path still returns the current cursor");
    assert_eq!(status.indexed_head.as_deref(), Some(new_head.as_str()));
    assert_eq!(sentinel_commit_count(&db), 1, "stale prepared append must not rewrite rows");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn stale_prepared_append_clears_when_root_loses_git_dir() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);
    fs::write(root.join("docs/search.md"), "# Title\ndetached root token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add detached root docs"]);

    let db = IndexDatabase::open_config(&config).unwrap();
    let plan = crate::index::git_history::prepare_plan(db.storage.connection(), &root);
    let prepared = crate::index::git_history::prepare_with_plan(&root, plan).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE repo_meta SET value = '0'
             WHERE repo_id = ?1 AND key = 'git_history_indexed_complete'",
            [&db.active_repo_id],
        )
        .unwrap();
    fs::rename(root.join(".git"), root.join(".git.gone")).unwrap();

    let (status, cursors) = crate::index::git_history::apply_prepared_deferring_cursors(
        db.storage.connection(),
        &root,
        prepared,
    )
    .unwrap();
    assert!(cursors.is_none(), "a vanished repository cannot keep append cursors");
    assert!(!status.available, "status reports the lost git repository");
    assert_eq!(status.commit_count, 0, "the stale git-history rows were cleared");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn stale_prepared_append_is_noop_when_db_cursor_is_ahead() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    fs::write(root.join("docs/search.md"), "# Title\nmiddle race token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add middle race docs"]);
    let middle_head = git_output(&root, &["rev-parse", "HEAD"]);

    let db = IndexDatabase::open_config(&config).unwrap();
    let plan = crate::index::git_history::prepare_plan(db.storage.connection(), &root);
    let prepared = crate::index::git_history::prepare_with_plan(&root, plan).unwrap();
    drop(db);

    fs::write(root.join("docs/search.md"), "# Title\nnewer race token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add newer race docs"]);
    let newer_head = git_output(&root, &["rev-parse", "HEAD"]);
    assert_ne!(middle_head, newer_head);

    let db = IndexDatabase::index_changed(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.git_history.indexed_head.as_deref(), Some(newer_head.as_str()));
    assert_eq!(db.commit_search("middle", 10).unwrap().len(), 1, "middle commit is indexed");
    assert_eq!(db.commit_search("newer", 10).unwrap().len(), 1, "newer commit is indexed");
    insert_sentinel_commit(&db);
    let before = db.status(&config.database).unwrap().git_history.commit_count;

    let status =
        crate::index::git_history::apply_prepared(db.storage.connection(), &root, prepared)
            .unwrap();

    assert_eq!(status.indexed_head.as_deref(), Some(newer_head.as_str()));
    assert_eq!(status.commit_count, before);
    assert_eq!(sentinel_commit_count(&db), 1, "stale prepared append must not rewrite newer rows");
    assert_eq!(db.commit_search("middle", 10).unwrap().len(), 1);
    assert_eq!(db.commit_search("newer", 10).unwrap().len(), 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_append_records_new_head_for_out_of_scope_fast_forward_commit() {
    let worktree = unique_temp_root();
    let _ = fs::remove_dir_all(&worktree);
    fs::create_dir_all(worktree.join("pkg/docs")).unwrap();
    fs::create_dir_all(worktree.join("pkg/src")).unwrap();
    run_git(&worktree, &["init"]);
    run_git(&worktree, &["config", "user.name", "Rag Rat"]);
    run_git(&worktree, &["config", "user.email", "rag@example.com"]);
    fs::write(worktree.join(".gitignore"), ".rag-rat/\n").unwrap();
    fs::write(worktree.join("pkg/docs/search.md"), "# Title\nscoped token\n").unwrap();
    fs::write(worktree.join("pkg/src/lib.rs"), "pub fn tracked_symbol() {}\n").unwrap();
    run_git(&worktree, &["add", "."]);
    run_git(&worktree, &["commit", "-m", "Initial scoped content"]);
    let config = rag_rat_config(&worktree.join("pkg"));

    let db = IndexDatabase::rebuild(&config).unwrap();
    let repo_id = db.active_repo_id.clone();
    db.storage
        .connection()
        .execute(
            "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, \
             committed_at_s, subject, body, changed_file_count, repo_id)
             VALUES ('__empty_append_desynced_commit__', 'Desync', 'desync@example.com', 0, 0,
                     'emptyappendunique subject', '', 0, ?1)",
            rusqlite::params![repo_id],
        )
        .unwrap();
    assert_eq!(
        db.commit_search("emptyappendunique", 10).unwrap().len(),
        0,
        "the synthetic existing commit starts absent from commit_fts"
    );
    let before = db.status(&config.database).unwrap().git_history.commit_count;
    insert_sentinel_commit(&db);
    drop(db);

    fs::write(worktree.join("README.md"), "outside the configured root\n").unwrap();
    run_git(&worktree, &["add", "."]);
    run_git(&worktree, &["commit", "-m", "Change outside indexed subtree"]);
    let new_head = git_output(&worktree, &["rev-parse", "HEAD"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.git_history.commit_count, before, "out-of-scope commit adds no row");
    assert_eq!(status.git_history.indexed_head.as_deref(), Some(new_head.as_str()));
    assert_eq!(sentinel_commit_count(&db), 1, "empty append must not full-replace history rows");
    assert_eq!(
        db.commit_search("emptyappendunique", 10).unwrap().len(),
        1,
        "empty append still rebuilds commit_fts and repairs pre-existing desync"
    );

    let _ = fs::remove_dir_all(worktree);
}

#[test]
fn git_history_append_reports_commit_fts_prepare_errors() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);
    fs::write(root.join("docs/search.md"), "# Title\nfts prepare token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add fts prepare docs"]);

    let db = IndexDatabase::open_config(&config).unwrap();
    let plan = crate::index::git_history::prepare_plan(db.storage.connection(), &root);
    let prepared = crate::index::git_history::prepare_with_plan(&root, plan).unwrap();
    db.storage.connection().execute("DROP TABLE commit_fts", []).unwrap();
    let err = crate::index::git_history::apply_prepared_deferring_cursors(
        db.storage.connection(),
        &root,
        prepared,
    )
    .expect_err("missing commit_fts must surface an append failure");
    assert!(err.to_string().contains("commit_fts"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn repo_generation_file_count_reports_sql_errors() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.storage.connection().execute("DROP TABLE main.files", []).unwrap();
    let err = db.repo_generation_file_count(true).expect_err("missing files table must error");
    assert!(err.to_string().contains("files"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn repo_generation_file_count_ignores_foreign_worktree_overlays() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let scoped = db.indexed_file_count().unwrap();
    assert_eq!(db.repo_generation_file_count(true).unwrap(), scoped);

    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
             indexed_at_ms, indexed_revision, commit_sha, worktree_id, has_test_code, repo_id,
             generation)
             SELECT 'src/linked_overlay.rs', language, kind, sha256, modified_at_ms, generated,
                    indexed_at_ms, indexed_revision, '', '__linked_worktree__', has_test_code,
                    repo_id, generation
             FROM main.files WHERE repo_id = ?1 LIMIT 1",
            rusqlite::params![db.active_repo_id],
        )
        .unwrap();
    assert_eq!(
        db.indexed_file_count().unwrap(),
        scoped,
        "foreign linked-worktree overlay rows are outside the active temp scope"
    );
    assert_eq!(
        db.repo_generation_file_count(true).unwrap(),
        scoped,
        "foreign linked-worktree overlay rows must not make changed-mode look incomplete"
    );
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
             indexed_at_ms, indexed_revision, commit_sha, worktree_id, has_test_code, repo_id,
             generation)
             SELECT 'src/future_commit.rs', language, kind, sha256, modified_at_ms, generated,
                    indexed_at_ms, indexed_revision, '__other_commit__', '', has_test_code,
                    repo_id, generation
             FROM main.files WHERE repo_id = ?1 LIMIT 1",
            rusqlite::params![db.active_repo_id],
        )
        .unwrap();
    assert_eq!(
        db.repo_generation_file_count(true).unwrap(),
        scoped,
        "committed rows from another checkout commit must not make changed-mode look incomplete"
    );

    db.write_tombstone_in_scope(Path::new("docs/search.md"), &db.active_worktree_id).unwrap();
    assert_eq!(
        db.repo_generation_file_count(true).unwrap(),
        db.indexed_file_count().unwrap(),
        "active tombstones shadow committed rows and should not inflate the expected scope count"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn discovered_empty_active_commit_does_not_promote_changed_mode() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert!(db.indexed_file_count().unwrap() > 0);

    fs::remove_file(root.join("docs/search.md")).unwrap();
    fs::remove_file(root.join("src/lib.rs")).unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-m", "Remove target files"]);

    let db = IndexDatabase::index_discover(&config).unwrap();
    assert_eq!(db.indexed_file_count().unwrap(), 0);
    assert!(
        db.active_base_scope_discovered(&config.targets).unwrap(),
        "a successful discover pass marks even an empty active base scope complete"
    );
    assert!(
        db.repo_generation_file_count(true).unwrap() > 0,
        "retained rows from older commits still exist in the live generation"
    );
    assert_eq!(
        db.repo_generation_file_count(false).unwrap(),
        0,
        "a marked-complete active empty scope must not count retained older commit rows"
    );

    let mut started_mode = None;
    let _db = IndexDatabase::index_changed_with_progress(&config, |progress| {
        if let IndexProgress::Started { mode, .. } = progress {
            started_mode = Some(mode);
        }
    })
    .unwrap();
    assert_eq!(
        started_mode,
        Some(IndexMode::Changed),
        "after an empty active scope is discover-checked, changed-mode must not promote forever"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn changed_mode_discovers_when_target_fingerprint_is_stale() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);
    fs::create_dir_all(root.join("examples")).unwrap();
    fs::write(root.join("examples/guide.md"), "# Guide\nnew target token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add example docs"]);

    let db = IndexDatabase::rebuild(&config).unwrap();
    assert!(!path_in_scope(&db, "examples/guide.md"), "old targets do not index examples/");
    assert!(db.active_base_scope_discovered(&config.targets).unwrap());

    let mut expanded_config = config.clone();
    expanded_config.targets.push(ResolvedTarget {
        name: "examples".to_string(),
        language: Language::Markdown,
        directories: vec![PathBuf::from("examples")],
        include: vec!["**/*.md".to_string()],
        exclude: vec!["drafts/**".to_string()],
        kind: TargetKind::Docs,
    });
    assert!(
        !db.active_base_scope_discovered(&expanded_config.targets).unwrap(),
        "adding a configured target invalidates the base-scope marker"
    );
    assert_eq!(
        db.indexed_file_count().unwrap(),
        db.repo_generation_file_count(true).unwrap(),
        "the old target rows can still make the count-only fallback look complete"
    );
    drop(db);

    let mut started_mode = None;
    let db = IndexDatabase::index_changed_with_progress(&expanded_config, |progress| {
        if let IndexProgress::Started { mode, .. } = progress {
            started_mode = Some(mode);
        }
    })
    .unwrap();
    assert_eq!(
        started_mode,
        Some(IndexMode::Discover),
        "a stale target fingerprint must force discovery even when row counts match"
    );
    assert!(
        path_in_scope(&db, "examples/guide.md"),
        "changed-mode discovery indexes the new target"
    );
    assert!(db.active_base_scope_discovered(&expanded_config.targets).unwrap());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn base_scope_marker_is_absent_without_commit_or_worktree_scope() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    db.active_commit_sha.clear();
    db.active_worktree_id.clear();

    assert!(
        !db.active_base_scope_discovered(&config.targets).unwrap(),
        "an unscoped connection cannot satisfy the base-scope discovery marker"
    );
    assert!(
        !db.mark_active_base_scope_discovered(&config.targets).unwrap(),
        "an unscoped connection has no stable marker to write"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn watch_shutdown_marker_clear_is_idempotent() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert!(
        !db.clear_watch_shutdown_reconcile_pending().unwrap(),
        "clearing an absent shutdown marker is a no-op"
    );
    assert!(db.mark_watch_shutdown_reconcile_pending().unwrap());
    assert!(db.watch_shutdown_reconcile_pending().unwrap());
    assert!(db.clear_watch_shutdown_reconcile_pending().unwrap());
    assert!(!db.watch_shutdown_reconcile_pending().unwrap());
    assert!(
        !db.clear_watch_shutdown_reconcile_pending().unwrap(),
        "a second clear remains write-free"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_appends_after_a_merge_commit() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = db.status(&config.database).unwrap().git_history.commit_count;
    insert_sentinel_commit(&db);
    drop(db);

    let indexed_head = git_output(&root, &["rev-parse", "HEAD"]);
    run_git(&root, &["checkout", "-b", "topic"]);
    fs::write(root.join("docs/search.md"), "# Title\nomega token\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 1, "dirty-file indexing must not reload history");
    drop(db);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add omega docs"]);
    run_git(&root, &["checkout", "-B", "merge-target", &indexed_head]);
    run_git(&root, &["merge", "--no-ff", "topic", "-m", "Merge topic branch"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.git_history.commit_count, before + 2, "topic and merge commits append");
    assert_eq!(sentinel_commit_count(&db), 1, "a merge append must not wipe old rows");
    assert!(
        db.commit_search("omega", 10).unwrap().iter().any(|hit| hit.subject == "Add omega docs"),
        "topic commit must be searchable after merge append"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_appends_after_a_squash_commit() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = db.status(&config.database).unwrap().git_history.commit_count;
    insert_sentinel_commit(&db);
    drop(db);

    let indexed_head = git_output(&root, &["rev-parse", "HEAD"]);
    run_git(&root, &["checkout", "-b", "topic"]);
    fs::write(root.join("docs/search.md"), "# Title\ntheta token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Draft theta docs"]);
    fs::write(root.join("docs/search.md"), "# Title\ntheta token\nlambda token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Refine theta docs"]);
    run_git(&root, &["checkout", "-B", "squash-target", &indexed_head]);
    run_git(&root, &["merge", "--squash", "topic"]);
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 1, "dirty-file indexing must not reload history");
    drop(db);
    run_git(&root, &["commit", "-m", "Squash theta docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.git_history.commit_count, before + 1, "squash adds one new commit");
    assert_eq!(sentinel_commit_count(&db), 1, "a squash append must not wipe old rows");
    assert_eq!(db.commit_search("theta", 10).unwrap().len(), 1, "squash commit is indexed");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_reloads_after_a_history_rewrite() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = db.status(&config.database).unwrap().git_history.commit_count;
    insert_sentinel_commit(&db);
    drop(db);

    // Amend rewrites the tip to a new sha WITHOUT adding a commit — a non-fast-forward rewrite,
    // like the squash that motivated the gate. HEAD's content-addressed sha changes → reload.
    run_git(&root, &["commit", "--amend", "-m", "Refresh delta docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 0, "a history rewrite must force a reload");
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.git_history.commit_count, before, "amend does not change the commit count");
    assert_eq!(db.commit_search("delta", 10).unwrap().len(), 1, "rewritten subject is indexed");
    assert_eq!(db.commit_search("beta", 10).unwrap().len(), 0, "old subject is gone after rewrite");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_reloads_after_a_non_fast_forward_branch_switch() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    insert_sentinel_commit(&db);
    drop(db);

    run_git(&root, &["checkout", "-b", "side", "HEAD~1"]);
    fs::write(root.join("docs/search.md"), "# Title\nbranch token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Switch to branch docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 0, "a non-FF branch switch must force a reload");
    assert_eq!(db.commit_search("branch", 10).unwrap().len(), 1, "new branch history is indexed");
    assert_eq!(db.commit_search("beta", 10).unwrap().len(), 0, "old branch history is gone");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_reloads_after_squashing_indexed_commits() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = db.status(&config.database).unwrap().git_history.commit_count;
    insert_sentinel_commit(&db);
    drop(db);

    let base = git_output(&root, &["rev-parse", "HEAD"]);
    fs::write(root.join("docs/search.md"), "# Title\nrho token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add rho docs"]);
    fs::write(root.join("docs/search.md"), "# Title\nrho token\nsigma token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "obsoleteunique sigma docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 1, "FF commits append before the squash");
    assert_eq!(
        db.status(&config.database).unwrap().git_history.commit_count,
        before + 2,
        "both temporary commits were appended"
    );
    drop(db);

    run_git(&root, &["reset", "--soft", &base]);
    run_git(&root, &["commit", "-m", "Squash rho sigma docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 0, "squashing indexed commits must force a reload");
    assert_eq!(
        db.status(&config.database).unwrap().git_history.commit_count,
        before + 1,
        "two indexed commits were replaced by one squashed commit"
    );
    assert_eq!(db.commit_search("sigma", 10).unwrap().len(), 1, "squashed commit is indexed");
    assert_eq!(
        db.commit_search("obsoleteunique", 10).unwrap().len(),
        0,
        "old commit FTS row is gone"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_reload_is_not_skipped_on_a_shallow_clone() {
    let origin = unique_temp_root();
    let _ = fs::remove_dir_all(&origin);
    let _ = git_history_test_config(&origin); // origin repo with two commits

    let shallow = unique_temp_root();
    let _ = fs::remove_dir_all(&shallow);
    // Local clones ignore --depth unless the source is a file:// URL; use one so the clone is
    // genuinely shallow.
    run_git(&std::env::temp_dir(), &[
        "clone",
        "--depth",
        "1",
        &format!("file://{}", origin.display()),
        shallow.to_str().unwrap(),
    ]);
    let config = rag_rat_config(&shallow);
    // A depth-cut shallow clone cannot derive a PORTABLE identity (its root is unreachable), but it
    // no longer fails: `resolve_repo_identity` derives a deterministic `local:`-prefixed LocalOnly
    // id from the shallow boundary and proceeds. So rebuild/open/incremental adopt it WITHOUT a
    // pin — exactly the path CI's shallow fixtures exercise — and this test keeps its subject
    // (the history reload gate) while also covering LocalOnly adoption end-to-end.

    let db = IndexDatabase::rebuild(&config).unwrap();
    assert!(
        db.active_repo_id.starts_with("local:"),
        "a cut shallow clone adopts under a LocalOnly id, got {}",
        db.active_repo_id
    );
    insert_sentinel_commit(&db);
    drop(db);

    // HEAD is unchanged, but a shallow clone can be deepened without moving HEAD, so its history
    // is not pinned by the HEAD sha — the gate must NOT skip. It reloads and wipes the sentinel.
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 0, "a shallow clone must never skip the reload");

    let _ = fs::remove_dir_all(origin);
    let _ = fs::remove_dir_all(shallow);
}

#[test]
fn idle_discover_sweep_does_not_rewrite_indexed_at_ms() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    // Stamp a non-numeric sentinel so any spurious timestamp write is unmistakable. Under the
    // ACTIVE repo id (a real git fixture is adopted, so the `__unassigned__` placeholder repos row
    // is gone and `repo_meta` under it would trip the FK).
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES(?1, 'indexed_at_ms', 'SENTINEL')
             ON CONFLICT(repo_id, key) DO UPDATE SET value = 'SENTINEL'",
            [&db.active_repo_id],
        )
        .unwrap();
    drop(db);

    // A discover sweep over an unchanged tree must not mutate the DB — the sentinel survives
    // (no timestamp-only write + COMMIT). See issue #63.
    let db = IndexDatabase::index_discover(&config).unwrap();
    assert_eq!(
        read_meta(&db, "indexed_at_ms").as_deref(),
        Some("SENTINEL"),
        "an unchanged discover sweep must not rewrite indexed_at_ms"
    );
    drop(db);

    // A real change must persist — the sweep writes a fresh timestamp, clearing the sentinel.
    fs::write(root.join("docs/added.md"), "# Added\nfresh content\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add a doc"]);
    let db = IndexDatabase::index_discover(&config).unwrap();
    assert_ne!(
        read_meta(&db, "indexed_at_ms").as_deref(),
        Some("SENTINEL"),
        "a sweep that indexes a new file must update indexed_at_ms"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn index_discover_reporting_flags_content_changes() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);
    IndexDatabase::rebuild(&config).unwrap();

    // No change → reports false, so the watch loop skips the reconcile / memory-validate tail.
    let (_db, changed) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert!(!changed, "an unchanged discover sweep must report no content change");

    // A new file on disk → reports true.
    fs::write(root.join("docs/extra.md"), "# Extra\nbody text\n").unwrap();
    let (_db, changed) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert!(changed, "a discover sweep that indexes a new file must report a content change");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn discover_relanguages_h_when_binding_changes_c_to_cpp() {
    // A `.h` indexed under a `c` binding, then re-discovered under a `cpp` binding with IDENTICAL
    // content, must be reindexed as C++ — discovery treats (language, kind) drift as a change, not
    // just sha drift. Without this the `.h`→C++ upgrade would never take effect on an existing
    // index (the sha is unchanged) until `--full`.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.h"), "class X { public: void f(); };\n").unwrap();

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::C)).unwrap();
    let lang: String = db
        .storage
        .connection()
        .query_row("SELECT language FROM files WHERE path = 'src/lib.h'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lang, "c", "indexed as C under the c binding");
    drop(db);

    let (db, changed) =
        IndexDatabase::index_discover_reporting(&source_config(root.clone(), Language::Cpp))
            .unwrap();
    assert!(changed, "re-languaging a .h with unchanged content must report a change");
    let lang: String = db
        .storage
        .connection()
        .query_row("SELECT language FROM files WHERE path = 'src/lib.h'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lang, "cpp", "the .h must be reindexed as C++ after the binding change");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn changed_mode_ignores_gitignored_target_files_under_root_target() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("target/debug")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);
    fs::write(root.join(".gitignore"), "target/\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn tracked_symbol() {}\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Initial source"]);

    let mut config = source_config(root.clone(), Language::Rust);
    config.targets[0].directories = vec![PathBuf::from(".")];
    config.targets[0].include = vec!["**/*.rs".to_string()];

    let db = IndexDatabase::rebuild(&config).unwrap();
    let before: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
        .unwrap();
    drop(db);

    fs::write(root.join("target/debug/generated.rs"), "pub fn ignored_build_artifact() {}\n")
        .unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    let after: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
        .unwrap();
    let target_rows: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE path LIKE 'target/%'", [], |row| {
            row.get(0)
        })
        .unwrap();

    assert_eq!(before, after, "ignored target/ writes must not index new files");
    assert_eq!(target_rows, 0, "target/ artifacts must not enter the index");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_rust_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
use crate::worker::Worker;
mod worker;

trait Service {
    fn serve(&self);
}

struct Worker;

impl Service for Worker {
    fn serve(&self) {
        helper();
    }
}

fn helper() {}

fn caller() {
    helper();
    Worker.serve();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "caller", "helper", "calls_name", "Syntactic");
    assert_edge(&db, "Worker", "Service", "implements", "Syntactic");
    assert_edge(&db, "src/lib.rs", "worker", "imports", "Syntactic");
    let callers = db.find_callers("helper", 10).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("caller"))
                && edge.edge_kind == "calls_name"
        }),
        "helper callers: {callers:?}"
    );

    let _ = fs::remove_dir_all(root);
}
