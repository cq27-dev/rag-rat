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
