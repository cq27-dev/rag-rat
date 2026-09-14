use super::*;

#[test]
fn provider_pagination_must_advance_some_cursor_state() {
    let current = PageCursor { page_token: Some("page-2".to_string()), ..PageCursor::default() };
    assert!(ensure_cursor_advanced(&current, &current, "test").is_err());

    let mut next = current.clone();
    next.provider_state = Some("opaque-state".to_string());
    ensure_cursor_advanced(&current, &next, "test").unwrap();
}

/// Namespaced numbering (GitLab): issue #N and change request !N share a key. The repo
/// comment lane must resolve the parent by the comment's OWN kind first — a key-only lookup
/// hitches the comment to whichever twin the scan returns first and then rewrites the
/// correctly-kinded row through the kind-less comment conflict key.
/// GitLab emits millisecond timestamps; the overlap rewind must handle the fraction —
/// refusing to parse it silently returned the input unchanged (ZERO overlap), and with a
/// strict updated_after filter the boundary row became unreachable, so replay convergence
/// could never complete.
#[test]
fn overlap_timestamp_rewinds_fractional_second_stamps() {
    assert_eq!(overlap_timestamp("2026-07-15T13:15:56.837Z"), "2026-07-15T13:15:55Z");
    assert_eq!(overlap_timestamp("2026-01-01T00:00:00.001Z"), "2025-12-31T23:59:59Z");
    // Non-fractional stamps keep their existing behavior.
    assert_eq!(overlap_timestamp("2026-07-15T13:15:56Z"), "2026-07-15T13:15:55Z");
}

#[test]
fn previous_civil_day_rolls_back_through_month_year_and_leap_boundaries() {
    for ((year, month, day), expected) in [
        ((2026, 7, 15), (2026, 7, 14)),
        ((2026, 3, 1), (2026, 2, 28)),
        ((2024, 3, 1), (2024, 2, 29)),
        ((2000, 3, 1), (2000, 2, 29)),
        ((1900, 3, 1), (1900, 2, 28)),
        ((2026, 5, 1), (2026, 4, 30)),
        ((2026, 1, 1), (2025, 12, 31)),
    ] {
        assert_eq!(previous_civil_day(year, month, day), expected, "{year}-{month}-{day}");
    }
}

#[test]
fn processed_item_cursor_json_keeps_the_kind_tokens_and_tolerates_drifted_ones() {
    let items = BTreeSet::from([
        ProcessedItem { kind: ItemKind::ChangeRequest, key: "2".into(), updated_at: None },
        ProcessedItem { kind: ItemKind::Issue, key: "1".into(), updated_at: Some("t".into()) },
    ]);
    let json = serde_json::to_string(&items).unwrap();
    assert_eq!(
        json,
        r#"[{"kind":"issue","key":"1","updated_at":"t"},{"kind":"change_request","key":"2","updated_at":null}]"#,
        "the persisted kind spellings are the ItemKind tokens"
    );
    assert_eq!(decode_processed_items(Some(json), 16).unwrap(), items);
    let keys = |raw: &str| {
        decode_processed_items(Some(raw.to_string()), 16)
            .unwrap()
            .into_iter()
            .map(|item| item.key)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys(
            r#"[{"kind":"issue","key":"1","updated_at":null},{"kind":"Issue","key":"2","updated_at":null}]"#
        ),
        ["1"],
        "a drifted kind is skipped, never a failed decode"
    );
    assert_eq!(keys(r#"[["issue","1"],["pull","2"]]"#), ["1"], "legacy pairs skip it too");
    let thread = |kind: &str| {
        format!(
            r#"{{"item":{{"kind":"{kind}","key":"1","updated_at":null}},"lane":"delta","stream_index":0,"page_cursor":null}}"#
        )
    };
    assert!(decode_item_thread_cursor(Some(thread("issue")), 15).unwrap().is_some());
    assert!(
        decode_item_thread_cursor(Some(thread("pull")), 15).unwrap().is_none(),
        "a drifted thread kind drops the cursor so the item is re-processed"
    );
}

/// A quiet probe must not starve an OWED boundary replay: providers without a probe
/// validator (GitLab) report a timestamp tie as not_modified, and the conservative frontier
/// left by an interrupted delta would otherwise wait for the daily full walk.
#[test]
fn a_quiet_probe_never_starves_an_owed_replay() {
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

    let mut cursor = load_cursor(&conn, &binding).unwrap();
    cursor.item_delta_replay_required = true;
    save_cursor(&conn, &binding, &cursor, false).unwrap();

    let quiet = ScriptClient::new(vec![Ok(ItemsPage {
        items: Vec::new(),
        next: None,
        backfill_boundary: None,
    })])
    .with_probe(FreshnessResult { latest: None, etag: None, not_modified: true })
    .with_repo_comments(Vec::new());
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &quiet, false))
        .unwrap();

    let requests = quiet.item_page_requests.borrow();
    assert_eq!(requests.len(), 1, "the owed replay must run despite the quiet probe");
    assert!(requests[0].updated_since.is_some(), "the replay is a delta scan");
}

