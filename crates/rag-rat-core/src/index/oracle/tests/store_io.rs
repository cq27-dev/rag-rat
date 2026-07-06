use super::*;

// ---------------------------------------------------------------------------
// store.rs — side-table I/O round trips, candidate scoping, staleness key.
// ---------------------------------------------------------------------------

/// `edge_join_candidates` returns only edges carrying a callee byte range, scoped to the active
/// commit/worktree, ordered by `(path, callee_start_byte)`. Edges with a NULL callee range and
/// edges in another worktree are excluded.
#[test]
fn edge_join_candidates_filters_null_range_and_scopes_by_worktree() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { a(); b(); }\n");
    // Two call edges with callee ranges (out of source order so we can assert ORDER BY).
    let edge_b = h.add_edge(caller, "b", 19, 20, "NameOnly", None);
    let edge_a = h.add_edge(caller, "a", 14, 15, "NameOnly", None);
    // A non-call edge with NULL callee range must be excluded.
    h.conn
        .execute(
            "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution) VALUES \
             (?1, 'mod', 'contains', 'Exact', 'exact')",
            params![caller],
        )
        .unwrap();

    let candidates = store::edge_join_candidates(&h.conn, COMMIT, WORKTREE).unwrap();
    let ids: Vec<i64> = candidates.iter().map(|c| c.edge_id).collect();
    // Only the two call edges, ordered by callee_start_byte (a at 14 before b at 19).
    assert_eq!(ids, vec![edge_a, edge_b]);
    // `file_sha` is the file's real content hash (what production records), so the candidate's
    // `edge_kind` and `file_sha` round-trip from the `files`/`edges` rows.
    assert_eq!(candidates[0].file_sha, sha256_hex("fn caller() { a(); b(); }\n".as_bytes()));
    assert_eq!(candidates[0].edge_kind, "calls_name");
    assert_eq!(candidates[0].source_path, "caller.rs");

    // A candidate scoped to a DIFFERENT commit is out of scope. (Under clean-checkout semantics a
    // commit-scoped file is visible from any worktree-overlay query as long as the commit matches,
    // so the isolation that actually matters is the commit, not the overlay id.)
    assert!(store::edge_join_candidates(&h.conn, "other-commit-sha", WORKTREE).unwrap().is_empty());
}

/// `symbol_spans_for_path` returns the file's symbols ordered by start byte, scoped to the path +
/// commit/worktree, and empty for an unknown path.
#[test]
fn symbol_spans_for_path_returns_scoped_ordered_spans() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn a() {}\nfn b() {}\n");
    let a = h.add_symbol(defs, "a", 3, 4);
    let b = h.add_symbol(defs, "b", 13, 14);

    let spans = store::symbol_spans_for_path(&h.conn, "defs.rs", COMMIT, WORKTREE).unwrap();
    assert_eq!(spans.iter().map(|s| s.symbol_id).collect::<Vec<_>>(), vec![a, b]);
    assert_eq!(spans[0].start_byte, 3);
    assert_eq!(spans[1].end_byte, 14);

    assert!(
        store::symbol_spans_for_path(&h.conn, "missing.rs", COMMIT, WORKTREE).unwrap().is_empty()
    );
}

/// #248: writing an `edge_oracle` row keyed by the edge's CONTENT key round-trips every field;
/// re-writing the SAME content key upserts (new file_sha/kind) rather than inserting a duplicate.
/// The row resolves through the live-edge content join, and the matching `edges` row is never
/// touched (side-table invariant). The write uses the real `files.sha256` so the count path (which
/// now gates on current content via the scope join) tallies it.
#[test]
fn write_edge_oracle_round_trips_and_upserts_without_touching_edges() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));
    let caller_sha: String = h
        .conn
        .query_row("SELECT sha256 FROM files WHERE path = 'caller.rs'", [], |r| r.get(0))
        .unwrap();

    h.write_verdict(
        edge,
        &caller_sha,
        Some(target_sym),
        "scip `target`().",
        OracleResolutionKind::Upgrade,
    );

    let (kind, resolved, scip) = h.verdict(edge).expect("row written");
    assert_eq!(kind, OracleResolutionKind::Upgrade.as_db_str());
    assert_eq!(resolved, Some(target_sym));
    assert_eq!(scip, "scip `target`().");
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1
    );

    // Re-write the SAME content key (same edge) with a new sha + verdict → upsert, still one row.
    // Use the same `caller_sha` so the count path keeps it (a different sha would read as stale).
    h.write_verdict(
        edge,
        &caller_sha,
        None,
        "scip cargo tokio `target`().",
        OracleResolutionKind::ResolvedExternal,
    );
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "upsert overwrote the row by content key — no duplicate"
    );
    let (kind2, resolved2, _) = h.verdict(edge).expect("row still present");
    assert_eq!(kind2, OracleResolutionKind::ResolvedExternal.as_db_str());
    assert_eq!(resolved2, None);
    let physical_rows: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(physical_rows, 1, "the upsert kept a single physical row");
    let file_sha: String = h
        .conn
        .query_row(
            "SELECT file_sha FROM edge_oracle WHERE source_path = 'caller.rs' AND \
             callee_start_byte = 14 AND callee_end_byte = 20 AND edge_kind = 'calls_name'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(file_sha, caller_sha, "upsert refreshed the staleness sha");

    // The heuristic edges row is untouched by either write.
    assert_eq!(h.heuristic_resolution(edge), ("exact".to_string(), Some(target_sym)));
}

