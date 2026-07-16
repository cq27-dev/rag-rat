use super::*;

/// Seed one edge with a written verdict and return `(harness, edge_id, file_sha)`. The verdict's
/// `file_sha` matches the file's recorded sha (current), so it surfaces; tests that want staleness
/// drift the edge's file sha afterward. When `in_corpus` is set, a real `target` symbol is inserted
/// and the verdict resolves to its id — so the def-drift gate (`resolved_symbol_id` must still
/// EXIST in `symbols`) is satisfied for current verdicts. `None` (external) verdicts skip that
/// gate.
fn seed_verdict(
    kind: OracleResolutionKind,
    scip_symbol: &str,
    in_corpus: bool,
) -> (Harness, i64, String) {
    let (h, edge, file_sha, _resolved) = seed_verdict_full(kind, scip_symbol, in_corpus);
    (h, edge, file_sha)
}

/// Like [`seed_verdict`] but also returns the in-corpus `resolved_symbol_id` (when any), so a test
/// can delete/reindex that definition symbol and assert the verdict stops surfacing (#82 finding
/// 3).
fn seed_verdict_full(
    kind: OracleResolutionKind,
    scip_symbol: &str,
    in_corpus: bool,
) -> (Harness, i64, String, Option<i64>) {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let file_sha: String = h
        .conn
        .query_row("SELECT sha256 FROM files WHERE id = ?1", params![f], |r| r.get(0))
        .unwrap();
    // An in-corpus resolution must point at a symbol that EXISTS — the def-drift gate in
    // `edge_oracle_current_predicate` filters a dangling `resolved_symbol_id`.
    let resolved_symbol_id = in_corpus.then(|| h.add_symbol(f, "target", 3, 9));
    let edge = h.add_edge(f, "target", 14, 20, "NameOnly", None);
    h.write_verdict(edge, &file_sha, resolved_symbol_id, scip_symbol, kind);
    (h, edge, file_sha, resolved_symbol_id)
}

/// A CURRENT verdict (its `file_sha` matches `files.sha256`) is returned by the surfacing read —
/// the `Compiler` tier data.
#[test]
fn current_verdict_is_surfaced_for_edge() {
    let (h, edge, _sha) = seed_verdict(OracleResolutionKind::Upgrade, "scip x `target`().", true);
    let verdicts =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &[edge])
            .unwrap();
    let verdict = verdicts.get(&edge).expect("current verdict surfaced");
    assert_eq!(verdict.kind, OracleResolutionKind::Upgrade);
    assert_eq!(verdict.resolution_reason(), format!("scip:{}@{VERSION}", TOOL.as_db_str()));
}

/// Staleness revert: drift the edge's file sha so it no longer matches the verdict's `file_sha`.
/// The surfacing read excludes it — the edge reverts to heuristic display, never `Compiler`.
#[test]
fn drifted_file_verdict_is_not_surfaced() {
    let (h, edge, _sha) = seed_verdict(OracleResolutionKind::Upgrade, "scip x `target`().", true);
    // The file's content changed since the verdict was computed: its sha now differs from
    // `edge_oracle.file_sha`, so the current-content predicate filters the verdict out.
    h.conn.execute("UPDATE files SET sha256 = 'drifted-sha' WHERE path = 'a.rs'", []).unwrap();
    let verdicts =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &[edge])
            .unwrap();
    assert!(verdicts.is_empty(), "a drifted file's verdict must not surface as Compiler");
}

