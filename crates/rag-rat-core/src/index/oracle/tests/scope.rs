use super::*;

// ---------------------------------------------------------------------------
// Multi-checkout scoping — the four PR-#81 review findings (#68). Every
// metric/clear/recall read must scope to the run's (commit_sha, worktree_id),
// mirroring `edge_join_candidates`, so a sibling checkout in the same DB can't
// leak into (or be erased by) the active run.
// ---------------------------------------------------------------------------

// A sibling checkout sharing the same DB: a DIFFERENT commit, clean (empty worktree). Modelling the
// sibling as a distinct commit (rather than the same commit + a second worktree id) matches the
// real shape — two checkouts at the same HEAD is unusual, and under the active-checkout predicate a
// same-commit worktree overlay would (correctly) *shadow* the clean row by path, which is not the
// cross-checkout-isolation property these tests mean to assert. Commit isolation is.
/// Finding 1 + #248: `clear_edge_oracle_for_tool` must delete ONLY the active checkout's verdicts.
/// With two worktrees' verdicts for the same `(tool, tool_version)` in one DB, clearing one leaves
/// the other's intact — the clear is scoped via a CONTENT join to the live edges in the active
/// checkout (path + source/callee spans + edge_kind), not the old `edge_id` rowid subquery. The two
/// checkouts use DISTINCT callee ranges so their content keys differ (a content-key COLLISION
/// across checkouts is the intentional "same resolution → shared verdict" case, which would defeat
/// the per-checkout isolation this test asserts).
#[test]
fn clear_edge_oracle_for_tool_scopes_by_checkout_content() {
    let h = Harness::new();
    // Active-checkout edge + a verdict (real file sha so the scoped count tallies it).
    let active_file = h.add_file("a.rs", "fn caller() { target(); other(); }\n");
    let active_sha = h.file_sha("a.rs");
    let active_edge = h.add_edge(active_file, "target", 14, 20, "NameOnly", None);
    // Another worktree's edge (same DB, same path, DIFFERENT callee range → distinct content key) +
    // a verdict for the SAME tool/version.
    let other_file = h.add_file_in_scope("a.rs", OTHER_COMMIT, OTHER_WORKTREE);
    let other_sha = h.file_sha_for_commit("a.rs", OTHER_COMMIT);
    let other_edge = h.add_edge(other_file, "other", 22, 27, "NameOnly", None);

    h.write_verdict(active_edge, &active_sha, None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(other_edge, &other_sha, None, "s", OracleResolutionKind::Upgrade);
    // Whole-table count (both worktrees) — the production count helper is intentionally scoped to
    // ONE checkout, so this test reads the raw total directly to prove the cross-checkout state.
    let total_rows = || -> i64 {
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |row| row.get(0)).unwrap()
    };
    assert_eq!(total_rows(), 2);
    // The scoped count sees ONLY the active checkout's verdict.
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1
    );

    // Clear the ACTIVE checkout's scope only.
    store::clear_edge_oracle_for_tool(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();

    assert!(h.verdict(active_edge).is_none(), "active checkout's verdict cleared");
    assert!(h.verdict(other_edge).is_some(), "the other worktree's verdict is untouched");
    assert_eq!(total_rows(), 1, "only the other worktree's verdict remains in the table");
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        0,
        "active checkout's scoped count is now zero"
    );
}

