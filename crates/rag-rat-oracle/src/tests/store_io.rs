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

/// `current_callee_monikers` (#275, Plan 3) returns only span→moniker entries a clone-refine
/// collapse may trust: rows whose `file_sha` matches the requested content hash (a drifted file
/// matches nothing), never a document-scoped `local N` symbol, and never a span two rows disagree
/// on (a multi-tool conflict has no trustworthy identity).
#[test]
fn current_callee_monikers_filters_sha_locals_and_conflicts() {
    let h = Harness::new();
    let src = "fn caller() { keep(); conflicted(); stale(); loc(); }\n";
    let file = h.add_file("m.rs", src);
    let sha = h.file_sha("m.rs");
    let span = |name: &str| {
        let start = src.find(name).unwrap();
        (start, start + name.len())
    };
    // The currency gate only trusts rows the LATEST run of their tool stands behind — record a
    // completed run for BOTH tools so every row below is in play (the conflict drop must fire
    // on trusted rows, not on rows the run gate already filtered).
    store::record_oracle_run_at(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, 0, "Completed", "{}")
        .unwrap();
    store::record_oracle_run_at(
        &h.conn,
        OracleTool::ScipClang,
        VERSION,
        COMMIT,
        WORKTREE,
        0,
        "Completed",
        "{}",
    )
    .unwrap();

    let (keep_lo, keep_hi) = span("keep");
    let keep_edge = h.add_edge(file, "keep", keep_lo, keep_hi, "NameOnly", None);
    h.write_verdict(keep_edge, &sha, None, "rust cr 1.0 keep().", OracleResolutionKind::Upgrade);

    // A span two tools disagree on: the second verdict is written under a DIFFERENT tool with a
    // different symbol → the span is dropped entirely.
    let (con_lo, con_hi) = span("conflicted");
    let con_edge = h.add_edge(file, "conflicted", con_lo, con_hi, "NameOnly", None);
    h.write_verdict(con_edge, &sha, None, "rust cr 1.0 one().", OracleResolutionKind::Upgrade);
    let key = h.edge_content_key(con_edge);
    store::write_edge_oracle(&h.conn, OracleTool::ScipClang, VERSION, &EdgeOracleRow {
        source_path: &key.source_path,
        source_start_byte: key.source_start_byte,
        source_end_byte: key.source_end_byte,
        callee_start_byte: key.callee_start_byte,
        callee_end_byte: key.callee_end_byte,
        edge_kind: &key.edge_kind,
        file_sha: &sha,
        resolved_symbol_id: None,
        scip_symbol: "rust cr 1.0 two().",
        kind: OracleResolutionKind::Upgrade,
    })
    .unwrap();

    // A verdict computed against DIFFERENT bytes (stale sha) — the spans can't be trusted to
    // line up with the requested content, so it is excluded.
    let (stale_lo, stale_hi) = span("stale");
    let stale_edge = h.add_edge(file, "stale", stale_lo, stale_hi, "NameOnly", None);
    h.write_verdict(
        stale_edge,
        "not-the-requested-sha",
        None,
        "rust cr 1.0 stale().",
        OracleResolutionKind::Upgrade,
    );

    // A `local N` symbol is document-scoped — cross-file equality would identify two different
    // functions, so it is never returned.
    let (loc_lo, loc_hi) = span("loc");
    let loc_edge = h.add_edge(file, "loc", loc_lo, loc_hi, "NameOnly", None);
    h.write_verdict(loc_edge, &sha, None, "local 5", OracleResolutionKind::Upgrade);

    let monikers = store::current_callee_monikers(&h.conn, "m.rs", &sha, COMMIT, WORKTREE).unwrap();
    assert_eq!(
        monikers,
        std::collections::HashMap::from([((keep_lo, keep_hi), "rust cr 1.0 keep().".to_string())]),
        "only the sha-current, non-local, conflict-free span may be returned"
    );
}

