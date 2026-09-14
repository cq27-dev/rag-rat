use super::*;

/// A comments page whose entries all map to no comment (GitLab events on commit/snippet
/// notes) must still advance the stream through its `frontier`, or the scan replays the
/// same pages on every sync forever.
#[test]
fn a_page_of_only_skipped_comment_events_still_advances_the_stream() {
    let conn = db();
    let binding = binding(&[]);
    let first_walk = ScriptClient::new(vec![
        Ok(ItemsPage {
            items: vec![item("1", "2026-01-02T00:00:00Z", "one", &[])],
            next: None,
            backfill_boundary: None,
        }),
        Ok(ItemsPage { items: Vec::new(), next: None, backfill_boundary: None }),
    ])
    .with_repo_comments(Vec::new());
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first_walk, false))
        .unwrap();

    let quiet = ScriptClient::new(vec![])
        .with_probe(FreshnessResult { latest: None, etag: None, not_modified: true })
        .with_repo_comment_pages(vec![Ok(CommentsPage {
            comments: Vec::new(),
            next: None,
            frontier: Some("2026-02-01T00:00:00Z".to_string()),
        })]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &quiet, false))
        .unwrap();

    let cursor = load_cursor(&conn, &binding).unwrap();
    assert_eq!(
        cursor.comment_stream_cursors.get("default").and_then(|s| s.high_mark_at.as_deref()),
        Some("2026-02-01T00:00:00Z"),
        "the frontier advances the durable stream mark even with zero returned comments"
    );
}

/// A drained multi-page comment window must advance past its LAST page's frontier: with a
/// date-granular provider filter (GitLab events `after`), a first-page-only frontier keeps
/// re-opening the same busy day and replays every later page on every poll, forever.
#[test]
fn a_drained_multi_page_window_advances_past_its_last_frontier() {
    let conn = db();
    let binding = binding(&[]);
    let first_walk = ScriptClient::new(vec![
        Ok(ItemsPage {
            items: vec![item("1", "2026-01-02T00:00:00Z", "one", &[])],
            next: None,
            backfill_boundary: None,
        }),
        Ok(ItemsPage { items: Vec::new(), next: None, backfill_boundary: None }),
    ])
    .with_repo_comments(Vec::new());
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first_walk, false))
        .unwrap();

    let quiet = ScriptClient::new(vec![])
        .with_probe(FreshnessResult { latest: None, etag: None, not_modified: true })
        .with_repo_comment_pages(vec![
            Ok(CommentsPage {
                comments: vec![comment("1", "early", "2026-02-01T08:00:00Z")],
                next: Some(PageCursor {
                    page_token: Some("events-page-2".to_string()),
                    ..PageCursor::default()
                }),
                frontier: Some("2026-02-01T08:00:00Z".to_string()),
            }),
            Ok(CommentsPage {
                comments: Vec::new(),
                next: None,
                frontier: Some("2026-02-01T20:00:00Z".to_string()),
            }),
        ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &quiet, false))
        .unwrap();

    let cursor = load_cursor(&conn, &binding).unwrap();
    assert_eq!(
        cursor.comment_stream_cursors.get("default").and_then(|s| s.high_mark_at.as_deref()),
        Some("2026-02-01T20:00:00Z"),
        "the stream must clear the drained window, not pin to the first page's maximum"
    );
}

/// Namespaced providers name the comment's kind authoritatively: a missing exact-kind
/// parent (a merge request pruned by the tag filter while issue #N is cached) means SKIP —
/// never attach the comment across namespaces.
#[test]
fn namespaced_comments_never_fall_back_across_namespaces() {
    let conn = db();
    let mut binding = binding(&[]);
    binding.provider = Tracker::Gitlab;
    binding.project = "g/r".to_string();
    let mut issue = item("7", "2026-01-02T00:00:00Z", "issue seven", &[]);
    issue.project = "g/r".to_string();
    store_item(&conn, binding.provider, &issue).unwrap();

    let mut report = empty_report(&binding);
    let mut change_note = comment("7", "note:9", "2026-01-04T00:00:00Z");
    change_note.project = "g/r".to_string();
    change_note.item_kind = ItemKind::ChangeRequest;
    let mut issue_note = comment("7", "note:10", "2026-01-04T00:00:00Z");
    issue_note.project = "g/r".to_string();
    store_repo_comments(
        &conn,
        &binding,
        std::slice::from_ref(&binding),
        &[change_note, issue_note],
        &mut report,
    )
    .unwrap();

    let rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT comment_id, item_kind FROM papertrail_comments ORDER BY comment_id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        rows,
        vec![("note:10".to_string(), "issue".to_string())],
        "the merge-request note must be skipped, not attached to the issue"
    );
    assert_eq!(report.stored_comments, 1);
}

