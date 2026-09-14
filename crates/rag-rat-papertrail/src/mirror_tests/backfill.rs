use super::*;

#[test]
fn lifo_backfill_resumes_after_pause_without_gaps_or_duplicates() {
    let conn = db();
    let binding = binding(&[]);
    let paused = anyhow::Error::new(TransportError::Paused {
        resume_at_ms: 42,
        reason: PauseReason::QuotaReserve,
    });
    let first = ScriptClient::new(vec![
        page(vec![
            item("3", "2026-01-03T00:00:00Z", "three", &[]),
            item("2", "2026-01-02T00:00:00Z", "two", &[]),
        ]),
        Err(paused),
    ]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
            .unwrap();
    assert_eq!(report.paused_until_ms, Some(42));
    assert_eq!(keys(&conn), vec!["2", "3"]);

    let second = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &second, false))
        .unwrap();
    assert_eq!(keys(&conn), vec!["1", "2", "3"]);
    let distinct: i64 = conn
        .query_row("SELECT COUNT(DISTINCT item_key) FROM papertrail_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(distinct, 3);
}

#[test]
fn search_tie_pages_commit_and_resume_from_the_opaque_provider_cursor() {
    let conn = db();
    let binding = binding(&[]);
    let continuation = PageCursor {
        stream: Some("search_backfill".to_string()),
        updated_before: Some("2026-01-02T00:00:00Z".to_string()),
        page_token: Some("tie-page-2".to_string()),
        provider_state: Some("opaque-tie-state".to_string()),
        ..PageCursor::default()
    };
    let first = ScriptClient::new(vec![
        Ok(ItemsPage {
            items: vec![item("2", "2026-01-02T00:00:00Z", "two", &[])],
            next: Some(continuation),
            backfill_boundary: None,
        }),
        Err(anyhow::Error::new(TransportError::Paused {
            resume_at_ms: 42,
            reason: PauseReason::QuotaReserve,
        })),
    ]);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
            .unwrap();
    assert_eq!(report.paused_until_ms, Some(42));
    assert_eq!(keys(&conn), vec!["2"]);
    assert_eq!(
        load_cursor(&conn, &binding)
            .unwrap()
            .backfill_page_cursor
            .and_then(|cursor| cursor.page_token),
        Some("tie-page-2".to_string())
    );

    let resumed = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-02T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
        .unwrap();
    let request = &resumed.item_page_requests.borrow()[0];
    assert_eq!(request.page_token.as_deref(), Some("tie-page-2"));
    assert_eq!(request.provider_state.as_deref(), Some("opaque-tie-state"));
    assert_eq!(keys(&conn), vec!["1", "2"]);
}

#[test]
fn compound_provider_boundary_advances_below_every_physical_stream_page() {
    let conn = db();
    let binding = binding(&[]);
    let client = ScriptClient::new(vec![
        Ok(ItemsPage {
            items: vec![item("5", "2026-01-05T00:00:00Z", "pull", &[])],
            next: None,
            backfill_boundary: Some("2026-01-01T00:00:00Z".to_string()),
        }),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
        .unwrap();

    let requests = client.item_page_requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].updated_before.as_deref(), Some("2026-01-01T00:00:00Z"));
}
