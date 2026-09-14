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