/// The whole-graph scan ([`store::current_oracle_verdicts_all`], used by symbol-importance ranking)
/// returns the same current+in-scope verdicts as the per-edge read — `(kind, resolved_symbol_id)`
/// keyed by edge id — and applies the same currency gate (a drifted callsite drops out).
#[test]
fn current_oracle_verdicts_all_returns_scoped_current() {
    let (h, edge, _sha, resolved) =
        seed_verdict_full(OracleResolutionKind::Upgrade, "scip x `target`().", true);
    let all = store::current_oracle_verdicts_all(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(
        all.get(&edge),
        Some(&(OracleResolutionKind::Upgrade, resolved)),
        "the whole-graph scan returns the current verdict with its resolved symbol id"
    );

    // Drift the callsite file: the currency gate must drop the verdict from the whole-graph scan
    // too, exactly as it does for the per-edge read.
    h.conn.execute("UPDATE files SET sha256 = 'drifted-sha' WHERE path = 'a.rs'", []).unwrap();
    let after =
        store::current_oracle_verdicts_all(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert!(after.is_empty(), "a drifted file's verdict must not surface in the whole-graph scan");
}

/// Def-drift revert (#82 finding 3): an in-corpus verdict keeps its callsite file unchanged (so the
/// `file_sha` gate still matches), but its resolved DEFINITION symbol is deleted/reinserted by
/// incremental reindexing — the old `resolved_symbol_id` dangles. The surfacing read must drop the
/// verdict (the def the compiler resolved to no longer exists), reverting to heuristic display.
#[test]
fn resolved_def_drift_verdict_is_not_surfaced() {
    let (h, edge, _sha, resolved) =
        seed_verdict_full(OracleResolutionKind::Upgrade, "scip x `target`().", true);
    let resolved = resolved.expect("in-corpus verdict has a resolved symbol id");
    // Sanity: while the resolved def symbol exists, the verdict surfaces.
    let before =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &[edge])
            .unwrap();
    assert!(before.contains_key(&edge), "current in-corpus verdict surfaces before def drift");
    // The def file was reindexed: AUTOINCREMENT mints new ids, so the old resolved symbol id is
    // gone. Model that by deleting the resolved symbol row.
    h.conn.execute("DELETE FROM symbols WHERE id = ?1", params![resolved]).unwrap();
    let after =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &[edge])
            .unwrap();
    assert!(after.is_empty(), "a verdict whose resolved definition drifted must not surface");
}

/// Overlay def-drift (#82 P2): when the *def* file goes dirty, the indexer inserts a
/// worktree-scoped overlay row and leaves the old commit-scoped symbols shadowed-but-PRESENT (not
/// deleted). A raw `EXISTS (symbols.id = resolved_symbol_id)` would still find the stale id and
/// keep surfacing a `Compiler` verdict pointing at the pre-edit target (the CALLSITE file is
/// untouched, so its sha still matches). The scope-aware def-drift EXISTS — which joins `symbols ->
/// files` and applies the active-checkout predicate — must treat the shadowed commit-scoped def as
/// out of scope, reverting the verdict to heuristic. Callsite and def are in SEPARATE files so only
/// the def's scope changes.
#[test]
fn overlay_shadowed_def_verdict_is_not_surfaced() {
    let h = Harness::new();
    // Callsite in `caller.rs` (stays committed); def in `defs.rs` (will get an overlay).
    // The active context for a dirty checkout carries a real worktree id (the root path) alongside
    // the HEAD commit — `resolve_git_context` always returns the root as `worktree_id`. Both the
    // committed (clean) caller row and the dirty overlay use that id.
    let active_wt = "/some/checkout/root";
    let caller = h.add_file_in_scope("caller.rs", COMMIT, "");
    h.conn
        .execute("UPDATE files SET sha256 = 'caller-sha' WHERE id = ?1", params![caller])
        .unwrap();
    let defs = h.add_file_in_scope("defs.rs", COMMIT, "");
    let resolved = h.add_symbol(defs, "target", 3, 9);
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);
    h.write_verdict(
        edge,
        "caller-sha",
        Some(resolved),
        "scip x `target`().",
        OracleResolutionKind::Upgrade,
    );

    // Sanity: with no overlay, the committed def is in scope → the verdict surfaces.
    let before =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, active_wt, &[
            edge,
        ])
        .unwrap();
    assert!(before.contains_key(&edge), "verdict surfaces before the def file goes dirty");

    // The def file goes dirty: a worktree-scoped overlay row for `defs.rs` is inserted; the
    // committed `defs.rs` row (and its `target` symbol) stay shadowed-but-present. The active
    // worktree id matches the overlay, so the committed def is now shadowed out of scope.
    h.add_file_in_scope("defs.rs", "", active_wt);

    let after =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, active_wt, &[
            edge,
        ])
        .unwrap();
    assert!(
        after.is_empty(),
        "a verdict whose resolved def is shadowed by a dirty overlay must not keep surfacing"
    );
}

/// A verdict whose edge lives in a different checkout never surfaces for the active one: querying
/// the seeded (clean-checkout) edge under a DIFFERENT commit is out of the active-checkout scope
/// join, so the verdict is excluded. (Commit, not worktree id, is the isolation boundary for a
/// commit-scoped row — a clean file is visible from any worktree-overlay query at the same commit,
/// so a sibling-worktree query at the SAME commit would correctly still see it; the genuine
/// out-of-scope case is a different commit.)
#[test]
fn out_of_scope_verdict_is_not_surfaced() {
    let (h, edge, sha) = seed_verdict(OracleResolutionKind::Upgrade, "scip x `target`().", true);
    // Query the SAME edge under a different commit's scope: the scope join excludes it.
    let verdicts = store::current_oracle_verdicts_for_edges(
        &h.conn,
        TOOL,
        VERSION,
        "a-different-commit-sha",
        WORKTREE,
        &[edge],
    )
    .unwrap();
    assert!(verdicts.is_empty(), "a verdict outside the active checkout must not surface");
    let _ = sha;
}