/// A5 finding: `edge_oracle` reads (via the shared `edge_oracle_scope_join`) and the authoritative
/// clear (`clear_edge_oracle_for_tool`) are scoped by `edge_oracle.repo_id`. A SIBLING repo's
/// verdict whose CONTENT key (path + spans + sha) collides with one of THIS repo's live edges must
/// neither inflate the scoped count nor be cross-cleared by this repo's run — the content join
/// alone (files/edges are shared) would otherwise surface and delete it.
#[test]
fn edge_oracle_reads_and_clears_are_scoped_to_the_active_repo() {
    let h = Harness::new();
    let active_file = h.add_file("a.rs", "fn caller() { target(); }\n");
    let active_sha = h.file_sha("a.rs");
    let active_edge = h.add_edge(active_file, "target", 14, 20, "NameOnly", None);
    h.write_verdict(active_edge, &active_sha, None, "s", OracleResolutionKind::Upgrade);
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1
    );

    // Copy the active verdict under a SIBLING repo_id: SAME content key, so it joins the SAME live
    // edge — an UNSCOPED read would double-count it and an unscoped clear would delete it.
    let copied = h
        .conn
        .execute(
            "INSERT INTO edge_oracle(repo_id, source_path, source_start_byte, source_end_byte, \
             callee_start_byte, callee_end_byte, edge_kind, file_sha, tool, tool_version, \
             resolved_symbol_id, scip_symbol, kind, computed_at)
             SELECT 'oracle-sibling', source_path, source_start_byte, source_end_byte, \
             callee_start_byte, callee_end_byte, edge_kind, file_sha, tool, tool_version, \
             resolved_symbol_id, scip_symbol, kind, computed_at
             FROM edge_oracle WHERE repo_id != 'oracle-sibling'",
            [],
        )
        .unwrap();
    assert_eq!(copied, 1, "one content-colliding sibling verdict seeded");

    // The scoped count still sees ONLY the active repo's verdict.
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "a sibling repo's content-colliding verdict must not inflate the active repo's count",
    );

    // The scoped clear removes ONLY the active repo's verdict; the sibling survives.
    store::clear_edge_oracle_for_tool(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    let remaining: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(remaining, 1, "the sibling repo's verdict survives the active repo's clear");
    let sibling: i64 = h
        .conn
        .query_row("SELECT COUNT(*) FROM edge_oracle WHERE repo_id = 'oracle-sibling'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(sibling, 1, "the surviving row is the sibling's");
}

/// A5 finding: `oracle status` pairs its run HEADER (`last_run_meta`) and its verdict COUNTS to the
/// SAME repo. The counts route through the repo-scoped `edge_oracle_scope_join`; without the same
/// `oracle_runs.repo_id` predicate on the header read, a SIBLING repo's NEWER run at the identical
/// `(tool, tool_version, commit_sha, worktree_id)` — the fork case — would headline this repo's
/// status while the counts describe this repo's pass.
#[test]
fn oracle_status_header_and_counts_describe_the_active_repo_not_a_sibling() {
    let h = Harness::new();
    // One active-repo verdict + its run row (stamped with the active repo by the writer).
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let sha = h.file_sha("a.rs");
    let edge = h.add_edge(f, "target", 14, 20, "NameOnly", None);
    h.write_verdict(edge, &sha, None, "s", OracleResolutionKind::Upgrade);
    store::record_oracle_run(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, "complete", "{}").unwrap();

    // A sibling repo's NEWER run (higher id — wins an unscoped ORDER BY id DESC) at the SAME
    // (tool, version, commit, worktree), plus a content-colliding sibling verdict.
    h.conn
        .execute(
            "INSERT INTO oracle_runs(repo_id, tool, tool_version, commit_sha, worktree_id, \
             started_at, status, stats_json)
             VALUES ('oracle-sibling', ?1, ?2, ?3, ?4, 999, 'failed', '{}')",
            params![TOOL.as_db_str(), VERSION, COMMIT, WORKTREE],
        )
        .unwrap();
    h.conn
        .execute(
            "INSERT INTO edge_oracle(repo_id, source_path, source_start_byte, source_end_byte, \
             callee_start_byte, callee_end_byte, edge_kind, file_sha, tool, tool_version, \
             resolved_symbol_id, scip_symbol, kind, computed_at)
             SELECT 'oracle-sibling', source_path, source_start_byte, source_end_byte, \
             callee_start_byte, callee_end_byte, edge_kind, file_sha, tool, tool_version, \
             resolved_symbol_id, scip_symbol, kind, computed_at
             FROM edge_oracle WHERE repo_id != 'oracle-sibling'",
            [],
        )
        .unwrap();

    let status = super::status::status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(
        status.last_run_status.as_deref(),
        Some("complete"),
        "the status header must be the ACTIVE repo's run, not the sibling's newer 'failed' run",
    );
    assert_eq!(
        status.total_verdicts, 1,
        "the counts stay scoped to the active repo — header and counts describe the SAME repo",
    );
}

/// #248 THE killer test: an `edge_oracle` verdict SURVIVES reindex for an UNCHANGED file. A reindex
/// rewrites `edges_data` (DELETE + reinsert with NEW rowids); before the content-key fix the
/// `ON DELETE CASCADE` FK wiped every verdict and the opt-in oracle never repopulated. Now the
/// verdict is content-keyed with no FK, so the reindexed edge (same path + spans + edge_kind, same
/// `files.sha256`) RE-ANCHORS the verdict by content — `count_edge_oracle_scoped` /
/// `current_oracle_comparisons` still return it, re-projected onto the NEW edge id.
#[test]
fn edge_oracle_survives_reindex_for_unchanged_file() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let sha = h.file_sha("a.rs");
    let target = h.add_symbol(f, "target", 3, 9);
    // An Exact edge the heuristic resolved to `target`; the oracle CONTRADICTS it (so it shows up
    // in `current_oracle_comparisons`, which keeps Contradict rows).
    let edge_v1 = h.add_edge(f, "target", 14, 20, "Exact", Some(target));
    h.write_verdict(
        edge_v1,
        &sha,
        None,
        "scip-rust cargo other 1.0 `target`().",
        OracleResolutionKind::Contradict,
    );

    // Sanity before reindex: counted + surfaced in compare.
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "verdict counted before reindex"
    );
    assert_eq!(
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap().len(),
        1
    );
    let physical_before: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();

    // --- Simulate a reindex of the UNCHANGED file: DELETE the edge (rewriting edges_data) and
    // re-insert the SAME edge content, which mints a NEW edges_data rowid. files.sha256 unchanged.
    // ---
    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![edge_v1]).unwrap();
    let edge_v2 = h.add_edge(f, "target", 14, 20, "Exact", Some(target));
    assert_ne!(edge_v2, edge_v1, "reindex minted a new edge rowid");

    // The verdict was NOT touched (no cascade) — same physical row count.
    let physical_after: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(physical_after, physical_before, "no FK cascade wiped the verdict on reindex");

    // And it RE-ANCHORS to the new edge by content key: still counted, still in compare, now keyed
    // on the NEW edge id.
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "verdict still counted after reindex (re-anchored by content)"
    );
    let comparisons =
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(comparisons.len(), 1, "verdict re-surfaces in compare after reindex");
    assert_eq!(
        comparisons[0].edge_id, edge_v2,
        "the comparison is re-projected onto the LIVE edge id"
    );
    assert_eq!(
        h.verdict(edge_v2).map(|(kind, _, _)| kind),
        Some(OracleResolutionKind::Contradict.as_db_str().to_string()),
        "the reindexed edge resolves the re-anchored verdict by content"
    );
}

/// #248: a verdict for a CHANGED file (its `files.sha256` no longer matches the verdict's
/// `file_sha`) is NOT counted — the scope join gates `files.sha256 = edge_oracle.file_sha`, so a
/// stale verdict drops out of the metrics until the next run rewrites it.
#[test]
fn edge_oracle_stale_after_file_change_not_counted() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let sha = h.file_sha("a.rs");
    let edge = h.add_edge(f, "target", 14, 20, "Exact", None);
    h.write_verdict(edge, &sha, None, "scip x `target`().", OracleResolutionKind::Confirm);
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "current verdict counted"
    );

    // The file content changed: its recorded sha no longer matches the verdict's file_sha.
    h.conn.execute("UPDATE files SET sha256 = 'changed-sha' WHERE id = ?1", params![f]).unwrap();

    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        0,
        "a changed file's verdict is stale → not counted (file_sha mismatch)"
    );
    assert!(
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE)
            .unwrap()
            .is_empty(),
        "a stale verdict does not surface in compare either"
    );
}

/// #248 END-TO-END (the regression that started the issue): a full `run_oracle` writes verdicts,
/// then a reindex of the UNCHANGED file rewrites `edges_data` with new ids — and the compare
/// surface (`current_oracle_comparisons`, the data behind `compare_graph_to_scip`) is STILL
/// non-empty, re-anchored to the reindexed edge by content key. Before #248 the `ON DELETE CASCADE`
/// FK wiped the verdict on reindex and the compare surface went empty (the opt-in oracle never
/// repopulated). Exercises the REAL join+write path (`run_oracle` over a built `.scip` with a call
/// + def occurrence), not a hand-written verdict.
#[test]
fn oracle_run_then_reindex_then_compare_graph_to_scip_nonempty() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\nfn other() {}\n");
    // Heuristic resolved `target` to the WRONG symbol; the oracle CONTRADICTS it (Contradict rows
    // are exactly what `current_oracle_comparisons` keeps).
    let wrong_sym = h.add_symbol(defs, "other", 18, 23);
    let right_sym = h.add_symbol(defs, "target", 3, 9);
    let edge_v1 = h.add_edge(caller, "target", 14, 20, "Exact", Some(wrong_sym));

    let symbol = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                14,
                20,
                symbol,
                SymbolRole::UnspecifiedSymbolRole as i32,
            )],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![occurrence(0, 3, 9, symbol, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.rows_written, 1, "the run wrote one verdict");

    // Compare surface is non-empty before reindex (sanity).
    let before =
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(before.len(), 1, "the contradiction surfaces in compare before reindex");
    assert_eq!(before[0].edge_id, edge_v1);
    assert_eq!(before[0].kind, OracleResolutionKind::Contradict);

    // --- Simulate a reindex of the UNCHANGED caller.rs: rewrite the edge (new edges_data rowid),
    // SAME file content/sha. (The defs.rs symbols are untouched so the def-drift gate stays
    // satisfied.) ---
    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![edge_v1]).unwrap();
    let edge_v2 = h.add_edge(caller, "target", 14, 20, "Exact", Some(wrong_sym));
    assert_ne!(edge_v2, edge_v1, "reindex minted a new edge rowid");

    // THE REGRESSION ASSERTION: compare surface is STILL non-empty, re-anchored to the new edge id.
    let after =
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(after.len(), 1, "compare surface survives reindex (re-anchored by content key)");
    assert_eq!(
        after[0].edge_id, edge_v2,
        "the comparison re-projects onto the LIVE reindexed edge"
    );
    assert_eq!(after[0].kind, OracleResolutionKind::Contradict);
    assert_eq!(
        after[0].resolved_symbol_id,
        Some(right_sym),
        "the re-anchored verdict still names the oracle's correct target",
    );
}