/// The run/def currency gates on `current_callee_monikers` (#275 Codex round-2): a row is trusted
/// ONLY when (a) the LATEST completed run of its tool in the active checkout stands behind its
/// `tool_version` — superseded-version rows linger by design (`clear_edge_oracle_for_tool` clears
/// only the current scope) and must not lend identity — and (b) its resolved definition, if
/// in-corpus, still exists in the active checkout (the same def-drift gate the surfacing reads
/// apply: an unchanged callsite can point at a deleted/reindexed target).
#[test]
fn current_callee_monikers_drops_superseded_runs_and_dead_defs() {
    let h = Harness::new();
    let src = "fn caller() { old_ver(); no_run(); dead_def(); live_def(); }\n";
    let file = h.add_file("m.rs", src);
    let sha = h.file_sha("m.rs");
    let span = |name: &str| {
        let start = src.find(name).unwrap();
        (start, start + name.len())
    };
    // The latest completed run for TOOL carries VERSION — rows under any other version of TOOL,
    // or under a tool with NO run in this checkout, are not backed by it.
    store::record_oracle_run_at(
        &h.conn,
        TOOL,
        "superseded",
        COMMIT,
        WORKTREE,
        0,
        "Completed",
        "{}",
    )
    .unwrap();
    store::record_oracle_run_at(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, 1, "Completed", "{}")
        .unwrap();

    // (a) A row written under TOOL's SUPERSEDED version: the latest run no longer stands behind
    // it, even though its file_sha is current.
    let (old_lo, old_hi) = span("old_ver");
    let old_edge = h.add_edge(file, "old_ver", old_lo, old_hi, "NameOnly", None);
    let old_key = h.edge_content_key(old_edge);
    store::write_edge_oracle(&h.conn, TOOL, "superseded", &EdgeOracleRow {
        source_path: &old_key.source_path,
        source_start_byte: old_key.source_start_byte,
        source_end_byte: old_key.source_end_byte,
        callee_start_byte: old_key.callee_start_byte,
        callee_end_byte: old_key.callee_end_byte,
        edge_kind: &old_key.edge_kind,
        file_sha: &sha,
        resolved_symbol_id: None,
        scip_symbol: "rust cr 1.0 old().",
        kind: OracleResolutionKind::Upgrade,
    })
    .unwrap();

    // (a') A row under a tool with NO run in this checkout — nothing stands behind it here (a
    // sibling checkout's run must not lend identity to this one).
    let (norun_lo, norun_hi) = span("no_run");
    let norun_edge = h.add_edge(file, "no_run", norun_lo, norun_hi, "NameOnly", None);
    let norun_key = h.edge_content_key(norun_edge);
    store::write_edge_oracle(&h.conn, OracleTool::ScipClang, VERSION, &EdgeOracleRow {
        source_path: &norun_key.source_path,
        source_start_byte: norun_key.source_start_byte,
        source_end_byte: norun_key.source_end_byte,
        callee_start_byte: norun_key.callee_start_byte,
        callee_end_byte: norun_key.callee_end_byte,
        edge_kind: &norun_key.edge_kind,
        file_sha: &sha,
        resolved_symbol_id: None,
        scip_symbol: "cpp . . norun().",
        kind: OracleResolutionKind::Upgrade,
    })
    .unwrap();

    // (b) An in-corpus verdict whose resolved definition no longer exists — the callsite file is
    // unchanged (sha matches) but the target is gone, so the moniker names nothing current.
    let (dead_lo, dead_hi) = span("dead_def");
    let dead_edge = h.add_edge(file, "dead_def", dead_lo, dead_hi, "NameOnly", None);
    h.write_verdict(
        dead_edge,
        &sha,
        Some(999_999),
        "rust cr 1.0 dead().",
        OracleResolutionKind::Upgrade,
    );

    // Control: a current-version verdict whose resolved definition IS live in the active
    // checkout — the only row the collapse may trust.
    let defs = h.add_file("defs.rs", "fn live_def() {}\n");
    let live_sym = h.add_symbol(defs, "live_def", 3, 11);
    let (live_lo, live_hi) = span("live_def");
    let live_edge = h.add_edge(file, "live_def", live_lo, live_hi, "NameOnly", Some(live_sym));
    h.write_verdict(
        live_edge,
        &sha,
        Some(live_sym),
        "rust cr 1.0 live().",
        OracleResolutionKind::Upgrade,
    );

    let monikers = store::current_callee_monikers(&h.conn, "m.rs", &sha, COMMIT, WORKTREE).unwrap();
    assert_eq!(
        monikers,
        std::collections::HashMap::from([((live_lo, live_hi), "rust cr 1.0 live().".to_string())]),
        "superseded-version, run-less-tool, and dead-def rows must all be dropped"
    );
}