/// #82 P3: the `--scip` run-id fingerprint is a stable 12-hex-char content hash, distinct for
/// distinct bytes — so two indexes sharing a basename don't collide onto one `tool_version`.
#[test]
fn scip_content_fingerprint_is_stable_and_content_distinct() {
    let a = super::scip_content_fingerprint(b"index-A-bytes");
    let b = super::scip_content_fingerprint(b"index-B-bytes");
    assert_eq!(a.len(), 12, "12 hex chars");
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(a, super::scip_content_fingerprint(b"index-A-bytes"), "stable for identical bytes");
    assert_ne!(a, b, "distinct bytes → distinct fingerprint (no basename collision)");
}

/// A `resolved-external` verdict surfaces a `resolved-external(<package>)` label derived from the
/// SCIP symbol's package component.
#[test]
fn resolved_external_label_surfaces_package() {
    let (h, edge, _sha) = seed_verdict(
        OracleResolutionKind::ResolvedExternal,
        "scip-rust cargo tokio 1.0 `spawn`().",
        false,
    );
    let verdicts =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &[edge])
            .unwrap();
    let verdict = verdicts.get(&edge).expect("verdict surfaced");
    assert_eq!(verdict.resolved_external_label().as_deref(), Some("resolved-external(tokio)"));
}

/// `current_oracle_comparisons` returns CURRENT, in-scope verdicts joined to the heuristic edge —
/// the `compare_graph_to_scip` data. A `Contradict` verdict appears with its scip symbol; a drifted
/// row does not.
#[test]
fn comparisons_return_current_contradictions_only() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let sha: String = h
        .conn
        .query_row("SELECT sha256 FROM files WHERE id = ?1", params![f], |r| r.get(0))
        .unwrap();
    let target = h.add_symbol(f, "target", 3, 9);
    // An Exact edge the heuristic resolved to `target`; the oracle CONTRADICTS it (points
    // elsewhere).
    let edge = h.add_edge(f, "target", 14, 20, "Exact", Some(target));
    h.write_verdict(
        edge,
        &sha,
        None,
        "scip-rust cargo other 1.0 `target`().",
        OracleResolutionKind::Contradict,
    );

    let comparisons =
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(comparisons.len(), 1);
    let c = &comparisons[0];
    assert_eq!(c.kind, OracleResolutionKind::Contradict);
    assert_eq!(c.edge_id, edge);
    assert_eq!(c.heuristic_confidence, "Exact");
    assert_eq!(c.scip_symbol, "scip-rust cargo other 1.0 `target`().");

    // Drift the file → the comparison drops out (no stale contradiction surfaced).
    h.conn.execute("UPDATE files SET sha256 = 'drift' WHERE id = ?1", params![f]).unwrap();
    let after =
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert!(after.is_empty(), "a drifted file's contradiction must not surface");
}

/// `latest_run_tool_version` returns the most recent run's version for the active checkout, and
/// `None` when there is no run.
#[test]
fn latest_run_tool_version_tracks_active_checkout() {
    let h = Harness::new();
    assert_eq!(store::latest_run_tool_version(&h.conn, TOOL, COMMIT, WORKTREE).unwrap(), None);
    store::record_oracle_run(&h.conn, TOOL, "v1", COMMIT, WORKTREE, "Completed", "{}").unwrap();
    store::record_oracle_run(&h.conn, TOOL, "v2", COMMIT, WORKTREE, "Completed", "{}").unwrap();
    assert_eq!(
        store::latest_run_tool_version(&h.conn, TOOL, COMMIT, WORKTREE).unwrap().as_deref(),
        Some("v2")
    );
    // A sibling worktree's run does not leak in.
    store::record_oracle_run(&h.conn, TOOL, "v3", COMMIT, "other", "Completed", "{}").unwrap();
    assert_eq!(
        store::latest_run_tool_version(&h.conn, TOOL, COMMIT, WORKTREE).unwrap().as_deref(),
        Some("v2")
    );
}