/// #248 collision: a `calls_name` edge and a `references_type` edge that share the SAME callee token
/// (same callee byte range, same call site) get DISTINCT verdicts — the content key includes
/// `edge_kind`, so the two never collide onto one `edge_oracle` row, and a verdict for one never
/// leaks onto the other. (This is the design's disambiguation claim — R1: the call-site span + the
/// callee span + the edge kind make the key unique per edge.)
#[test]
fn edge_oracle_collision_same_callee_range_different_kind_disambiguated() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { Thing(); }\n");
    let sha = h.file_sha("a.rs");
    // Two edges on the SAME identifier token `Thing` (callee bytes 14..19, same call site span):
    // one a `calls_name` (constructor call), one a `references_type`. Same callee range, different
    // kind — the pre-#248 callee-range-only key would have collided them.
    let call_edge = h.add_edge_with_kind(f, "Thing", 14, 19, "calls_name", "Exact", None);
    let ref_edge = h.add_edge_with_kind(f, "Thing", 14, 19, "references_type", "Exact", None);
    assert_ne!(call_edge, ref_edge, "two distinct edges share the callee token");

    // Distinct verdicts: the call resolves to a function symbol, the type-ref to a type symbol.
    h.write_verdict(call_edge, &sha, None, "scip x `Thing`#new().", OracleResolutionKind::Upgrade);
    h.write_verdict(ref_edge, &sha, None, "scip x `Thing`#", OracleResolutionKind::Confirm);

    // Two physical rows (the content key disambiguated by edge_kind), not one overwritten row.
    let rows: i64 = h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 2, "edge_kind in the content key keeps the two verdicts distinct");

    // Each edge resolves to ITS OWN verdict — no cross-contamination.
    let (call_kind, _, call_scip) = h.verdict(call_edge).expect("call verdict");
    assert_eq!(call_kind, OracleResolutionKind::Upgrade.as_db_str());
    assert_eq!(call_scip, "scip x `Thing`#new().");
    let (ref_kind, _, ref_scip) = h.verdict(ref_edge).expect("ref verdict");
    assert_eq!(ref_kind, OracleResolutionKind::Confirm.as_db_str());
    assert_eq!(ref_scip, "scip x `Thing`#");
}

/// #248 content-key collision SEMANTICS (the schema-permitted, organically-unreachable edge case):
/// two verdicts with the SAME FULL content key
/// `(tool, tool_version, source_path, source_start_byte, source_end_byte, callee_start_byte,
/// callee_end_byte, edge_kind)` — differing only in the resolved target (`resolved_symbol_id` /
/// `scip_symbol`) and verdict `kind` — collapse to EXACTLY ONE physical `edge_oracle` row via the
/// PRIMARY-KEY upsert (last write wins). The unique-key disambiguation test above keeps two rows by
/// changing `edge_kind`; THIS test pins what happens when even `edge_kind` matches.
///
/// This is DOCUMENTED as schema-PERMITTED but organically UNREACHABLE for real extracted edges: the
/// call-site source span (`source_start_byte`/`source_end_byte`) is part of the key, and two
/// distinct call sites always carry distinct source spans (measured 0 collisions / 1.03M rows). The
/// key is NOT schema-enforced to be 1:1 with a live edge — it is unique only because real call-site
/// spans are. Pinning the upsert here means a FUTURE extract/resolve span change that made the same
/// full key reachable for two genuinely-different edges would surface as a behavior change this
/// test catches (the count helpers assume this measured 1:1, per `count_edge_oracle_scoped`).
#[test]
fn edge_oracle_same_full_content_key_upserts_one_row() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { thing(); }\n");
    let sha = h.file_sha("a.rs");
    // ONE edge → ONE content key. Writing two verdicts for it reuses the identical full key.
    let edge = h.add_edge(f, "thing", 14, 19, "Exact", None);

    // First verdict: an Upgrade naming target A.
    h.write_verdict(edge, &sha, Some(101), "scip x `thing`#A().", OracleResolutionKind::Upgrade);
    // Second verdict, SAME full content key (same edge → same path + spans + edge_kind, same
    // TOOL/VERSION), differing only in the resolved target and the verdict kind.
    h.write_verdict(edge, &sha, Some(202), "scip x `thing`#B().", OracleResolutionKind::Confirm);

    // EXACTLY ONE physical row — the second write UPSERTED the first (it did not insert a sibling).
    let rows: i64 = h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1, "same full content key collapses to one edge_oracle row (PK upsert)");

    // Last write wins: the surviving row carries the SECOND verdict's target + kind.
    let (kind, resolved, scip) = h.verdict(edge).expect("the single upserted verdict");
    assert_eq!(
        kind,
        OracleResolutionKind::Confirm.as_db_str(),
        "the later verdict's kind survives"
    );
    assert_eq!(resolved, Some(202), "the later verdict's resolved target survives");
    assert_eq!(scip, "scip x `thing`#B().", "the later verdict's scip symbol survives");
}

/// #248 counts: after a reindex that CHANGES one file (its `files.sha256` drifts), status/eval
/// counts reflect only LIVE + CURRENT verdicts — the changed file's verdict drops out (file_sha
/// mismatch in the scope join) and never inflates the totals, while the unchanged file's verdict
/// stays counted. Without the live-edge content join + the sha gate, dangling/stale verdicts would
/// inflate the count.
#[test]
fn status_eval_counts_unaffected_by_dangling() {
    let h = Harness::new();
    // Two files each with a confirmed verdict — both counted initially.
    let stable = h.add_file("stable.rs", "fn caller() { keep(); }\n");
    let stable_sha = h.file_sha("stable.rs");
    let stable_edge = h.add_edge(stable, "keep", 14, 18, "Exact", None);
    h.write_verdict(
        stable_edge,
        &stable_sha,
        None,
        "scip x `keep`().",
        OracleResolutionKind::Confirm,
    );

    let churn = h.add_file("churn.rs", "fn caller() { drift(); }\n");
    let churn_sha = h.file_sha("churn.rs");
    let churn_edge = h.add_edge(churn, "drift", 14, 19, "Exact", None);
    h.write_verdict(
        churn_edge,
        &churn_sha,
        None,
        "scip x `drift`().",
        OracleResolutionKind::Confirm,
    );

    let status0 = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(status0.total_verdicts, 2, "both verdicts counted before any change");
    assert_eq!(status0.confirmed, 2);

    // Reindex churn.rs with CHANGED content: rewrite its edge (new rowid) AND drift its
    // files.sha256 — the verdict's file_sha no longer matches, so it is stale. The stable file
    // is untouched.
    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![churn_edge]).unwrap();
    let _churn_edge_v2 = h.add_edge(churn, "drift", 14, 19, "Exact", None);
    h.conn
        .execute("UPDATE files SET sha256 = 'churn-changed' WHERE id = ?1", params![churn])
        .unwrap();

    let status1 = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(
        status1.total_verdicts, 1,
        "only the live + current (unchanged-file) verdict counts; the changed file's is stale",
    );
    assert_eq!(status1.confirmed, 1);

    // Eval metrics use the SAME scoped counts — the stale verdict does not inflate them either.
    let m = super::oracle_eval_metrics(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, RecallCalls {
        covered: 1,
        oracle_only: 0,
    })
    .unwrap();
    assert_eq!(m.confirmed, 1, "eval confirmed count is live + current only");
}