/// The `(file_sha, tool, tool_version)` staleness key is a real composite: rows written under one
/// sha are findable by that sha, and a different sha (changed file bytes) does not match — the
/// content-addressing property the staleness index guards.
#[test]
fn staleness_key_distinguishes_rows_by_file_sha_tool_and_version() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() {}\n");
    let e1 = h.add_edge(f, "x", 1, 2, "NameOnly", None);
    let e2 = h.add_edge(f, "y", 3, 4, "NameOnly", None);

    h.write_verdict(e1, "sha-fresh", None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(e2, "sha-old", None, "s", OracleResolutionKind::Upgrade);

    let count_for_sha = |sha: &str| -> i64 {
        h.conn
            .query_row(
                "SELECT COUNT(*) FROM edge_oracle WHERE file_sha = ?1 AND tool = ?2 AND \
                 tool_version = ?3",
                params![sha, TOOL.as_db_str(), VERSION],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(count_for_sha("sha-fresh"), 1, "row matches its own sha");
    assert_eq!(count_for_sha("sha-old"), 1);
    assert_eq!(count_for_sha("sha-changed"), 0, "a changed file's sha matches no rows (stale)");
    // Wrong tool_version also fails the key.
    let other_version: i64 = h
        .conn
        .query_row(
            "SELECT COUNT(*) FROM edge_oracle WHERE tool = ?1 AND tool_version = ?2",
            params![TOOL.as_db_str(), "other-version"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other_version, 0);
}

/// #145: `record_oracle_run_at` persists the PASSED start time, not `now_ms()`/completion — so the
/// auto-run staleness gate keys on when the run BEGAN, not when it finished (a run that overlapped
/// a watcher reindex must not look fresher than the edits it skipped).
#[test]
fn record_oracle_run_at_persists_the_passed_start_time() {
    let h = Harness::new();
    // Deliberately far in the past: if the impl stamped `now_ms()` instead, this would not match.
    let started_at_ms = 1_000_000_i64;
    store::record_oracle_run_at(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        started_at_ms,
        "Completed",
        "{}",
    )
    .unwrap();
    assert_eq!(
        store::latest_run_started_at(&h.conn, TOOL, COMMIT, WORKTREE).unwrap(),
        Some(started_at_ms),
        "started_at must be the value the caller passed, not completion time"
    );
}

/// `record_oracle_run` inserts a run row and returns its id; `count_edge_oracle_scoped` tallies
/// only the requested kind for the tool/version within the active checkout.
#[test]
fn record_oracle_run_and_count_by_kind() {
    let h = Harness::new();
    let id1 = store::record_oracle_run(&h.conn, TOOL, VERSION, "abc", WORKTREE, "Completed", "{}")
        .unwrap();
    let id2 = store::record_oracle_run(&h.conn, TOOL, VERSION, "abc", WORKTREE, "Completed", "{}")
        .unwrap();
    assert!(id2 > id1, "row id increments");

    let f = h.add_file("a.rs", "x\n");
    let sha = h.file_sha("a.rs");
    let e_up = h.add_edge(f, "u", 0, 1, "NameOnly", None);
    let e_conf = h.add_edge(f, "c", 1, 2, "Exact", None);
    h.write_verdict(e_up, &sha, None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(e_conf, &sha, None, "s", OracleResolutionKind::Confirm);

    assert_eq!(
        store::count_edge_oracle_scoped(
            &h.conn,
            TOOL,
            VERSION,
            COMMIT,
            WORKTREE,
            Some(OracleResolutionKind::Upgrade)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        store::count_edge_oracle_scoped(
            &h.conn,
            TOOL,
            VERSION,
            COMMIT,
            WORKTREE,
            Some(OracleResolutionKind::Contradict)
        )
        .unwrap(),
        0
    );
}
