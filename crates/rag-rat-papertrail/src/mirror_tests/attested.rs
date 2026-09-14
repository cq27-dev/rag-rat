use super::*;

/// A filter change must RESET the attested-closers watermark: a widened filter caches
/// newly-in-scope closed issues whose PRs/issues predate the stored `since`, and a reused
/// watermark would stop the attested walk before ever visiting them. An unchanged filter must
/// leave the watermark intact so the incremental walk stays incremental (#727 review).
#[test]
fn a_filter_change_resets_the_attested_watermark_but_a_stable_filter_keeps_it() {
    let conn = db();
    let binding = binding(&[]);
    let key = attested_since_key(&binding, &conn).unwrap();

    // Persist a cursor whose stored fingerprint will NOT match the binding's, forcing the
    // next sync onto the filter-changed path.
    let mut cursor = load_cursor(&conn, &binding).unwrap();
    cursor.filter_fingerprint = "stale-fingerprint".to_string();
    save_cursor(&conn, &binding, &cursor, false).unwrap();
    rag_rat_db::meta::set_meta(&conn, &key, "2020-01-01T00:00:00Z").unwrap();

    let changed =
        ScriptClient::new(vec![page(Vec::new()), page(Vec::new())]).with_repo_comments(Vec::new());
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &changed, false))
        .unwrap();
    assert!(
        rag_rat_db::meta::read_meta(&conn, &key).unwrap().is_none(),
        "the widened-filter path clears the attested watermark",
    );

    // The prior run stored the binding's own fingerprint, so a repeat sync sees no change:
    // the freshly re-seeded watermark must survive.
    rag_rat_db::meta::set_meta(&conn, &key, "2021-01-01T00:00:00Z").unwrap();
    let stable = ScriptClient::new(vec![page(Vec::new()), page(Vec::new())])
        .with_probe(FreshnessResult { latest: None, etag: None, not_modified: true })
        .with_repo_comments(Vec::new());
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &stable, false))
        .unwrap();
    assert_eq!(
        rag_rat_db::meta::read_meta(&conn, &key).unwrap().as_deref(),
        Some("2021-01-01T00:00:00Z"),
        "a stable filter leaves the attested watermark untouched",
    );
}

/// A FULL rewalk re-caches every closed issue, so it must clear the attested watermark for the
/// same reason a widened filter does — otherwise the attested walk reads a stale `since` and
/// silently never re-fetches provider closers for issues whose closer predates it. This is the
/// full-rewalk seam in isolation: the filter is unchanged, so ONLY `reset_for_full_rewalk` can
/// do the clearing.
#[test]
fn a_full_rewalk_clears_the_attested_watermark_even_with_an_unchanged_filter() {
    let conn = db();
    let binding = binding(&[]);
    let key = attested_since_key(&binding, &conn).unwrap();
    rag_rat_db::meta::set_meta(&conn, &key, "2026-01-01T00:00:00Z").unwrap();

    // full=true on a fresh cursor ⇒ starting_full_rewalk; tags=[] matches the stored empty
    // fingerprint ⇒ filter_changed=false, so the filter-change clear cannot fire.
    let full =
        ScriptClient::new(vec![page(Vec::new()), page(Vec::new())]).with_repo_comments(Vec::new());
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &full, true)).unwrap();

    assert!(
        rag_rat_db::meta::read_meta(&conn, &key).unwrap().is_none(),
        "reset_for_full_rewalk must clear the attested watermark",
    );
}

#[test]
fn attested_closers_walk_stores_upgrades_and_watermarks() {
    let conn = db();
    let binding = binding(&[]);
    // A pre-existing TEXT-tier edge for the same pair: the provider walk must upgrade it.
    crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
        project: "o/r".into(),
        issue_kind: ItemKind::Issue,
        issue_key: "5".into(),
        closer_kind: crate::CloserKind::ChangeRequest,
        closer_key: "9".into(),
        closer_commit: None,
        source: crate::ClosingEdgeSource::Text,
    })
    .unwrap();
    cache_closed_issue(&conn, "5");
    let client = ScriptClient::new(vec![page(Vec::new())]);
    client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
        edges: vec![crate::ClosingEdge {
            project: "o/r".into(),
            issue_kind: ItemKind::Issue,
            issue_key: "5".into(),
            closer_kind: crate::CloserKind::ChangeRequest,
            closer_key: "9".into(),
            closer_commit: Some("abc123".into()),
            source: crate::ClosingEdgeSource::Provider,
        }],
        item_updates: Vec::new(),
        replaced_issue_closers: Vec::new(),
        next: Some("issues".into()),
        frontier: Some("2026-01-05T00:00:00Z".into()),
    }));
    client.attested.borrow_mut().push_back(Some(AttestedClosersPage::default()));
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
    assert_eq!(report.attested_edges, 1);
    let edges =
        crate::store::closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5")
            .unwrap();
    assert_eq!(edges.len(), 1, "text and provider tiers converge on the pair");
    assert_eq!(edges[0].source, crate::ClosingEdgeSource::Provider);
    assert_eq!(edges[0].closer_commit.as_deref(), Some("abc123"));
    // The COMPLETED walk stamped its watermark.
    let repo_id = rag_rat_db::schema::active_repo_id(&conn).unwrap();
    let since = rag_rat_db::meta::read_meta(
        &conn,
        &format!("papertrail_attested_closers_since:{repo_id}:github:o/r"),
    )
    .unwrap();
    assert_eq!(since.as_deref(), Some("2026-01-05T00:00:00Z"));
}