/// Finding 2: a low-confidence edge in ANOTHER worktree must not inflate the current run's
/// `oracle_upgradeable_fraction` denominator. With one upgraded low-conf edge in-scope and an
/// extra unresolved low-conf edge out-of-scope, the scoped fraction is 1/1 = 1.0 (not 1/2).
#[test]
fn upgradeable_fraction_denominator_is_scoped_to_active_checkout() {
    let h = Harness::new();
    // Active checkout: one NameOnly edge the oracle upgraded → numerator 1, denominator 1.
    let active = h.add_file("a.rs", "fn caller() {}\n");
    let active_sha = h.file_sha("a.rs");
    let e_low = h.add_edge(active, "u", 0, 1, "NameOnly", None);
    h.write_verdict(e_low, &active_sha, Some(1), "s", OracleResolutionKind::Upgrade);

    // Another worktree: an unresolved NameOnly candidate carrying a callee range, NO verdict. If
    // the denominator weren't scoped, it would count → fraction 1/2.
    let other = h.add_file_in_scope("a.rs", OTHER_COMMIT, OTHER_WORKTREE);
    let _ = h.add_edge(other, "v", 0, 1, "NameOnly", None);

    let m = super::oracle_eval_metrics(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        RecallCalls::default(),
    )
    .unwrap();
    assert!(
        (m.oracle_upgradeable_fraction - 1.0).abs() < 1e-9,
        "scoped fraction is 1/1, not diluted to 1/2 by the other worktree; got {}",
        m.oracle_upgradeable_fraction
    );
}

