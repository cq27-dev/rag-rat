use super::*;

#[test]
fn fold_status_candidates_marks_complete_on_a_clean_read() {
    let mut candidates = BTreeSet::new();
    let items: Vec<Result<&str, ()>> = vec![Ok("src/a.rs"), Ok("src/b.rs")];
    let complete = fold_status_candidates(&mut candidates, items, |s| PathBuf::from(s));
    assert!(complete, "a clean status read is complete");
    assert_eq!(candidates, BTreeSet::from([PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]),);
}

#[test]
fn fold_status_candidates_marks_incomplete_on_a_per_item_error() {
    // The bug the #219 review caught: `flatten()` dropped the erroring item but left the read
    // looking complete, so the prune treated a partial candidate set as authoritative and could
    // delete valid overlay rows. A per-item error must mark the delta INCOMPLETE.
    let mut candidates = BTreeSet::new();
    let items: Vec<Result<&str, ()>> = vec![Ok("src/a.rs"), Err(()), Ok("src/c.rs")];
    let complete = fold_status_candidates(&mut candidates, items, |s| PathBuf::from(s));
    assert!(!complete, "a per-item status error makes the delta incomplete → caller skips prune");
    // Stops at the error (the trailing path after it is not authoritative either way).
    assert!(candidates.contains(Path::new("src/a.rs")));
    assert!(!candidates.contains(Path::new("src/c.rs")));
}

#[test]
fn fold_status_candidates_empty_stream_is_complete() {
    let mut candidates = BTreeSet::new();
    let items: Vec<Result<&str, ()>> = vec![];
    assert!(fold_status_candidates(&mut candidates, items, |s| PathBuf::from(s)));
    assert!(candidates.is_empty());
}

#[test]
fn checkout_probes_isolate_base_and_linked_rows() {
    use rag_rat_base::checkout::CheckoutRef;
    let (_root, config) =
        crate::index::schema_bootstrap_tests::poison_test_config("checkout_probes");
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    rag_rat_base::test_git::run(&config.root, &[
        "worktree",
        "add",
        "-q",
        "-b",
        "probe-linked",
        linked.to_str().unwrap(),
    ]);
    let overlay = resolve_overlay_scope(&config, &linked).unwrap().unwrap();
    let base = CheckoutKey::commit(overlay.checkout.commit_sha.clone());
    let branch = CheckoutKey::worktree(overlay.checkout.worktree_id.clone());
    let path = Path::new("src/lib.rs");
    db.set_context(overlay.checkout.borrowed()).unwrap();
    assert!(db.base_scope_has_path(path, base.borrowed()).unwrap());
    assert!(!db.overlay_source_row_exists(path, branch.borrowed()).unwrap());
    db.write_tombstone_in_scope(path, &branch.worktree_id).unwrap();
    assert!(db.overlay_tombstone_exists(path, branch.borrowed()).unwrap());
    assert!(!db.overlay_tombstone_exists(path, base.borrowed()).unwrap());
    assert!(!db.overlay_tombstone_exists(path, CheckoutRef::worktree("other-sibling")).unwrap());
    db.remove_file_in_scope(path, branch.borrowed()).unwrap();
    assert!(db.base_scope_has_path(path, base.borrowed()).unwrap());
    assert!(!db.overlay_tombstone_exists(path, branch.borrowed()).unwrap());
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());
}

/// A deferred FK fails at COMMIT, after the refresh body has completed successfully.
fn inject_deferred_commit_failure(conn: &rusqlite::Connection, operation: &str, key: &str) {
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE commit_failure_parent(id INTEGER PRIMARY KEY);
         CREATE TABLE commit_failure_child(parent_id INTEGER REFERENCES commit_failure_parent(id) \
         DEFERRABLE INITIALLY DEFERRED);
         CREATE TEMP TRIGGER fail_refresh_commit AFTER {operation} ON main.repo_meta
         WHEN {}.key = '{key}' BEGIN INSERT INTO commit_failure_child VALUES (1); END;",
        if operation == "DELETE" { "OLD" } else { "NEW" },
    ))
    .unwrap();
}

#[test]
fn pending_rebuild_commit_failure_rolls_back_and_preserves_obligation() {
    let (_root, config) =
        crate::index::schema_bootstrap_tests::poison_test_config("commit_rollback");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.set_repo_meta_if_changed(OVERLAY_LOGICAL_REBUILD_PENDING_META, "1").unwrap();
    let conn = db.storage.connection();
    inject_deferred_commit_failure(conn, "DELETE", OVERLAY_LOGICAL_REBUILD_PENDING_META);
    let err = db.apply_pending_logical_rebuild().unwrap_err();
    assert!(err.to_string().contains("FOREIGN KEY"), "{err:#}");
    assert!(conn.is_autocommit(), "failed COMMIT must not strand an open transaction");
    assert_eq!(db.repo_meta(OVERLAY_LOGICAL_REBUILD_PENDING_META).unwrap().as_deref(), Some("1"));
    conn.execute_batch("DROP TRIGGER fail_refresh_commit").unwrap();
    assert!(db.apply_pending_logical_rebuild().unwrap());
    crate::index::poison_sibling::assert_sibling_intact(conn);
}

#[test]
fn overlay_package_commit_failure_rolls_back_and_allows_retry() {
    let (_root, config) =
        crate::index::schema_bootstrap_tests::poison_test_config("package_rollback");
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    rag_rat_base::test_git::run(&config.root, &[
        "worktree",
        "add",
        "-q",
        "-b",
        "package-linked",
        linked.to_str().unwrap(),
    ]);
    let key = rag_rat_db::meta::LENS_SYMBOLS_REVISION_META;
    // Force the bump down its INSERT path so one trigger covers the failure seam.
    rag_rat_db::meta::delete_repo_meta(db.storage.connection(), &db.active_repo_id, key).unwrap();
    inject_deferred_commit_failure(db.storage.connection(), "INSERT", key);
    let err = db.refresh_worktree_overlay_packages(&config, &linked).unwrap_err();
    assert!(err.to_string().contains("FOREIGN KEY"), "{err:#}");
    assert!(
        db.storage.connection().is_autocommit(),
        "failed COMMIT must not strand an open transaction"
    );
    db.storage.connection().execute_batch("DROP TRIGGER fail_refresh_commit").unwrap();
    db.refresh_worktree_overlay_packages(&config, &linked).unwrap();
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());
}
