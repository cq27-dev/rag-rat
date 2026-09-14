use super::*;

#[test]
fn item_threads_commit_individually_and_resume_without_refetching_finished_threads() {
    let conn = db();
    let binding = binding(&[]);
    let same_page = vec![
        item("2", "2026-01-02T00:00:00Z", "two", &[]),
        item("1", "2026-01-01T00:00:00Z", "one", &[]),
    ];
    let first = ScriptClient::new(vec![page(same_page.clone())]).with_item_comment_results(vec![
        Ok(vec![comment("2", "two-comment", "2026-01-02T01:00:00Z")]),
        Err(anyhow::Error::new(TransportError::Paused {
            resume_at_ms: 42,
            reason: PauseReason::PassBudget,
        })),
    ]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
            .unwrap();
    assert_eq!(report.paused_until_ms, Some(42));
    assert_eq!(keys(&conn), vec!["1", "2"]);

    let resumed = ScriptClient::new(vec![page(same_page), page(Vec::new())])
        .with_item_comments(vec![comment("1", "one-comment", "2026-01-01T01:00:00Z")]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
        .unwrap();
    assert_eq!(resumed.item_comment_requests.borrow().as_slice(), ["1"]);
    assert_eq!(keys(&conn), vec!["1", "2"]);
}

#[test]
fn resumed_item_thread_revalidates_earlier_pages_before_pruning_deleted_comments() {
    let conn = db();
    let binding = binding(&[]);
    let provider_page = vec![item("1", "2026-01-01T00:00:00Z", "one", &[])];
    let next = PageCursor {
        stream: Some("default".to_string()),
        page_token: Some("thread-page-2".to_string()),
        ..PageCursor::default()
    };
    let first = ScriptClient::new(vec![page(provider_page.clone())]).with_item_comment_pages(vec![
        Ok(CommentsPage {
            comments: vec![comment("1", "first", "2026-01-01T01:00:00Z")],
            next: Some(next),
            frontier: None,
        }),
        Err(anyhow::Error::new(TransportError::Paused {
            resume_at_ms: 42,
            reason: PauseReason::PassBudget,
        })),
    ]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
            .unwrap();
    assert_eq!(report.paused_until_ms, Some(42));
    assert_eq!(report.stored_comments, 1);
    let cursor = load_cursor(&conn, &binding).unwrap();
    assert_eq!(
        cursor
            .item_thread_cursor
            .as_ref()
            .and_then(|thread| thread.page_cursor.as_ref())
            .and_then(|cursor| cursor.page_token.as_deref()),
        Some("thread-page-2")
    );

    let resumed = ScriptClient::new(vec![page(provider_page), page(Vec::new())])
        .with_item_comment_pages(vec![
            Ok(CommentsPage {
                // The first-page comment was deleted while this thread was paused.
                comments: Vec::new(),
                next: Some(PageCursor {
                    stream: Some("default".to_string()),
                    page_token: Some("thread-page-2".to_string()),
                    ..PageCursor::default()
                }),
                frontier: None,
            }),
            Ok(CommentsPage {
                comments: vec![comment("1", "second", "2026-01-01T02:00:00Z")],
                next: None,
                frontier: None,
            }),
            // The confirming walk is identical, so absence of the deleted first comment is
            // now safe to apply destructively.
            Ok(CommentsPage {
                comments: vec![comment("1", "second", "2026-01-01T02:00:00Z")],
                next: None,
                frontier: None,
            }),
        ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
        .unwrap();
    let requests = resumed.item_comment_page_requests.borrow();
    assert_eq!(requests[0].page_token, None);
    assert_eq!(requests[1].page_token.as_deref(), Some("thread-page-2"));
    assert_eq!(requests[2].page_token, None);
    assert!(load_cursor(&conn, &binding).unwrap().item_thread_cursor.is_none());
    let comments: Vec<String> = conn
        .prepare("SELECT comment_id FROM papertrail_comments ORDER BY comment_id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(comments, vec!["second"]);
}

#[test]
fn resumed_page_refreshes_a_completed_item_when_its_provider_version_changed() {
    let conn = db();
    let binding = binding(&[]);
    let old_page = vec![
        item("2", "2026-01-02T00:00:00Z", "old", &[]),
        item("1", "2026-01-01T00:00:00Z", "one", &[]),
    ];
    let first = ScriptClient::new(vec![page(old_page)]).with_item_comment_results(vec![
        Ok(vec![comment("2", "old-comment", "2026-01-02T01:00:00Z")]),
        Err(anyhow::Error::new(TransportError::Paused {
            resume_at_ms: 42,
            reason: PauseReason::PassBudget,
        })),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
        .unwrap();

    let changed_page = vec![
        item("2", "2026-01-03T00:00:00Z", "new", &[]),
        item("1", "2026-01-01T00:00:00Z", "one", &[]),
    ];
    let resumed =
        ScriptClient::new(vec![page(changed_page), page(Vec::new())]).with_item_comment_results(
            vec![Ok(Vec::new()), Ok(vec![comment("2", "new-comment", "2026-01-03T01:00:00Z")])],
        );
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
        .unwrap();

    assert_eq!(resumed.item_comment_requests.borrow().as_slice(), ["1", "2"]);
    let title: String = conn
        .query_row("SELECT title FROM papertrail_items WHERE item_key='2'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(title, "new");
    let comment_ids = conn
        .prepare("SELECT comment_id FROM papertrail_comments WHERE item_key='2'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(comment_ids, vec!["new-comment"]);
}

#[test]
fn failed_comment_refresh_keeps_the_previous_complete_thread() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "initial", &[])]),
        page(Vec::new()),
    ])
    .with_item_comments(vec![comment("1", "old", "2026-01-01T01:00:00Z")]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let failed =
        ScriptClient::new(vec![page(vec![item("1", "2026-01-02T00:00:00Z", "updated", &[])])])
            .with_probe(FreshnessResult {
                latest: Some("2026-01-02T00:00:00Z".to_string()),
                etag: Some("v2".to_string()),
                not_modified: false,
            })
            .with_item_comment_results(vec![Err(anyhow::anyhow!("comment fetch failed"))]);
    let error =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &failed, false))
            .unwrap_err();
    assert_eq!(error.to_string(), "comment fetch failed");
    let comments: Vec<String> = conn
        .prepare("SELECT comment_id FROM papertrail_comments ORDER BY comment_id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(comments, vec!["old"]);
}

#[test]
fn comment_not_found_skips_the_thread_without_erasing_cached_evidence() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ])
    .with_item_comments(vec![comment("1", "seed", "2026-01-01T01:00:00Z")]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let missing =
        ScriptClient::new(vec![page(vec![item("1", "2026-01-02T00:00:00Z", "updated", &[])])])
            .with_probe(FreshnessResult {
                latest: Some("2026-01-02T00:00:00Z".to_string()),
                etag: Some("v2".to_string()),
                not_modified: false,
            })
            .with_item_comment_results(vec![Err(PapertrailClientError::ItemNotFound.into())]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &missing, false))
            .unwrap();

    assert_eq!(report.pruned_items, 0);
    assert_eq!(keys(&conn), vec!["1"]);
    let cursor = load_cursor(&conn, &binding).unwrap();
    assert!(cursor.item_thread_cursor.is_none());
    assert!(!cursor.item_delta_in_progress);
}