/// Finding 3: a `.scip` definition in a file rag-rat did NOT index (no matching DB symbol) is
/// excluded from the recall gap — counting calls to un-indexed tests/examples/generated/dependency
/// sources as misses is a false negative. The same callable, when its def maps to an indexed
/// symbol, IS counted; without an indexed symbol it drops to zero.
#[test]
fn recall_gap_excludes_definitions_in_unindexed_files() {
    // The `.scip` references `target`, defined in `gen.rs` — a file with occurrences but whose
    // definition byte range maps to NO indexed symbol (rag-rat never indexed gen.rs's symbols).
    let build = |seed_symbol: bool| -> u64 {
        let h = Harness::new();
        let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
        let _ = h.add_edge(caller, "noop", 0, 1, "NameOnly", None); // unrelated, distinct occurrence
        let gen_file = h.add_file("gen.rs", "fn target() {}\n");
        if seed_symbol {
            // Seed the def symbol so the callable resolves to OUR corpus → counted.
            h.add_symbol(gen_file, "target", 3, 9);
        }

        let callable = "scip-rust crate v1 `target`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                // Uncovered callable reference (no edge covers it) at bytes 14..20.
                occurrences: vec![occurrence(
                    0,
                    14,
                    20,
                    callable,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "gen.rs".to_string(),
            occurrences: vec![occurrence(0, 3, 9, callable, SymbolRole::Definition as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        });
        let bytes = index.write_to_bytes().unwrap();
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None)
            .unwrap()
            .oracle_only_calls
    };

    assert_eq!(build(false), 0, "a def in an un-indexed file is NOT a recall gap");
    assert_eq!(build(true), 1, "the same callable IS a recall gap once its def maps to a symbol");
}

/// Round-3 finding (occurrence side of the recall gap): a `.scip` occurrence whose *call site*
/// lives in a SOURCE document rag-rat did NOT index in this checkout is excluded from the recall
/// gap, even when the callee *definition* resolves to an indexed symbol. No edge candidate can ever
/// cover a call from an unindexed file (`edge_join_candidates` only emits candidates for indexed
/// files), so counting it as an uncovered call is a false miss. The same callable, when its call
/// site IS an indexed file, IS counted — proving the filter is the occurrence path, not the
/// definition.
#[test]
fn recall_gap_excludes_occurrences_in_unindexed_source_files() {
    // `target` is defined in an INDEXED file with a seeded symbol (so the def-side filter passes).
    // The uncovered call occurrence lives in `caller.rs`; we vary whether rag-rat indexed it.
    let build = |index_caller: bool| -> u64 {
        let h = Harness::new();
        // The def file is always indexed + its symbol seeded → the def-side filter never trips.
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        h.add_symbol(defs, "target", 3, 9);
        if index_caller {
            // caller.rs IS an indexed file in this checkout → the call site is in scope.
            h.add_file("caller.rs", "fn caller() { target(); }\n");
        }
        // else: caller.rs exists in the `.scip` but is NOT a `files` row → out-of-scope call site.

        let callable = "scip-rust crate v1 `target`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                // Uncovered callable reference (no edge covers it) at bytes 14..20.
                occurrences: vec![occurrence(
                    0,
                    14,
                    20,
                    callable,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "defs.rs".to_string(),
            occurrences: vec![occurrence(0, 3, 9, callable, SymbolRole::Definition as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        });
        let bytes = index.write_to_bytes().unwrap();
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None)
            .unwrap()
            .oracle_only_calls
    };

    assert_eq!(build(false), 0, "a call from an un-indexed source file is NOT a recall gap");
    assert_eq!(build(true), 1, "the same call IS a recall gap once its source file is indexed");
}

/// Round-3 finding (the comprehensive audit): a sibling checkout's `edge_oracle` rows for the SAME
/// `(tool, tool_version)` must not perturb the active checkout's `verdict_counts`-derived metrics
/// (precision/recall) OR its status. Checkout A sees 1 confirm + 1 contradict (precision 0.5);
/// checkout B has its own confirm/confirm/upgrade rows. Before the round-3 fix `verdict_counts` was
/// a global `(tool, tool_version)` count, so B's confirms would inflate A's numerator and break
/// both precision and recall. This pins the whole metric path to the active checkout.
#[test]
fn verdict_counts_and_metrics_ignore_sibling_checkout_rows() {
    let h = Harness::new();

    // Active checkout A: one confirmed + one contradicted Exact edge → precision 1/2.
    let a_file = h.add_file("a.rs", "fn caller() {}\n");
    let a_sha = h.file_sha("a.rs");
    let a_conf = h.add_edge(a_file, "c", 0, 1, "Exact", None);
    let a_contra = h.add_edge(a_file, "d", 1, 2, "Exact", None);
    h.write_verdict(a_conf, &a_sha, None, "s", OracleResolutionKind::Confirm);
    h.write_verdict(a_contra, &a_sha, None, "s", OracleResolutionKind::Contradict);

    // Sibling checkout B (same DB, same tool/version): TWO confirms + an upgrade. Uses a DISTINCT
    // path ("b.rs") so the content keys never collide with A's (#248: the content key omits
    // commit/worktree, so a same-path same-span edge in B would SHARE A's verdict row — that is the
    // intentional "same resolution" case; this test asserts SCOPE isolation, so it keeps the
    // populations physically distinct). If A's counts leaked B's rows, A's precision would jump.
    let b_file = h.add_file_in_scope("b.rs", OTHER_COMMIT, OTHER_WORKTREE);
    let b_sha = h.file_sha_for_commit("b.rs", OTHER_COMMIT);
    let b_conf1 = h.add_edge(b_file, "e", 0, 1, "Exact", None);
    let b_conf2 = h.add_edge(b_file, "f", 1, 2, "Exact", None);
    let b_up = h.add_edge(b_file, "g", 2, 3, "NameOnly", None);
    h.write_verdict(b_conf1, &b_sha, None, "s", OracleResolutionKind::Confirm);
    h.write_verdict(b_conf2, &b_sha, None, "s", OracleResolutionKind::Confirm);
    h.write_verdict(b_up, &b_sha, None, "s", OracleResolutionKind::Upgrade);

    // A's metrics: precision = 1 confirm / (1 confirm + 1 contradict) = 0.5; recall over A's
    // covered call set (2 covered call occurrences) and 0 oracle-only = 2/2 = 1.0. The recall
    // counts come from the run; B's three verdict rows never enter A's precision either.
    let m = super::oracle_eval_metrics(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, RecallCalls {
        covered: 2,
        oracle_only: 0,
    })
    .unwrap();
    assert_eq!(m.confirmed, 1, "only A's confirm counts");
    assert_eq!(m.contradicted, 1);
    assert_eq!(m.upgraded, 0, "B's upgrade does not leak into A");
    assert!(
        (m.precision - 0.5).abs() < 1e-9,
        "precision unperturbed by B's confirms; got {}",
        m.precision
    );
    assert!((m.recall - 1.0).abs() < 1e-9, "recall is A's covered set only; got {}", m.recall);

    // The status read shares the same scoped `verdict_counts`, so it is scoped identically.
    let status = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(status.total_verdicts, 2, "status counts only A's two verdicts");
    assert_eq!(status.confirmed, 1);
    assert_eq!(status.contradicted, 1);
    assert_eq!(status.upgraded, 0);

    // Sanity: checkout B's own scoped status sees its three rows, proving the rows really exist.
    let status_b =
        super::oracle_status(&h.conn, TOOL, VERSION, OTHER_COMMIT, OTHER_WORKTREE).unwrap();
    assert_eq!(status_b.total_verdicts, 3);
    assert_eq!(status_b.confirmed, 2);
    assert_eq!(status_b.upgraded, 1);
}

/// Finding 4: an already-resolved (`Exact`) edge pointing at an IN-CORPUS target, when SCIP
/// resolves the same call to an EXTERNAL definition, is a CONTRADICTION (the heuristic picked the
/// wrong target) — not `resolved-external`. It must count in `confirm + contradict` and lower
/// precision.
#[test]
fn exact_in_corpus_edge_contradicted_by_external_scip_resolution() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { spawn(); }\n");
    let defs = h.add_file("defs.rs", "fn spawn() {}\n");
    // The heuristic resolved `spawn` to an IN-CORPUS symbol (Exact).
    let in_corpus = h.add_symbol(defs, "spawn", 3, 8);
    let edge = h.add_edge(caller, "spawn", 14, 19, "Exact", Some(in_corpus));

    // SCIP says `spawn` is the external tokio::spawn — a package-bearing symbol with NO in-corpus
    // definition.
    let external = "scip-rust cargo tokio 1.0 `spawn`().";
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 14, 19, external, SymbolRole::UnspecifiedSymbolRole as i32),
    ]);

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let (kind, resolved, scip) = h.verdict(edge).expect("verdict written");
    assert_eq!(
        kind,
        OracleResolutionKind::Contradict.as_db_str(),
        "external-vs-in-corpus disagreement is a contradiction, not resolved-external"
    );
    assert_eq!(resolved, None, "no in-corpus symbol — the real target is external");
    assert_eq!(scip, external);
    assert_eq!(report.contradicted, 1);
    assert_eq!(report.resolved_external, 0, "it is NOT counted as resolved-external");
    // Heuristic row untouched.
    assert_eq!(h.heuristic_resolution(edge), ("exact".to_string(), Some(in_corpus)));

    // Precision counts it honestly: 0 confirmed / (0 + 1 contradicted) = 0.0.
    let m = super::oracle_eval_metrics(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        RecallCalls::default(),
    )
    .unwrap();
    assert!(
        (m.precision - 0.0).abs() < 1e-9,
        "contradiction lowers precision; got {}",
        m.precision
    );
    assert_eq!(m.contradicted, 1);
}

/// Finding 4 (counterpart): an UNRESOLVED / `NameOnly` edge SCIP places externally is still
/// `resolved-external` — there is no in-corpus claim to contradict. This pins the boundary so the
/// contradiction rule doesn't swallow the legitimate external-recovery case.
#[test]
fn name_only_edge_with_external_scip_stays_resolved_external() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { spawn(); }\n");
    // NameOnly + unresolved (no heuristic symbol) → no in-corpus claim.
    let edge = h.add_edge(caller, "spawn", 14, 19, "NameOnly", None);

    let external = "scip-rust cargo tokio 1.0 `spawn`().";
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 14, 19, external, SymbolRole::UnspecifiedSymbolRole as i32),
    ]);

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let (kind, _, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::ResolvedExternal.as_db_str());
    assert_eq!(report.resolved_external, 1);
    assert_eq!(report.contradicted, 0);
}