#[test]
fn empty_initial_project_enters_delta_polling_and_discovers_later_items() {
    let conn = db();
    let binding = binding(&[]);
    block_on(mirror_binding(
        &conn,
        &binding,
        std::slice::from_ref(&binding),
        &ScriptClient::new(vec![page(Vec::new())]),
        false,
    ))
    .unwrap();
    let cursor = load_cursor(&conn, &binding).unwrap();
    assert!(cursor.backfill_done);
    assert_eq!(cursor.high_mark_at.as_deref(), Some(EMPTY_PROJECT_HIGH_MARK));

    let later =
        ScriptClient::new(vec![page(vec![item("1", "2026-01-01T00:00:00Z", "later", &[])])])
            .with_probe(FreshnessResult {
                latest: Some("2026-01-01T00:00:00Z".to_string()),
                etag: Some("v1".to_string()),
                not_modified: false,
            });
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &later, false))
        .unwrap();
    assert_eq!(keys(&conn), vec!["1"]);
    assert_eq!(
        later.item_page_requests.borrow()[0].updated_since.as_deref(),
        Some("1969-12-31T23:59:59Z")
    );
}

#[test]
fn delta_catches_an_item_updated_while_backfill_is_paused() {
    let conn = db();
    let binding = binding(&[]);
    let first = ScriptClient::new(vec![
        page(vec![item("2", "2026-01-02T00:00:00Z", "old", &[])]),
        Err(anyhow::Error::new(TransportError::Paused {
            resume_at_ms: 42,
            reason: PauseReason::PassBudget,
        })),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
        .unwrap();
    let second = ScriptClient::new(vec![
        page(vec![item("2", "2026-01-04T00:00:00Z", "updated", &[])]),
        page(Vec::new()),
        page(Vec::new()),
    ])
    .with_probe(FreshnessResult {
        latest: Some("2026-01-04T00:00:00Z".to_string()),
        etag: Some("v2".to_string()),
        not_modified: false,
    });
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &second, false))
        .unwrap();
    let title: String = conn
        .query_row("SELECT title FROM papertrail_items WHERE item_key='2'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(title, "updated");
}

#[test]
fn changed_etag_replays_the_overlap_even_when_latest_timestamp_is_tied() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-02T00:00:00Z", "old", &[])]),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let tied =
        ScriptClient::new(vec![page(vec![item("2", "2026-01-02T00:00:00Z", "same-second", &[])])])
            .with_probe(FreshnessResult {
                latest: None,
                etag: Some("changed".to_string()),
                not_modified: false,
            });
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &tied, false))
        .unwrap();
    assert_eq!(keys(&conn), vec!["1", "2"]);
    assert_eq!(
        load_cursor(&conn, &binding).unwrap().high_mark_at.as_deref(),
        Some("2026-01-02T00:00:00Z")
    );
}

#[test]
fn item_delta_persists_the_next_page_before_a_pause() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let next = PageCursor {
        updated_since: Some("2025-12-31T23:59:59Z".to_string()),
        page_token: Some("delta-page-2".to_string()),
        ..PageCursor::default()
    };
    let paused = ScriptClient::new(vec![
        Ok(ItemsPage {
            items: vec![item("2", "2026-01-02T00:00:00Z", "two", &[])],
            next: Some(next),
            backfill_boundary: None,
        }),
        Err(anyhow::Error::new(TransportError::Paused {
            resume_at_ms: 42,
            reason: PauseReason::PassBudget,
        })),
    ])
    .with_probe(FreshnessResult {
        latest: Some("2026-01-02T00:00:00Z".to_string()),
        etag: Some("v2".to_string()),
        not_modified: false,
    });
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &paused, false))
            .unwrap();
    assert_eq!(report.paused_until_ms, Some(42));
    let cursor = load_cursor(&conn, &binding).unwrap();
    assert!(cursor.item_delta_in_progress);
    assert_eq!(cursor.item_delta_page_token.as_deref(), Some("delta-page-2"));

    let resumed =
        ScriptClient::new(vec![page(vec![item("3", "2026-01-03T00:00:00Z", "three", &[])])]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
        .unwrap();
    assert_eq!(resumed.item_page_requests.borrow()[0].page_token.as_deref(), Some("delta-page-2"));
    assert_eq!(
        resumed.item_page_requests.borrow()[0].updated_since.as_deref(),
        Some("2025-12-31T23:59:59Z")
    );
    assert_eq!(keys(&conn), vec!["1", "2", "3"]);
}