/// The call-HEAD kind gate (#275 Codex round-2): `uses_macro` rows participate in the collapse —
/// the classifier treats a macro head exactly like a call head, so a clone class differing only
/// by macro name needs macro verdicts to reach scip mode — while `references_type` rows (which
/// also carry a callee range and join SCIP occurrences) are never callee positions and stay out.
#[test]
fn current_callee_monikers_includes_macro_heads_and_excludes_non_call_kinds() {
    let h = Harness::new();
    let src = "fn caller() { emit!(1); let t: Widget = make(); }\n";
    let file = h.add_file("m.rs", src);
    let sha = h.file_sha("m.rs");
    let span = |name: &str| {
        let start = src.find(name).unwrap();
        (start, start + name.len())
    };
    store::record_oracle_run_at(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, 0, "Completed", "{}")
        .unwrap();

    let (mac_lo, mac_hi) = span("emit");
    let mac_edge =
        h.add_edge_with_kind(file, "emit", mac_lo, mac_hi, "uses_macro", "NameOnly", None);
    h.write_verdict(mac_edge, &sha, None, "rust cr 1.0 emit!.", OracleResolutionKind::Upgrade);

    let (ty_lo, ty_hi) = span("Widget");
    let ty_edge =
        h.add_edge_with_kind(file, "Widget", ty_lo, ty_hi, "references_type", "NameOnly", None);
    h.write_verdict(ty_edge, &sha, None, "rust cr 1.0 Widget#", OracleResolutionKind::Upgrade);

    let monikers = store::current_callee_monikers(&h.conn, "m.rs", &sha, COMMIT, WORKTREE).unwrap();
    assert_eq!(
        monikers,
        std::collections::HashMap::from([((mac_lo, mac_hi), "rust cr 1.0 emit!.".to_string())]),
        "uses_macro is a call-HEAD kind (returned); references_type is not (excluded)"
    );
}

/// The live-edge gate (#512 Codex round-3): a verdict whose content key no longer maps to a LIVE
/// edge — a dangling row surviving a reindex after the extractor stopped emitting that call — is
/// dropped, exactly as the surfacing reads drop it via `edge_oracle_scope_join`. A live-edge-backed
/// verdict on the same file, current sha, and same run is still returned; only the missing edge
/// distinguishes them.
#[test]
fn current_callee_monikers_drops_verdicts_without_a_live_edge() {
    let h = Harness::new();
    let src = "fn caller() { live(); dangling(); }\n";
    let file = h.add_file("m.rs", src);
    let sha = h.file_sha("m.rs");
    let span = |name: &str| {
        let start = src.find(name).unwrap();
        (start, start + name.len())
    };
    store::record_oracle_run_at(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, 0, "Completed", "{}")
        .unwrap();

    // A live edge + its verdict, keyed off the real edge — returned.
    let (live_lo, live_hi) = span("live");
    let live_edge = h.add_edge(file, "live", live_lo, live_hi, "NameOnly", None);
    h.write_verdict(live_edge, &sha, None, "rust cr 1.0 live().", OracleResolutionKind::Upgrade);

    // A dangling verdict: a `calls_name` content key with NO backing edge (no `add_edge`), same
    // file + current sha + same completed run — only the live-edge gate can exclude it.
    let (dang_lo, dang_hi) = span("dangling");
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &EdgeOracleRow {
        source_path: "m.rs",
        source_start_byte: 0,
        source_end_byte: 0,
        callee_start_byte: dang_lo as i64,
        callee_end_byte: dang_hi as i64,
        edge_kind: "calls_name",
        file_sha: &sha,
        resolved_symbol_id: None,
        scip_symbol: "rust cr 1.0 dangling().",
        kind: OracleResolutionKind::Upgrade,
    })
    .unwrap();

    let monikers = store::current_callee_monikers(&h.conn, "m.rs", &sha, COMMIT, WORKTREE).unwrap();
    assert_eq!(
        monikers,
        std::collections::HashMap::from([((live_lo, live_hi), "rust cr 1.0 live().".to_string())]),
        "a verdict with no live edge is dropped; the live-edge-backed one is returned"
    );
}