/// The `(None, Some(_))` join arm: SCIP HAS a definition document for the callee, but that
/// definition's byte range maps to NO indexed symbol (the def lives in an un-indexed file) → the
/// callee is external. A NameOnly edge there is `resolved-external`. Distinct from the package-only
/// `(None, None)` path covered above — this exercises the in-`.scip`-but-out-of-corpus branch.
#[test]
fn scip_definition_outside_indexed_corpus_resolves_external() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    // `defs.rs` is indexed as a file but we seed NO symbol for `target`, so the def maps to
    // nothing.
    let _defs = h.add_file("defs.rs", "fn target() {}\n");
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    // A package-bearing symbol WITH a definition occurrence in defs.rs → goes through (None, Some).
    let symbol = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                14,
                20,
                symbol,
                SymbolRole::UnspecifiedSymbolRole as i32,
            )],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![occurrence(0, 3, 9, symbol, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::ResolvedExternal.as_db_str());
    assert_eq!(resolved, None, "def maps to no indexed symbol → external");
    assert_eq!(report.resolved_external, 1);
}

/// A reference occurrence whose symbol has NO SCIP definition and names NO package is the
/// no-actionable-data `(None, None)` drop: the join returns `None`, no verdict is written. (A
/// non-`local`, non-package, definition-less symbol — e.g. a malformed/synthetic one.)
#[test]
fn reference_without_definition_or_package_yields_no_verdict() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { mystery(); }\n");
    let edge = h.add_edge(caller, "mystery", 14, 21, "NameOnly", None);

    // A bare symbol with no `Definition` occurrence anywhere and no package component.
    let bare = "scip-rust  `mystery`().";
    assert!(!join::names_external_package(bare), "fixture must have no package");
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 14, 21, bare, SymbolRole::UnspecifiedSymbolRole as i32),
    ]);

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert!(h.verdict(edge).is_none(), "no definition + no package → no verdict");
    assert_eq!(report.rows_written, 0);
    assert_eq!(report.no_occurrence, 1, "dropped into the no-actionable bucket");
}

// ---------------------------------------------------------------------------
// #81 review fixes — recall population, content integrity, worktree scope, tombstones.
// ---------------------------------------------------------------------------

/// Finding 1 (gap side): an in-corpus FIELD/CONST read — a SCIP `Term` descriptor ending in a bare
/// `.` (no `()`), with an in-corpus definition — is NOT a callable, so it must NOT count as a
/// missed call in the recall gap. Before the `symbol_is_callable` tightening (`).` not bare `.`) it
/// slipped through and deflated recall. The same occurrence as a genuine `Method` (`).`) IS
/// counted, proving the suffix is what gates it.
#[test]
fn recall_gap_excludes_field_const_term_reads() {
    let build = |callable: bool| -> u64 {
        let h = Harness::new();
        let src = h.add_file("src.rs", "fn caller() {}\n");
        // Seed the def symbol so the def-side filter passes — the ONLY thing left deciding the gap
        // is callability.
        h.add_symbol(src, "VALUE", 3, 8);
        // A `Term` (bare `.`) is a const/field read; a `Method` (`).`) is a call.
        let symbol =
            if callable { "scip-rust crate v1 `VALUE`()." } else { "scip-rust crate v1 `VALUE`." };
        let index = Index {
            documents: vec![Document {
                relative_path: "src.rs".to_string(),
                occurrences: vec![
                    // Uncovered reference (no edge covers it) at bytes 5..10.
                    occurrence(0, 5, 10, symbol, SymbolRole::UnspecifiedSymbolRole as i32),
                    // Its in-corpus definition (line 0, bytes 3..8).
                    occurrence(0, 3, 8, symbol, SymbolRole::Definition as i32),
                ],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = index.write_to_bytes().unwrap();
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None)
            .unwrap()
            .oracle_only_calls
    };

    assert_eq!(build(false), 0, "a bare-`.` Term (field/const read) is NOT a missed call");
    assert_eq!(build(true), 1, "the same occurrence as a `).` Method IS a missed call");
}

/// Finding 1 (covered side): the covered side of recall counts ONLY `calls_name`-edge occurrences.
/// A `references_type` edge that the oracle CONFIRMS carries a callee byte range (so it joins and
/// gets a verdict), but it is not a *call* — it must NOT inflate `covered_calls`. A `calls_name`
/// edge over a DIFFERENT occurrence does count. So with one confirmed type-ref edge and one covered
/// call, `covered_calls == 1`, not 2.
#[test]
fn covered_side_ignores_references_type_confirmation() {
    let h = Harness::new();
    // `caller.rs`: a call to `target` at 14..20 and a type reference `Thing` at 24..29.
    let caller = h.add_file("caller.rs", "fn caller() { target(); Thing::new(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\nstruct Thing;\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let thing_sym = h.add_symbol(defs, "Thing", 22, 27);
    // A CALL edge (covers the call occurrence) and a TYPE-REF edge (covers the type occurrence).
    let call_edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));
    let type_edge =
        h.add_edge_with_kind(caller, "Thing", 24, 29, "references_type", "Exact", Some(thing_sym));

    let call_sym = "scip-rust crate v1 `target`().";
    let type_sym = "scip-rust crate v1 `Thing`#";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![
                occurrence(0, 14, 20, call_sym, SymbolRole::UnspecifiedSymbolRole as i32),
                occurrence(0, 24, 29, type_sym, SymbolRole::UnspecifiedSymbolRole as i32),
            ],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![
            occurrence(0, 3, 9, call_sym, SymbolRole::Definition as i32),
            occurrence(1, 7, 12, type_sym, SymbolRole::Definition as i32),
        ],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    // BOTH edges got verdicts (both carry callee ranges and join)…
    assert!(h.verdict(call_edge).is_some(), "call edge verdicted");
    assert!(h.verdict(type_edge).is_some(), "type-ref edge verdicted");
    // …but only the CALL occurrence counts toward the covered side of recall.
    assert_eq!(
        report.covered_calls, 1,
        "the references_type confirmation does NOT inflate covered"
    );
    assert_eq!(report.oracle_only_calls, 0, "both call-like occurrences were covered");

    let m = super::oracle_eval_metrics(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, RecallCalls {
        covered: report.covered_calls,
        oracle_only: report.oracle_only_calls,
    })
    .unwrap();
    assert!(
        (m.recall - 1.0).abs() < 1e-9,
        "recall over the call population only; got {}",
        m.recall
    );
    assert_eq!(m.covered_calls, 1);
}

/// #176 (covered side): the covered side requires the matched SCIP symbol be CALLABLE (`).`) — the
/// same filter `count_uncovered_calls` applies. A `calls_name` edge a verdict matched to a CLASS
/// symbol (`…Thing#`, e.g. scip-python's `Thing()` constructor, which our extractor emits as
/// `CallsName` but SCIP records as a reference to the class) must NOT inflate `covered_calls`.
/// Otherwise the two sides measure different populations and a MISSED constructor — invisible to
/// the callable-filtered uncovered side — would never offset a covered one, inflating recall.
#[test]
fn covered_side_requires_a_callable_scip_symbol() {
    let h = Harness::new();
    // `caller.rs`: a method call `target` at 14..20 and a constructor call `Thing` at 24..29.
    let caller = h.add_file("caller.rs", "fn caller() { target(); Thing(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\nstruct Thing;\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let thing_sym = h.add_symbol(defs, "Thing", 22, 27);
    // BOTH are `calls_name` edges (a constructor call is a `CallsName` in our extractor).
    let call_edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));
    let ctor_edge = h.add_edge(caller, "Thing", 24, 29, "Exact", Some(thing_sym));

    let call_sym = "scip-rust crate v1 `target`().";
    // Class symbol — ends `#`, NOT `).`: not callable (how scip-python records a constructor ref).
    let class_sym = "scip-rust crate v1 `Thing`#";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![
                occurrence(0, 14, 20, call_sym, SymbolRole::UnspecifiedSymbolRole as i32),
                occurrence(0, 24, 29, class_sym, SymbolRole::UnspecifiedSymbolRole as i32),
            ],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![
            occurrence(0, 3, 9, call_sym, SymbolRole::Definition as i32),
            occurrence(1, 7, 12, class_sym, SymbolRole::Definition as i32),
        ],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    // Both edges still get verdicts (both join + resolve in-corpus)…
    assert!(h.verdict(call_edge).is_some(), "call edge verdicted");
    assert!(h.verdict(ctor_edge).is_some(), "constructor edge verdicted");
    // …but only the callable-symbol call counts as covered; the class-symbol constructor does not,
    // and the uncovered side excludes it too → no phantom recall gap.
    assert_eq!(report.covered_calls, 1, "constructor (class symbol) must NOT inflate covered");
    assert_eq!(report.oracle_only_calls, 0);
}

