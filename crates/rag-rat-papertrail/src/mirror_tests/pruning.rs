use super::*;

#[test]
fn resumed_changed_item_is_pruned_when_it_no_longer_matches_the_binding() {
    let conn = db();
    let binding = binding(&["bug"]);
    let first =
        ScriptClient::new(vec![page(vec![item("1", "2026-01-01T00:00:00Z", "old", &["bug"])])])
            .with_item_comment_results(vec![Err(anyhow::Error::new(TransportError::Paused {
                resume_at_ms: 42,
                reason: PauseReason::PassBudget,
            }))]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
            .unwrap();
    assert_eq!(report.paused_until_ms, Some(42));
    assert_eq!(keys(&conn), vec!["1"]);

    let resumed = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-02T00:00:00Z", "changed", &["feature"])]),
        page(Vec::new()),
    ]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
            .unwrap();
    assert_eq!(report.pruned_items, 1);
    assert!(keys(&conn).is_empty());
    assert!(resumed.item_comment_requests.borrow().is_empty());
    assert!(load_cursor(&conn, &binding).unwrap().item_thread_cursor.is_none());
}

#[test]
fn changing_tag_filter_prunes_and_backfills_the_new_match_set() {
    let conn = db();
    let bug = binding(&["bug"]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-02T00:00:00Z", "bug", &["bug"])]),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &bug, std::slice::from_ref(&bug), &initial, false)).unwrap();
    assert_eq!(keys(&conn), vec!["1"]);

    let docs = binding(&["docs"]);
    let changed = ScriptClient::new(vec![
        page(vec![item("2", "2026-01-03T00:00:00Z", "docs", &["docs"])]),
        page(Vec::new()),
    ]);
    let report =
        block_on(mirror_binding(&conn, &docs, std::slice::from_ref(&docs), &changed, false))
            .unwrap();
    assert_eq!(report.pruned_items, 1);
    assert!(report.completed_full_walk);
    assert_eq!(keys(&conn), vec!["2"]);
}

#[test]
fn forced_full_rewalk_heals_a_poisoned_row() {
    let conn = db();
    let binding = binding(&[]);
    store_item(&conn, Tracker::Github, &item("1", "2026-01-01T00:00:00Z", "poison", &[])).unwrap();
    store_item(&conn, Tracker::Github, &item("2", "2026-01-01T00:00:00Z", "deleted", &[])).unwrap();
    let client = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "healed", &[])]),
        page(Vec::new()),
    ]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, true))
            .unwrap();
    let title: String = conn
        .query_row("SELECT title FROM papertrail_items WHERE item_key='1'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(title, "healed");
    assert_eq!(report.pruned_items, 1);
    assert_eq!(keys(&conn), vec!["1"]);
}

#[test]
fn full_rewalk_pruning_survives_a_pause_and_an_ordinary_resume() {
    let conn = db();
    let binding = binding(&[]);
    store_item(&conn, Tracker::Github, &item("1", "2026-01-01T00:00:00Z", "kept", &[])).unwrap();
    store_item(&conn, Tracker::Github, &item("2", "2026-01-01T00:00:00Z", "gone", &[])).unwrap();
    let paused = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "kept", &[])]),
        Err(anyhow::Error::new(TransportError::Paused {
            resume_at_ms: 42,
            reason: PauseReason::PassBudget,
        })),
    ]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &paused, true))
            .unwrap();
    assert_eq!(report.paused_until_ms, Some(42));
    assert!(load_cursor(&conn, &binding).unwrap().full_rewalk);
    assert_eq!(keys(&conn), vec!["1", "2"]);

    let resumed = ScriptClient::new(vec![page(Vec::new())]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
            .unwrap();
    assert!(report.completed_full_walk);
    assert_eq!(report.pruned_items, 1);
    assert!(!load_cursor(&conn, &binding).unwrap().full_rewalk);
    assert_eq!(keys(&conn), vec!["1"]);
}

#[test]
fn unmatched_pages_skip_comments_and_delete_cached_items() {
    let conn = db();
    let all = binding(&[]);
    assert_eq!(prune_unmatched(&conn, &all).unwrap(), 0);
    store_item(&conn, Tracker::Github, &item("1", "2026-01-01T00:00:00Z", "cached", &["docs"]))
        .unwrap();

    let bugs = binding(&["bug"]);
    let client = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-02T00:00:00Z", "still docs", &["docs"])]),
        page(Vec::new()),
    ]);
    let report =
        block_on(mirror_binding(&conn, &bugs, std::slice::from_ref(&bugs), &client, false))
            .unwrap();
    assert!(report.pruned_items >= 1);
    assert!(keys(&conn).is_empty());
}

#[test]
fn pruning_an_out_of_scope_issue_takes_its_provider_closing_edges() {
    let conn = db();
    // A tag-scoped binding; the item walk stores a `docs`-labelled issue #5, then a filter
    // narrowed to `bug` prunes it — its provider closing edge must go too.
    cache_closed_issue(&conn, "5");
    crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
        project: "o/r".into(),
        issue_kind: ItemKind::Issue,
        issue_key: "5".into(),
        closer_kind: crate::CloserKind::Commit,
        closer_key: "abc".into(),
        closer_commit: Some("abc".into()),
        source: crate::ClosingEdgeSource::Provider,
    })
    .unwrap();
    delete_item_for_tests(&conn, &binding(&["bug"]), ItemKind::Issue, "5").unwrap();
    assert!(
        crate::store::closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5")
            .unwrap()
            .is_empty(),
        "a pruned issue's closing edges leave with it",
    );
}