#[test]
fn attested_item_updates_touch_only_cached_merged_rows() {
    let conn = db();
    let binding = binding(&[]);
    // A cached CLOSED-UNMERGED change request: the attested sha must NOT land on it.
    let mut unmerged = item("9", "2026-01-01T00:00:00Z", "t", &[]);
    unmerged.item_kind = ItemKind::ChangeRequest;
    unmerged.state = "closed".into();
    crate::store::store_item(&conn, Tracker::Github, &unmerged).unwrap();
    let client = ScriptClient::new(vec![page(Vec::new())]);
    client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
        edges: Vec::new(),
        item_updates: vec![crate::AttestedItemUpdate {
            item_kind: ItemKind::ChangeRequest,
            item_key: "9".into(),
            resolution: None,
            merge_commit_sha: Some("attested".into()),
        }],
        replaced_issue_closers: Vec::new(),
        next: None,
        frontier: None,
    }));
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
        .unwrap();
    let sha: Option<String> = conn
        .query_row(
            "SELECT merge_commit_sha FROM papertrail_items WHERE item_key = '9'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sha, None, "merged-only invariant holds against attested updates too");
}

#[test]
fn issue_keyed_replace_set_drops_a_re_read_issues_stale_closer_of_any_kind() {
    let conn = db();
    let binding = binding(&[]);
    cache_closed_issue(&conn, "5");
    // Issue 5's PREVIOUS provider closer was commit `old`; this walk re-reads issue 5 and its
    // ClosedEvent now names PR 9. The issue-keyed replace-set reaps EVERY provider closer for
    // issue 5 (the commit included), so the stale commit closer dies and only the fresh PR
    // closer remains — reaping is keyed by the issue, not by any one closer.
    crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
        project: "o/r".into(),
        issue_kind: ItemKind::Issue,
        issue_key: "5".into(),
        closer_kind: crate::CloserKind::Commit,
        closer_key: "old".into(),
        closer_commit: Some("old".into()),
        source: crate::ClosingEdgeSource::Provider,
    })
    .unwrap();
    let client = ScriptClient::new(vec![page(Vec::new())]);
    client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
        edges: vec![crate::ClosingEdge {
            project: "o/r".into(),
            issue_kind: ItemKind::Issue,
            issue_key: "5".into(),
            closer_kind: crate::CloserKind::ChangeRequest,
            closer_key: "9".into(),
            closer_commit: None,
            source: crate::ClosingEdgeSource::Provider,
        }],
        item_updates: Vec::new(),
        replaced_issue_closers: vec!["5".into()],
        next: None,
        frontier: None,
    }));
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
        .unwrap();
    let edges =
        crate::store::closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5")
            .unwrap();
    assert_eq!(edges.len(), 1, "the stale commit closer died with the refresh");
    assert_eq!(edges[0].closer_kind, crate::CloserKind::ChangeRequest);
    assert_eq!(edges[0].closer_key, "9", "only the fresh authoritative closer remains");
}