/// gc: `prune_oracle_runs_outside_scope` drops runs whose `(commit, worktree)` is dead, keeps live
/// ones, and refuses to prune when both live sets are empty (so a missing live set never wipes all
/// run history).
#[test]
fn prune_oracle_runs_drops_dead_contexts_only() {
    let h = Harness::new();
    store::record_oracle_run(&h.conn, TOOL, "v1", "live-commit", "live-wt", "Completed", "{}")
        .unwrap();
    store::record_oracle_run(&h.conn, TOOL, "v1", "dead-commit", "dead-wt", "Completed", "{}")
        .unwrap();
    // A run whose commit is dead but whose worktree overlay is live survives (OR rule).
    store::record_oracle_run(&h.conn, TOOL, "v1", "dead-commit", "live-wt", "Completed", "{}")
        .unwrap();

    let live_commits = vec!["live-commit".to_string()];
    let live_worktrees = vec!["live-wt".to_string()];

    // Empty live sets are a no-op (never wipe everything).
    assert_eq!(store::prune_oracle_runs_outside_scope(&h.conn, &[], &[]).unwrap(), 0);

    let deleted =
        store::prune_oracle_runs_outside_scope(&h.conn, &live_commits, &live_worktrees).unwrap();
    assert_eq!(deleted, 1, "only the (dead-commit, dead-wt) run is pruned");
    let remaining: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM oracle_runs", [], |r| r.get(0)).unwrap();
    assert_eq!(remaining, 2);
}

/// gc (#248): `prune_edge_oracle_without_live_edge` is a GLOBAL sweep — it deletes a verdict whose
/// content key matches NO live edge in ANY scope, but KEEPS one that matches a live edge anywhere
/// (so a sibling worktree's still-live verdict is never swept by a sweep run in another checkout).
/// This is the gc hygiene that replaces the dropped FK cascade; correctness never depended on it
/// (dangling verdicts already never resolve via the live join — see
/// `edge_oracle_survives_reindex_for_unchanged_file`), so this only guards unbounded growth.
#[test]
fn gc_prunes_edge_oracle_rows_with_no_live_edge() {
    let h = Harness::new();

    // (A) A verdict whose edge is LIVE in the active checkout — must be kept.
    let live_file = h.add_file("live.rs", "fn caller() { target(); }\n");
    let live_sha = h.file_sha("live.rs");
    let live_edge = h.add_edge(live_file, "target", 14, 20, "Exact", None);
    h.write_verdict(
        live_edge,
        &live_sha,
        None,
        "scip x `target`().",
        OracleResolutionKind::Confirm,
    );

    // (B) A verdict whose edge lives in a SIBLING checkout (another commit). The global sweep does
    // NOT apply the active-checkout predicate, so it must still see this edge as live and keep its
    // verdict — a sweep in THIS checkout must never delete a sibling's live verdict.
    let sibling_file = h.add_file_in_scope("sibling.rs", OTHER_COMMIT, OTHER_WORKTREE);
    let sibling_sha = h.file_sha_for_commit("sibling.rs", OTHER_COMMIT);
    let sibling_edge = h.add_edge(sibling_file, "thing", 14, 19, "Exact", None);
    h.write_verdict(
        sibling_edge,
        &sibling_sha,
        None,
        "scip x `thing`().",
        OracleResolutionKind::Confirm,
    );

    // (C) A DANGLING verdict — content key matches no live edge anywhere (the edge was deleted in a
    // reindex, leaving the FK-less verdict behind). Build it by writing a verdict, then deleting
    // its edge (simulating `remove_file_in_scope` dropping a changed file's edges).
    let dangling_file = h.add_file("dangling.rs", "fn caller() { gone(); }\n");
    let dangling_sha = h.file_sha("dangling.rs");
    let dangling_edge = h.add_edge(dangling_file, "gone", 14, 18, "Exact", None);
    h.write_verdict(
        dangling_edge,
        &dangling_sha,
        None,
        "scip x `gone`().",
        OracleResolutionKind::Confirm,
    );
    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![dangling_edge]).unwrap();

    let before: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(before, 3, "three verdicts before the sweep (live, sibling, dangling)");

    let deleted = store::prune_edge_oracle_without_live_edge(&h.conn).unwrap();
    assert_eq!(deleted, 1, "only the dangling verdict (no live edge anywhere) is swept");

    let after: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(after, 2, "the live + sibling verdicts survive the global sweep");
    // The two survivors still resolve to their live edges by content key.
    assert!(h.verdict(live_edge).is_some(), "the active-checkout verdict is kept");
    assert!(
        h.verdict(sibling_edge).is_some(),
        "the sibling-checkout verdict is kept (global sweep)"
    );
}