/// Finding 2: a candidate whose recorded `file_sha` no longer matches the disk bytes (content drift
/// between the index build and the `.scip`) is SKIPPED — no verdict is emitted from mismatched
/// content — and tallied in `skipped_drifted`. The same edge, with a matching `file_sha`, IS
/// verdicted, proving the gate is the sha comparison.
#[test]
fn drifted_file_sha_is_skipped_not_verdicted() {
    // (verdict row: kind, resolved_symbol_id, scip_symbol; skipped_drifted; examined)
    type DriftProbe = (Option<(String, Option<i64>, String)>, u64, u64);
    let build = |drift: bool| -> DriftProbe {
        let h = Harness::new();
        let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        let target_sym = h.add_symbol(defs, "target", 3, 9);
        let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);
        if drift {
            // The edge was indexed against a DIFFERENT sha than the file on disk now.
            h.set_file_sha(
                caller,
                "0000000000000000000000000000000000000000000000000000000000000000",
            );
        }

        let sym = "scip-rust crate v1 `target`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                occurrences: vec![occurrence(
                    0,
                    14,
                    20,
                    sym,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "defs.rs".to_string(),
            occurrences: vec![occurrence(0, 3, 9, sym, SymbolRole::Definition as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        });
        let _ = target_sym;
        let bytes = index.write_to_bytes().unwrap();
        let report =
            run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None)
                .unwrap();
        (h.verdict(edge), report.skipped_drifted, report.rows_written)
    };

    let (verdict, skipped, written) = build(true);
    assert!(verdict.is_none(), "a drifted candidate must NOT be verdicted");
    assert_eq!(skipped, 1, "the drifted candidate is tallied in skipped_drifted");
    assert_eq!(written, 0, "nothing written from drifted content");

    let (verdict, skipped, written) = build(false);
    assert!(verdict.is_some(), "the same candidate with a matching file_sha IS verdicted");
    assert_eq!(skipped, 0, "no drift when the sha matches");
    assert_eq!(written, 1);
}

/// #82 TOCTOU: the scip-vs-disk gate. A tool-driven run carries a per-document `production_sha`
/// snapshot — the disk hashes captured the instant the subprocess finished. The join verdicts a
/// candidate only when that snapshot STILL equals the disk bytes it reads; if the snapshot no
/// longer matches (the watcher reindexed the call-site file in the lock-free window after the
/// `.scip` was built, so index-vs-disk agrees on the NEW content while the `.scip` describes the
/// OLD), the candidate is skipped as drifted instead of writing a spurious Compiler verdict. A
/// document absent from the snapshot (unreadable at production) also fails the gate. The pre-built
/// `--scip` path (`None`) keeps only the index-vs-disk gate — proven by
/// `drifted_file_sha_is_skipped_not_verdicted`.
#[test]
fn stale_production_snapshot_is_skipped_not_verdicted() {
    // What the production snapshot says about the documents a verdict depends on: the call-site
    // file `caller.rs` and the definition file `defs.rs`.
    enum Snapshot {
        MatchesDisk,
        StaleCaller,
        MissingCaller,
        StaleDefs,
    }
    // (verdict row; skipped_drifted; rows_written)
    type Probe = (Option<(String, Option<i64>, String)>, u64, u64);
    let build = |snapshot: Snapshot| -> Probe {
        let h = Harness::new();
        let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        let _target_sym = h.add_symbol(defs, "target", 3, 9);
        let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

        let sym = "scip-rust crate v1 `target`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                occurrences: vec![occurrence(
                    0,
                    14,
                    20,
                    sym,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "defs.rs".to_string(),
            occurrences: vec![occurrence(0, 3, 9, sym, SymbolRole::Definition as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        });
        let bytes = index.write_to_bytes().unwrap();

        // Hash the disk bytes the way the join does, so MatchesDisk pins the actual current
        // content.
        let disk_hash =
            |rel: &str| super::run::hex_sha256(&std::fs::read(h.root().join(rel)).unwrap());
        let stale = "0000000000000000000000000000000000000000000000000000000000000000".to_string();
        let mut production: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // Default: both documents match disk. Each arm then drifts exactly one (or omits it).
        production.insert("caller.rs".to_string(), disk_hash("caller.rs"));
        production.insert("defs.rs".to_string(), disk_hash("defs.rs"));
        match snapshot {
            Snapshot::MatchesDisk => {},
            // The subprocess saw older CALL-SITE content than what's on disk at join time.
            Snapshot::StaleCaller => {
                production.insert("caller.rs".to_string(), stale);
            },
            Snapshot::MissingCaller => {
                production.remove("caller.rs"); // unreadable at production → absent from the snapshot
            },
            // The call site is pristine, but the watcher reindexed the DEFINITION file in the
            // window: the resolved-symbol byte range is converted against drifted bytes, so the
            // verdict must be skipped even though `caller.rs` passes the call-site gate.
            Snapshot::StaleDefs => {
                production.insert("defs.rs".to_string(), stale);
            },
        }

        let report = run_oracle(
            &h.conn,
            TOOL,
            VERSION,
            COMMIT,
            WORKTREE,
            &bytes,
            h.root(),
            Some(&production),
            None,
        )
        .unwrap();
        (h.verdict(edge), report.skipped_drifted, report.rows_written)
    };

    let (verdict, skipped, written) = build(Snapshot::MatchesDisk);
    assert!(verdict.is_some(), "a snapshot matching disk IS verdicted");
    assert_eq!(skipped, 0);
    assert_eq!(written, 1);

    let (verdict, skipped, written) = build(Snapshot::StaleCaller);
    assert!(verdict.is_none(), "a stale production snapshot must NOT be verdicted (TOCTOU)");
    assert_eq!(skipped, 1, "the stale-snapshot candidate is tallied in skipped_drifted");
    assert_eq!(written, 0, "nothing written from a `.scip` describing superseded content");

    let (verdict, skipped, written) = build(Snapshot::MissingCaller);
    assert!(
        verdict.is_none(),
        "a candidate absent from the production snapshot must NOT be verdicted"
    );
    assert_eq!(skipped, 1);
    assert_eq!(written, 0);

    // The def-document leg of the gate: call site pristine, definition file drifted from the
    // snapshot → the resolved-target conversion is untrustworthy, so the verdict is skipped too.
    let (verdict, skipped, written) = build(Snapshot::StaleDefs);
    assert!(
        verdict.is_none(),
        "a verdict whose DEFINITION document drifted must NOT be verdicted (def-doc TOCTOU)"
    );
    assert_eq!(skipped, 1, "the stale-def candidate is tallied in skipped_drifted");
    assert_eq!(written, 0);
}