#[test]
fn repo_comments_resolve_namespaced_twins_by_their_own_kind() {
    let conn = db();
    let mut binding = binding(&[]);
    binding.provider = Tracker::Gitlab;
    binding.project = "g/r".to_string();
    let mut issue = item("1", "2026-01-02T00:00:00Z", "issue one", &[]);
    issue.project = "g/r".to_string();
    let mut change = item("1", "2026-01-03T00:00:00Z", "mr one", &[]);
    change.project = "g/r".to_string();
    change.item_kind = ItemKind::ChangeRequest;
    store_item(&conn, binding.provider, &issue).unwrap();
    store_item(&conn, binding.provider, &change).unwrap();

    let mut report = empty_report(&binding);
    let mut issue_note = comment("1", "note:1", "2026-01-04T00:00:00Z");
    issue_note.project = "g/r".to_string();
    let mut change_note = comment("1", "note:2", "2026-01-04T00:00:00Z");
    change_note.project = "g/r".to_string();
    change_note.item_kind = ItemKind::ChangeRequest;
    store_repo_comments(
        &conn,
        &binding,
        std::slice::from_ref(&binding),
        &[issue_note, change_note],
        &mut report,
    )
    .unwrap();

    let kinds: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT comment_id, item_kind FROM papertrail_comments ORDER BY comment_id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(kinds, vec![
        ("note:1".to_string(), "issue".to_string()),
        ("note:2".to_string(), "change_request".to_string()),
    ]);
}

/// The fallback half of the twin resolution: a provider whose feed cannot name the kind
/// (GitHub's issue-comment stream spans issues and pull requests) still resolves through the
/// key alone when no item of the claimed kind exists.
#[test]
fn repo_comments_fall_back_to_the_key_when_the_claimed_kind_has_no_item() {
    let conn = db();
    let binding = binding(&[]);
    let mut pull = item("7", "2026-01-02T00:00:00Z", "pull seven", &[]);
    pull.item_kind = ItemKind::ChangeRequest;
    store_item(&conn, binding.provider, &pull).unwrap();

    let mut report = empty_report(&binding);
    // The GitHub feed guesses Issue; only the pull exists.
    store_repo_comments(
        &conn,
        &binding,
        std::slice::from_ref(&binding),
        &[comment("7", "c1", "2026-01-04T00:00:00Z")],
        &mut report,
    )
    .unwrap();
    let kind: String = conn
        .query_row("SELECT item_kind FROM papertrail_comments WHERE comment_id='c1'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(kind, "change_request");
}

#[test]
fn invalid_repo_comment_continuation_commits_neither_page_nor_cursor() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "initial", &[])]),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let invalid = ScriptClient::new(Vec::new())
        .with_probe(FreshnessResult {
            latest: None,
            etag: Some("v1".to_string()),
            not_modified: true,
        })
        .with_repo_comment_pages(vec![Ok(CommentsPage {
            comments: vec![comment("1", "must-not-commit", "2026-01-02T00:00:00Z")],
            next: Some(PageCursor {
                stream: Some("other".to_string()),
                page_token: Some("page-2".to_string()),
                ..PageCursor::default()
            }),
            frontier: None,
        })]);
    let error =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &invalid, false))
            .unwrap_err();
    assert!(error.to_string().contains("crossed"));
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM papertrail_comments", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let cursor = load_cursor(&conn, &binding).unwrap();
    assert!(cursor.comment_stream_cursors["default"].page_token.is_none());
    assert!(cursor.comment_stream_cursors["default"].scan_high_mark_at.is_none());
}