#[test]
fn paginated_item_delta_advances_only_to_the_first_page_frontier() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let next = PageCursor { page_token: Some("delta-page-2".to_string()), ..Default::default() };
    let delta = ScriptClient::new(vec![
        Ok(ItemsPage {
            items: vec![item("2", "2026-01-02T00:00:00Z", "two", &[])],
            next: Some(next),
            backfill_boundary: None,
        }),
        page(vec![item("3", "2026-01-04T00:00:00Z", "three", &[])]),
    ])
    .with_probe(FreshnessResult {
        latest: Some("2026-01-05T00:00:00Z".to_string()),
        etag: Some("v2".to_string()),
        not_modified: false,
    });
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &delta, false))
        .unwrap();

    let cursor = load_cursor(&conn, &binding).unwrap();
    assert_eq!(cursor.high_mark_at.as_deref(), Some("2026-01-02T00:00:00Z"));
    assert!(cursor.probe_etag.is_none(), "the conservative frontier must force a replay");
    assert_eq!(keys(&conn), vec!["1", "2", "3"]);

    let replay = ScriptClient::new(vec![page(vec![item(
        "4",
        "2026-01-03T00:00:00Z",
        "shifted boundary row",
        &[],
    )])])
    .with_probe(FreshnessResult {
        latest: Some("2026-01-05T00:00:00Z".to_string()),
        etag: Some("v2".to_string()),
        not_modified: false,
    });
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &replay, false))
        .unwrap();
    assert_eq!(keys(&conn), vec!["1", "2", "3", "4"]);
}

#[test]
fn item_delta_does_not_advance_to_an_unobserved_probe_timestamp() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let raced = ScriptClient::new(vec![page(Vec::new())]).with_probe(FreshnessResult {
        latest: Some("2026-01-05T00:00:00Z".to_string()),
        etag: Some("v2".to_string()),
        not_modified: false,
    });
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &raced, false))
        .unwrap();

    let cursor = load_cursor(&conn, &binding).unwrap();
    assert_eq!(cursor.high_mark_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert!(cursor.probe_etag.is_none());
}

#[test]
fn a_stable_paginated_tie_settles_after_one_forced_replay() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let tied_page = || {
        Ok(ItemsPage {
            items: vec![item("2", "2026-01-02T00:00:00Z", "tie", &[])],
            next: Some(PageCursor {
                page_token: Some("tie-page-2".to_string()),
                ..PageCursor::default()
            }),
            backfill_boundary: None,
        })
    };
    let first =
        ScriptClient::new(vec![tied_page(), page(Vec::new())]).with_probe(FreshnessResult {
            latest: Some("2026-01-02T00:00:00Z".to_string()),
            etag: Some("v2".to_string()),
            not_modified: false,
        });
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
        .unwrap();
    let cursor = load_cursor(&conn, &binding).unwrap();
    assert!(cursor.item_delta_replay_required);
    assert!(cursor.probe_etag.is_none());

    let replay =
        ScriptClient::new(vec![tied_page(), page(Vec::new())]).with_probe(FreshnessResult {
            latest: None,
            etag: Some("v2".to_string()),
            not_modified: false,
        });
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &replay, false))
        .unwrap();
    let cursor = load_cursor(&conn, &binding).unwrap();
    assert!(!cursor.item_delta_replay_required);
    assert_eq!(cursor.probe_etag.as_deref(), Some("v2"));
}