/// `edge_join_candidates_for_paths` (the live oracle's worklist read, #534) scopes candidates to
/// the named paths AND survives a worklist larger than one `IN`-list chunk — an accumulated
/// watcher backlog must never fail the prepare with a bound-variable overflow and wedge the
/// backlog.
#[test]
fn edge_join_candidates_for_paths_scopes_and_chunks() {
    let h = Harness::new();
    let a = h.add_file("a.rs", "fn a() { t(); }\n");
    let edge_a = h.add_edge(a, "t", 9, 10, "NameOnly", None);
    let b = h.add_file("b.rs", "fn b() { t(); }\n");
    h.add_edge(b, "t", 9, 10, "NameOnly", None);
    // c.rs exists but is NOT in the worklist.
    let c = h.add_file("c.rs", "fn c() { t(); }\n");
    h.add_edge(c, "t", 9, 10, "NameOnly", None);

    // Scoped: only the named paths come back.
    let candidates = store::edge_join_candidates_for_paths(&h.conn, COMMIT, WORKTREE, &[
        "a.rs".to_string(),
        "b.rs".to_string(),
    ])
    .unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].edge_id, edge_a);
    assert!(candidates.iter().all(|c| c.source_path != "c.rs"));

    // Chunked: > 500 paths (499 fillers + the real one) crosses one chunk boundary and still
    // resolves — the query is issued in bounded `IN` lists.
    let mut big: Vec<String> = (0..600).map(|i| format!("src/filler-{i}.rs")).collect();
    big.push("a.rs".to_string());
    let candidates =
        store::edge_join_candidates_for_paths(&h.conn, COMMIT, WORKTREE, &big).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].edge_id, edge_a);

    // Empty worklist → no query, no candidates.
    assert!(
        store::edge_join_candidates_for_paths(&h.conn, COMMIT, WORKTREE, &[]).unwrap().is_empty()
    );
}

/// `live_covered_edges_for_path` (the budget-continuation coverage read, #534) must NOT count a
/// verdict whose resolved DEFINITION no longer exists in the active checkout — the surfacing
/// read rejects such a row, so continuation would otherwise skip re-resolving the edge forever
/// behind evidence the read path never shows.
#[test]
fn live_covered_edges_excludes_a_stale_definition_verdict() {
    let h = Harness::new();
    let src = h.add_file("src.rs", "fn caller() { target(); }\n");
    let sha = h.file_sha("src.rs");
    // A live verdict resolving to a symbol that does NOT exist (id 9999) — a def that was
    // deleted/reindexed away.
    let stale = h.add_edge(src, "target", 14, 20, "NameOnly", None);
    let key = h.edge_content_key(stale);
    store::write_edge_oracle(&h.conn, OracleTool::RaLsp, "v", &EdgeOracleRow {
        source_path: &key.source_path,
        source_start_byte: key.source_start_byte,
        source_end_byte: key.source_end_byte,
        callee_start_byte: key.callee_start_byte,
        callee_end_byte: key.callee_end_byte,
        edge_kind: &key.edge_kind,
        file_sha: &sha,
        resolved_symbol_id: Some(9999),
        scip_symbol: "local ra-lsp-stale",
        kind: OracleResolutionKind::Upgrade,
    })
    .unwrap();

    let covered = store::live_covered_edges_for_path(
        &h.conn,
        OracleTool::RaLsp,
        "v",
        "src.rs",
        &sha,
        COMMIT,
        WORKTREE,
    )
    .unwrap();
    assert!(covered.is_empty(), "a verdict with a vanished definition is not coverage");

    // A verdict with NULL resolved_symbol_id (external-ish) or a live definition IS coverage.
    let real_target = h.add_symbol(src, "target", 0, 25);
    let live = h.add_edge_with_kind(src, "target", 14, 20, "references_type", "NameOnly", None);
    let live_key = h.edge_content_key(live);
    store::write_edge_oracle(&h.conn, OracleTool::RaLsp, "v", &EdgeOracleRow {
        source_path: &live_key.source_path,
        source_start_byte: live_key.source_start_byte,
        source_end_byte: live_key.source_end_byte,
        callee_start_byte: live_key.callee_start_byte,
        callee_end_byte: live_key.callee_end_byte,
        edge_kind: &live_key.edge_kind,
        file_sha: &sha,
        resolved_symbol_id: Some(real_target),
        scip_symbol: "local ra-lsp-live",
        kind: OracleResolutionKind::Upgrade,
    })
    .unwrap();
    let covered = store::live_covered_edges_for_path(
        &h.conn,
        OracleTool::RaLsp,
        "v",
        "src.rs",
        &sha,
        COMMIT,
        WORKTREE,
    )
    .unwrap();
    assert_eq!(covered.len(), 1, "the live-definition verdict is coverage; the stale one is not");
}