#[test]
fn delta_walks_continuations_and_updates_repo_wide_comments() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "initial", &[])]),
        page(Vec::new()),
    ])
    .with_item_comments(vec![comment("1", "initial", "2026-01-01T01:00:00Z")]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let continuation = PageCursor {
        updated_since: Some("2026-01-01T00:00:00Z".to_string()),
        page_token: Some("page-2".to_string()),
        ..PageCursor::default()
    };
    let delta = ScriptClient::new(vec![
        Ok(ItemsPage {
            items: vec![item("1", "2026-01-02T00:00:00Z", "updated", &[])],
            next: Some(continuation),
            backfill_boundary: None,
        }),
        page(Vec::new()),
    ])
    .with_probe(FreshnessResult {
        latest: Some("2026-01-03T00:00:00Z".to_string()),
        etag: Some("v2".to_string()),
        not_modified: false,
    })
    .with_item_comments(vec![comment("1", "item", "2026-01-02T01:00:00Z")])
    .with_repo_comments(vec![
        comment("1", "repo", "2026-01-03T00:00:00Z"),
        comment("404", "orphan", "2026-01-04T00:00:00Z"),
    ]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &delta, false))
            .unwrap();
    assert_eq!(report.stored_items, 1);
    assert_eq!(report.stored_comments, 2);
    let comments: i64 =
        conn.query_row("SELECT COUNT(*) FROM papertrail_comments", [], |row| row.get(0)).unwrap();
    assert_eq!(comments, 2, "the complete item thread replaces its stale predecessor");
    let cursor = load_cursor(&conn, &binding).unwrap();
    assert_eq!(
        cursor.high_mark_at.as_deref(),
        Some("2026-01-02T00:00:00Z"),
        "a mutable continuation advances only through the first consumed page"
    );
    assert_eq!(cursor.comment_high_mark_at.as_deref(), Some("2026-01-04T00:00:00Z"));
    assert!(cursor.probe_etag.is_none());
}

#[test]
fn comment_pages_commit_progress_before_a_pause_and_resume_from_the_token() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ])
    .with_repo_comments(vec![comment("1", "seed", "2026-01-01T00:00:00Z")]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let next = PageCursor { page_token: Some("page-2".to_string()), ..PageCursor::default() };
    let paused = ScriptClient::new(Vec::new())
        .with_probe(FreshnessResult {
            latest: None,
            etag: Some("v1".to_string()),
            not_modified: true,
        })
        .with_repo_comment_pages(vec![
            Ok(CommentsPage {
                comments: vec![comment("1", "first", "2026-01-03T00:00:00Z")],
                next: Some(next),
                frontier: None,
            }),
            Err(anyhow::Error::new(TransportError::Paused {
                resume_at_ms: 42,
                reason: PauseReason::PassBudget,
            })),
        ]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &paused, false))
            .unwrap();
    assert_eq!(report.paused_until_ms, Some(42));
    let cursor = load_cursor(&conn, &binding).unwrap();
    let stream = &cursor.comment_stream_cursors["default"];
    assert_eq!(stream.page_token.as_deref(), Some("page-2"));
    assert_eq!(cursor.comment_high_mark_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert_eq!(stream.scan_high_mark_at.as_deref(), Some("2026-01-03T00:00:00Z"));

    let resumed = ScriptClient::new(Vec::new())
        .with_probe(FreshnessResult {
            latest: None,
            etag: Some("v1".to_string()),
            not_modified: true,
        })
        .with_repo_comments(vec![comment("1", "second", "2026-01-02T00:00:00Z")]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
        .unwrap();
    let cursor = load_cursor(&conn, &binding).unwrap();
    assert!(cursor.comment_stream_cursors["default"].page_token.is_none());
    assert_eq!(cursor.comment_high_mark_at.as_deref(), Some("2026-01-03T00:00:00Z"));
    let comments: i64 =
        conn.query_row("SELECT COUNT(*) FROM papertrail_comments", [], |row| row.get(0)).unwrap();
    assert_eq!(comments, 3);
}

#[test]
fn paginated_repo_comments_advance_only_to_the_first_page_frontier() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ])
    .with_repo_comments(vec![comment("1", "seed", "2026-01-01T00:00:00Z")]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let next = PageCursor { page_token: Some("comments-page-2".to_string()), ..Default::default() };
    let delta = ScriptClient::new(Vec::new())
        .with_probe(FreshnessResult {
            latest: None,
            etag: Some("v1".to_string()),
            not_modified: true,
        })
        .with_repo_comment_pages(vec![
            Ok(CommentsPage {
                comments: vec![comment("1", "first", "2026-01-02T00:00:00Z")],
                next: Some(next),
                frontier: None,
            }),
            Ok(CommentsPage {
                comments: vec![comment("1", "later", "2026-01-04T00:00:00Z")],
                next: None,
                frontier: None,
            }),
        ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &delta, false))
        .unwrap();

    let cursor = load_cursor(&conn, &binding).unwrap();
    assert_eq!(
        cursor.comment_stream_cursors["default"].high_mark_at.as_deref(),
        Some("2026-01-02T00:00:00Z")
    );
}

