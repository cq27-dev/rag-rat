use super::*;

// ---------------------------------------------------------------------------
// status.rs — status construction + serialization, last-run metadata.
// ---------------------------------------------------------------------------

/// `oracle_status` reflects the persisted verdict counts and the most recent run's status/commit;
/// it serializes to JSON with the documented field names.
#[test]
fn oracle_status_reports_counts_and_last_run() {
    let h = Harness::new();
    // No verdicts, no runs yet → zeros and `None` last run.
    let empty = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(empty.total_verdicts, 0);
    assert_eq!(empty.last_run_status, None);
    assert_eq!(empty.last_run_commit_sha, None);

    // Two runs in THIS checkout; the later one wins for "last run". Both recorded under the active
    // `(COMMIT, WORKTREE)` so the worktree-scoped `last_run_meta` can see them (finding 3).
    store::record_oracle_run(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, "Completed", "{}").unwrap();
    store::record_oracle_run(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, "Blocked", "{}").unwrap();

    let f = h.add_file("a.rs", "x\n");
    let sha = h.file_sha("a.rs");
    let e1 = h.add_edge(f, "a", 0, 1, "NameOnly", None);
    let e2 = h.add_edge(f, "b", 1, 2, "Exact", None);
    h.write_verdict(e1, &sha, None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(e2, &sha, None, "s", OracleResolutionKind::Contradict);

    let status = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(status.tool, "rust-analyzer");
    assert_eq!(status.tool_version, VERSION);
    assert_eq!(status.total_verdicts, 2);
    assert_eq!(status.upgraded, 1);
    assert_eq!(status.contradicted, 1);
    assert_eq!(status.confirmed, 0);
    assert_eq!(status.last_run_status.as_deref(), Some("Blocked"));
    assert_eq!(status.last_run_commit_sha.as_deref(), Some(COMMIT));

    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["total_verdicts"], 2);
    assert_eq!(json["last_run_commit_sha"], COMMIT);
}