/// The reported hazard the issue-keyed model fixes: a UI-linked closure (issue 5 <- PR 9 from
/// ClosedEvent, NOT in PR 9's `closingIssuesReferences`) must SURVIVE a later PR-phase re-read
/// of PR 9. Under the old closer-keyed replace-set, re-reading PR 9 deleted every
/// `change_request` edge with closer 9 — including the issue-phase UI-linked row 5<-9, which
/// the PR phase never re-provides — and issue 5 (older than the watermark) was never revisited
/// to restore it, silently dropping attested closure evidence.
#[test]
fn a_pr_phase_re_read_does_not_clobber_an_issue_phase_ui_linked_edge() {
    let conn = db();
    let binding = binding(&[]);
    cache_closed_issue(&conn, "5");
    // Prior walk stored the UI-linked edge 5<-9 from issue 5's ClosedEvent.
    crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
        project: "o/r".into(),
        issue_kind: ItemKind::Issue,
        issue_key: "5".into(),
        closer_kind: crate::CloserKind::ChangeRequest,
        closer_key: "9".into(),
        closer_commit: Some("mergesha".into()),
        source: crate::ClosingEdgeSource::Provider,
    })
    .unwrap();
    // This incremental walk re-reads PR 9 in the PR phase (it edited after the watermark), but
    // PR 9's closingIssuesReferences does NOT list issue 5 (the closure was UI-linked). Issue 5
    // is older than the watermark, so the issue phase does NOT revisit it:
    // `replaced_issue_closers` is empty and there are no fresh edges.
    let client = ScriptClient::new(vec![page(Vec::new())]);
    client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
        edges: Vec::new(),
        item_updates: Vec::new(),
        replaced_issue_closers: Vec::new(),
        next: None,
        frontier: None,
    }));
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
        .unwrap();
    let edges =
        crate::store::closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5")
            .unwrap();
    assert_eq!(edges.len(), 1, "the UI-linked edge survives a PR-phase re-read of its closer");
    assert_eq!(edges[0].closer_key, "9");
}

/// The PR phase must not RESURRECT a stale closer: if issue 5's authoritative provider closer
/// is already PR 7 (its ClosedEvent moved there after a reopen+reclose) and PR 9 — edited after
/// the watermark — still lists #5 in `closingIssuesReferences` while #5 sits below the
/// watermark (never re-read this walk, so no reap), storing `5<-9` would leave two
/// conflicting provider closers. The conflicting-closer gate suppresses it; the same-closer
/// idempotent case still passes.
#[test]
fn the_pr_phase_does_not_resurrect_a_closer_that_conflicts_with_the_authoritative_one() {
    let conn = db();
    let binding = binding(&[]);
    cache_closed_issue(&conn, "5");
    crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
        project: "o/r".into(),
        issue_kind: ItemKind::Issue,
        issue_key: "5".into(),
        closer_kind: crate::CloserKind::ChangeRequest,
        closer_key: "7".into(),
        closer_commit: None,
        source: crate::ClosingEdgeSource::Provider,
    })
    .unwrap();
    // PR-phase page (no reap: replaced_issue_closers empty) re-adding the stale 5<-9.
    let client = ScriptClient::new(vec![page(Vec::new())]);
    client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
        edges: vec![crate::ClosingEdge {
            project: "o/r".into(),
            issue_kind: ItemKind::Issue,
            issue_key: "5".into(),
            closer_kind: crate::CloserKind::ChangeRequest,
            closer_key: "9".into(),
            closer_commit: None,
            source: crate::ClosingEdgeSource::Provider,
        }],
        item_updates: Vec::new(),
        replaced_issue_closers: Vec::new(),
        next: None,
        frontier: None,
    }));
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
        .unwrap();
    let edges =
        crate::store::closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5")
            .unwrap();
    assert_eq!(edges.len(), 1, "the stale conflicting closer is not resurrected");
    assert_eq!(edges[0].closer_key, "7", "only the authoritative closer remains");
}

#[test]
fn min_frontier_across_phases_cannot_skip_the_older_stream() {
    let conn = db();
    let binding = binding(&[]);
    let client = ScriptClient::new(vec![page(Vec::new())]);
    // PR phase frontier is NEWER than the issue phase's — the stored watermark must be the
    // conservative minimum so the next walk cannot skip issue updates in between.
    client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
        frontier: Some("2026-01-09T00:00:00Z".into()),
        next: Some("issues".into()),
        ..Default::default()
    }));
    client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
        frontier: Some("2026-01-03T00:00:00Z".into()),
        ..Default::default()
    }));
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
        .unwrap();
    let repo_id = rag_rat_db::schema::active_repo_id(&conn).unwrap();
    let since = rag_rat_db::meta::read_meta(
        &conn,
        &format!("papertrail_attested_closers_since:{repo_id}:github:o/r"),
    )
    .unwrap();
    assert_eq!(since.as_deref(), Some("2026-01-03T00:00:00Z"));
}