/// Finding 3: a run recorded for ANOTHER worktree (same tool/version/commit) does NOT surface as
/// this checkout's last run. `oracle_runs` now carries `worktree_id` and `last_run_meta` filters on
/// it, so the status read describes only the active checkout — consistent with its worktree-scoped
/// verdict counts.
#[test]
fn last_run_meta_is_scoped_to_active_worktree() {
    let h = Harness::new();
    // A run in a SIBLING worktree (same tool/version/commit, distinct worktree id). It must not be
    // THIS checkout's last. `oracle_runs.worktree_id` scoping is orthogonal to the file-predicate
    // fix, so this uses a non-empty sibling worktree id directly rather than the file-level
    // `OTHER_*` constants.
    store::record_oracle_run(&h.conn, TOOL, VERSION, COMMIT, "sibling-wt", "Completed", "{}")
        .unwrap();

    // This checkout has no run yet → no last run, despite the sibling's row existing.
    let status = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(status.last_run_status, None, "the sibling worktree's run is not ours");
    assert_eq!(status.last_run_commit_sha, None);

    // Record a run in THIS checkout → now it's the last run.
    store::record_oracle_run(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, "Blocked", "{}").unwrap();
    let status = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(status.last_run_status.as_deref(), Some("Blocked"));
}

/// Finding 5: a file marked deleted (a `kind='deleted'` tombstone left by `mark_file_deleted`) but
/// still present in the `.scip` must NOT have its occurrences inflate the recall gap. The tombstone
/// is not "indexed in scope", so an uncovered call from it is out of scope, not a miss. A live file
/// with the same occurrence IS counted.
#[test]
fn deleted_file_occurrences_do_not_inflate_gap() {
    let build = |deleted: bool| -> u64 {
        let h = Harness::new();
        // `target` is defined in an indexed, live file (def-side filter passes).
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        h.add_symbol(defs, "target", 3, 9);
        // The call site lives in caller.rs.
        let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
        if deleted {
            // Tombstone it: a `kind='deleted'` row, as `mark_file_deleted` leaves.
            h.conn
                .execute("UPDATE files SET kind = 'deleted' WHERE id = ?1", params![caller])
                .unwrap();
        }

        let sym = "scip-rust crate v1 `target`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                occurrences: vec![occurrence(
                    0,
                    14,
                    20,
                    sym,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "defs.rs".to_string(),
            occurrences: vec![occurrence(0, 3, 9, sym, SymbolRole::Definition as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        });
        let bytes = index.write_to_bytes().unwrap();
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None)
            .unwrap()
            .oracle_only_calls
    };

    assert_eq!(build(true), 0, "a tombstoned source file's occurrence is NOT a recall gap");
    assert_eq!(build(false), 1, "the same call from a live indexed file IS a recall gap");
}

/// Finding 6a: the oracle's checkout scope is leak-proof BY CONSTRUCTION — every raw `edge_oracle`
/// query lives in `store.rs` (which owns the scoped helpers + the `edge_oracle_scope_join`
/// predicate). This source-scan test fails CI if a future unscoped `FROM edge_oracle` query is
/// added to any other oracle module (run.rs / status.rs / join.rs / scip.rs), forcing it back
/// through the scoped helper.
#[test]
fn raw_edge_oracle_queries_live_only_in_store() {
    let oracle_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/index/oracle");
    for entry in std::fs::read_dir(&oracle_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        // store.rs owns the scoped helpers; tests.rs exercises them with direct assertions.
        if name == "store.rs" || name == "tests.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("FROM edge_oracle"),
            "{name} contains a raw `FROM edge_oracle` query — route it through \
             store::count_edge_oracle_scoped / edge_oracle_scope_join so it can't drop the \
             checkout scope",
        );
    }
}

/// Finding 6b: `name_only_recovery_rate` cannot exceed 1.0 even if a writer bug stamps an `upgrade`
/// verdict on an `Exact`-confidence edge (e.g. an Exact edge with a NULL `to_symbol_id` that
/// `classify_resolved` would treat as unresolved). The numerator is now scoped to `upgrade`
/// verdicts on `NameOnly`/`Ambiguous` edges only, so the stray Exact upgrade is excluded — the rate
/// stays over the low-confidence population the denominator counts.
#[test]
fn name_only_recovery_rate_excludes_exact_upgrades() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() {}\n");
    // The legitimate low-confidence population: one NameOnly edge the oracle upgraded (1/1 = 1.0).
    let e_low = h.add_edge(f, "u", 0, 1, "NameOnly", None);
    // A pathological Exact edge ALSO stamped `upgrade` (the writer-bug shape). It is NOT in the
    // low-confidence denominator, so admitting it in the numerator (the old raw `counts.upgraded`)
    // would push the rate to 2/1 = 2.0.
    let e_exact = h.add_edge(f, "x", 1, 2, "Exact", None);
    let sha = h.file_sha("a.rs");

    h.write_verdict(e_low, &sha, None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(e_exact, &sha, None, "s", OracleResolutionKind::Upgrade);

    let m = super::oracle_eval_metrics(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        RecallCalls::default(),
    )
    .unwrap();
    // Raw count still reports both upgrade rows for transparency…
    assert_eq!(m.upgraded, 2);
    // …but the rate is scoped to the low-confidence population: 1 low-conf upgrade / 1 low-conf
    // edge-with-oracle = 1.0, never 2.0.
    assert!(
        m.name_only_recovery_rate <= 1.0,
        "recovery rate {} exceeds 1.0",
        m.name_only_recovery_rate
    );
    assert!((m.name_only_recovery_rate - 1.0).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Phase 2 (#69): read-side surfacing helpers — `Compiler` tier, staleness/dirty
// revert, `resolved-external`, `compare_graph_to_scip` data, and gc pruning of
// `oracle_runs`. All deterministic against the synthetic harness conn (no
// rust-analyzer, no `.scip` subprocess).
// ---------------------------------------------------------------------------