#[test]
fn continuation_classification_covers_every_persisted_resume_lane() {
    assert_eq!(MirrorCursor::default().continuation(), MirrorContinuation::None);
    let mut cursor = MirrorCursor { backfill_done: true, ..Default::default() };
    assert_eq!(cursor.continuation(), MirrorContinuation::None);

    cursor.item_delta_replay_required = true;
    assert_eq!(cursor.continuation(), MirrorContinuation::Incremental);
    cursor.item_delta_replay_required = false;
    cursor.comment_stream_cursors.insert("default".to_string(), CommentStreamCursor {
        page_token: Some("next".to_string()),
        ..Default::default()
    });
    assert_eq!(cursor.continuation(), MirrorContinuation::Incremental);

    cursor.full_rewalk = true;
    assert_eq!(cursor.continuation(), MirrorContinuation::Full);

    let partial_backfill = MirrorCursor {
        low_mark_at: Some("2026-01-01T00:00:00Z".to_string()),
        ..Default::default()
    };
    assert_eq!(partial_backfill.continuation(), MirrorContinuation::Incremental);
}

#[test]
fn non_pause_errors_propagate_and_pause_classifier_ignores_them() {
    let conn = db();
    let binding = binding(&[]);
    let client = ScriptClient::new(vec![Err(anyhow::anyhow!("provider failed"))]);
    let error =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap_err();
    assert_eq!(error.to_string(), "provider failed");
    assert!(pause(&error).is_none());

    let unused = block_on(client.item("o/r", ItemKind::Issue, "1")).unwrap_err();
    assert_eq!(unused.to_string(), "unused");
}

#[test]
fn invalid_item_delta_continuation_is_not_persisted() {
    let conn = db();
    let binding = binding(&[]);
    let initial = ScriptClient::new(vec![
        page(vec![item("1", "2026-01-01T00:00:00Z", "initial", &[])]),
        page(Vec::new()),
    ]);
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
        .unwrap();

    let unchanged = PageCursor {
        updated_since: Some("2025-12-31T23:59:59Z".to_string()),
        ..PageCursor::default()
    };
    let invalid = ScriptClient::new(vec![Ok(ItemsPage {
        items: vec![item("2", "2026-01-02T00:00:00Z", "must-not-commit", &[])],
        next: Some(unchanged),
        backfill_boundary: None,
    })])
    .with_probe(FreshnessResult {
        latest: Some("2026-01-02T00:00:00Z".to_string()),
        etag: Some("v2".to_string()),
        not_modified: false,
    });
    let error =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &invalid, false))
            .unwrap_err();
    assert!(error.to_string().contains("did not advance"));
    let cursor = load_cursor(&conn, &binding).unwrap();
    assert_eq!(cursor.item_delta_page_token, None);
    assert!(cursor.item_delta_in_progress, "the valid scan start remains retryable");
    assert_eq!(keys(&conn), vec!["1"]);
}

#[test]
fn delta_overlap_rewinds_one_second_without_changing_provider_tokens() {
    assert_eq!(overlap_timestamp("2026-01-02T00:00:00Z"), "2026-01-01T23:59:59Z");
    assert_eq!(overlap_timestamp("2026-01-01T00:00:01Z"), "2026-01-01T00:00:00Z");
    assert_eq!(overlap_timestamp("2026-01-01T00:01:00Z"), "2026-01-01T00:00:59Z");
    assert_eq!(overlap_timestamp("2026-01-01T01:00:00Z"), "2026-01-01T00:59:59Z");
    assert_eq!(overlap_timestamp("2026-05-01T00:00:00Z"), "2026-04-30T23:59:59Z");
    assert_eq!(overlap_timestamp("2024-03-01T00:00:00Z"), "2024-02-29T23:59:59Z");
    assert_eq!(overlap_timestamp("2026-03-01T00:00:00Z"), "2026-02-28T23:59:59Z");
    assert_eq!(overlap_timestamp("2026-01-01T00:00:00Z"), "2025-12-31T23:59:59Z");
    assert_eq!(overlap_timestamp("2026-01-01TinvalidZ"), "2026-01-01TinvalidZ");
    assert_eq!(overlap_timestamp("provider-token"), "provider-token");

    let outside = anyhow::Error::new(TransportError::UrlOutsideBinding {
        url: "https://other.example".to_string(),
        host: "github.example".to_string(),
        problem: "origin differs",
    });
    assert!(pause(&outside).is_none());

    let legacy = decode_processed_items(Some(r#"[["issue","1"]]"#.to_string()), 0).unwrap();
    assert!(legacy.contains(&ProcessedItem {
        kind: ItemKind::Issue,
        key: "1".to_string(),
        updated_at: None,
    }));
}