#[test]
fn a_mid_walk_capability_trip_surfaces_a_partial_signal() {
    let conn = db();
    let binding = binding(&[]);
    let client = ScriptClient::new(vec![page(Vec::new())]);
    // First page stores work and advances to the issues phase; the SECOND call returns
    // `None` (a mid-walk capability trip), so the walk is partial.
    client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
        next: Some("issues".into()),
        frontier: Some("2026-01-05T00:00:00Z".into()),
        ..Default::default()
    }));
    client.attested.borrow_mut().push_back(None);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
    assert!(
        report.attested_error.is_some(),
        "a mid-walk trip is reported, not folded into a clean no-supply result",
    );
    // The watermark did NOT advance: the partial walk redoes from the top next pass.
    let repo_id = rag_rat_db::schema::active_repo_id(&conn).unwrap();
    assert!(
        rag_rat_db::meta::read_meta(
            &conn,
            &format!("papertrail_attested_closers_since:{repo_id}:github:o/r"),
        )
        .unwrap()
        .is_none(),
    );
}

#[test]
fn no_attested_supply_is_a_clean_no_op() {
    let conn = db();
    let binding = binding(&[]);
    let client = ScriptClient::new(vec![page(Vec::new())]);
    // FIRST page is `None` — the provider has no GraphQL supply.
    client.attested.borrow_mut().push_back(None);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
    assert!(report.attested_error.is_none(), "no supply is clean, not an error");
    assert_eq!(report.attested_edges, 0);
}

#[test]
fn attested_edge_for_an_uncached_or_open_target_is_skipped() {
    let conn = db();
    let binding = binding(&[]);
    // #5 is NOT cached (out of scope / not mirrored); #6 is cached but OPEN (reopened).
    conn.execute(
        "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, title, \
         body, synced_at_ms, repo_id, state_normalized) VALUES ('github', 'o/r', 'issue', '6', \
         'u', 'open', 't', 'b', 1, '__unassigned__', 'open')",
        [],
    )
    .unwrap();
    let client = ScriptClient::new(vec![page(Vec::new())]);
    client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
        edges: vec![
            crate::ClosingEdge {
                project: "o/r".into(),
                issue_kind: ItemKind::Issue,
                issue_key: "5".into(),
                closer_kind: crate::CloserKind::Commit,
                closer_key: "a".into(),
                closer_commit: Some("a".into()),
                source: crate::ClosingEdgeSource::Provider,
            },
            crate::ClosingEdge {
                project: "o/r".into(),
                issue_kind: ItemKind::Issue,
                issue_key: "6".into(),
                closer_kind: crate::CloserKind::Commit,
                closer_key: "b".into(),
                closer_commit: Some("b".into()),
                source: crate::ClosingEdgeSource::Provider,
            },
        ],
        ..Default::default()
    }));
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
    assert_eq!(report.attested_edges, 0, "neither an uncached nor an open target gets an edge");
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM papertrail_closing_edges", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn an_attested_pause_propagates_the_resume_time_not_a_generic_error() {
    let conn = db();
    let binding = binding(&[]);
    let client = ScriptClient::new(vec![page(Vec::new())]);
    client.attested_pause.set(true);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
    assert_eq!(report.paused_until_ms, Some(999_000), "the scheduler must honor the resume time");
    assert!(report.attested_error.is_none(), "a pause is a pause, not a swallowed error");
}

/// A HARD (non-pause) attested-walk failure must be PERSISTED so `papertrail_sync_status` shows
/// it — the item mirror records success and clears its own error state, so without a separate
/// persisted signal a doomed enrichment walk re-runs every tick looking healthy. A later clean
/// attested walk clears it.
#[test]
fn a_hard_attested_failure_is_persisted_then_cleared_by_a_clean_walk() {
    let conn = db();
    let binding = binding(&[]);

    let failing = ScriptClient::new(vec![page(Vec::new())]).with_repo_comments(Vec::new());
    failing.attested_hard_error.set(true);
    let report =
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &failing, false))
            .unwrap();
    assert!(report.attested_error.is_some(), "the hard error is surfaced on the report");
    assert!(report.paused_until_ms.is_none(), "a hard error is not a pause");
    assert_eq!(
        read_attested_error(&conn, &binding).unwrap().as_deref(),
        Some("attested walk boom"),
        "the failure is persisted for the durable status snapshot",
    );

    // A subsequent clean attested walk (no supply, no error) clears the persisted failure.
    let clean = ScriptClient::new(vec![page(Vec::new())]).with_repo_comments(Vec::new());
    block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &clean, false))
        .unwrap();
    assert!(
        read_attested_error(&conn, &binding).unwrap().is_none(),
        "a clean attested walk clears the persisted failure",
    );
}