#[test]
fn repo_comment_delta_overlaps_its_watermark_and_keeps_the_scan_boundary_across_pages() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ])
    .with_repo_comments(vec![comment("1", "first", "2026-01-02T00:00:00Z")]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let next = PageCursor { page_token: Some("comments-page-2".to_string()), ..Default::default() };
    let delta = ScriptClient::new(Vec::new())
        .with_probe(FreshnessResult {
            latest: None,
            etag: Some("v1".to_string()),
            not_modified: true,
        })
        .with_repo_comment_pages(vec![
            Ok(CommentsPage { comments: Vec::new(), next: Some(next), frontier: None }),
            Ok(CommentsPage { comments: Vec::new(), next: None, frontier: None }),
        ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &delta, false))
        .unwrap();
    let requests = delta.repo_comment_requests.borrow();
    assert_eq!(requests[0].updated_since.as_deref(), Some("2026-01-01T23:59:59Z"));
    assert_eq!(requests[1].updated_since, requests[0].updated_since);
}

#[test]
fn independent_comment_streams_do_not_advance_an_unscanned_stream() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ])
    .with_comment_streams(&["issue_comments", "review_comments"])
    .with_repo_comment_pages(vec![
        Ok(CommentsPage {
            comments: vec![comment("1", "issue", "2026-01-01T00:00:00Z")],
            next: None,
            frontier: None,
        }),
        Ok(CommentsPage {
            comments: vec![comment("1", "review", "2026-01-03T00:00:00Z")],
            next: None,
            frontier: None,
        }),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let cursor = load_cursor(&conn, &binding).unwrap();
    assert_eq!(
        cursor.comment_stream_cursors["issue_comments"].high_mark_at.as_deref(),
        Some("2026-01-01T00:00:00Z")
    );
    assert_eq!(
        cursor.comment_stream_cursors["review_comments"].high_mark_at.as_deref(),
        Some("2026-01-03T00:00:00Z")
    );
    assert_eq!(cursor.comment_high_mark_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    let initial_requests = initial.repo_comment_requests.borrow();
    assert!(initial_requests.iter().all(|request| request.updated_since.is_none()));

    let next = ScriptClient::new(Vec::new())
        .with_comment_streams(&["issue_comments", "review_comments"])
        .with_probe(FreshnessResult {
            latest: None,
            etag: Some("v1".to_string()),
            not_modified: true,
        })
        .with_repo_comment_pages(vec![
            Ok(CommentsPage {
                comments: vec![comment("1", "late-issue", "2026-01-02T00:00:00Z")],
                next: None,
                frontier: None,
            }),
            Ok(CommentsPage { comments: Vec::new(), next: None, frontier: None }),
        ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &next, false))
        .unwrap();
    let requests = next.repo_comment_requests.borrow();
    assert_eq!(requests[0].stream.as_deref(), Some("issue_comments"));
    assert_eq!(requests[0].updated_since.as_deref(), Some("2025-12-31T23:59:59Z"));
    assert_eq!(requests[1].stream.as_deref(), Some("review_comments"));
    assert_eq!(requests[1].updated_since.as_deref(), Some("2026-01-02T23:59:59Z"));
}

#[test]
fn item_thread_snapshots_do_not_advance_the_repo_comment_watermark() {
    let conn = db();
    let binding = binding(&[]);
    let client = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ])
    .with_item_comments(vec![comment("1", "thread", "2026-02-01T00:00:00Z")]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
        .unwrap();
    assert_eq!(client.repo_comment_requests.borrow()[0].updated_since, None);
    assert_eq!(load_cursor(&conn, &binding).unwrap().comment_high_mark_at, None);
}
