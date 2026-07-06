use super::*;

// ---------------------------------------------------------------------------
// Pre-spawn gate (#83): the mid-subprocess TOCTOU the post-exit snapshot can't see.
// ---------------------------------------------------------------------------

/// Build the standard caller/defs corpus + scip, then run the oracle with explicit
/// production/pre-spawn maps. Returns (verdict for the edge, report, defs logical id).
fn run_with_pins(
    pre_spawn: impl Fn(&str, &str) -> std::collections::HashMap<String, String>,
) -> (Option<(String, Option<i64>, String)>, super::OracleReport) {
    let h = Harness::new();
    let caller_text = "fn caller() { target(); }\n";
    let defs_text = "fn target() {}\n";
    let caller = h.add_file("caller.rs", caller_text);
    let defs = h.add_file("defs.rs", defs_text);
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    let bytes = scip_bytes_docs(vec![
        ("caller.rs", vec![occurrence(0, 14, 20, TARGET_MONIKER, 0)]),
        ("defs.rs", vec![occurrence(0, 3, 9, TARGET_MONIKER, SymbolRole::Definition as i32)]),
    ]);
    // The post-exit production snapshot agrees with disk (and disk agrees with the index): every
    // #82 gate passes. Only the PRE-SPAWN snapshot distinguishes the mid-subprocess scenarios.
    let caller_sha = sha256_hex(caller_text.as_bytes());
    let defs_sha = sha256_hex(defs_text.as_bytes());
    let production: std::collections::HashMap<String, String> =
        [("caller.rs".to_string(), caller_sha.clone()), ("defs.rs".to_string(), defs_sha.clone())]
            .into();
    let pre = pre_spawn(&caller_sha, &defs_sha);
    let report = run_oracle(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        &bytes,
        h.root(),
        Some(&production),
        Some(&pre),
    )
    .unwrap();
    let moniker_written = h.moniker(1001).is_some();
    assert_eq!(
        moniker_written,
        pre.get("defs.rs").map(String::as_str) == Some(defs_sha.as_str()),
        "moniker write must follow the def document's pre-spawn gate"
    );
    (h.verdict(edge), report)
}

/// Control: a pre-spawn snapshot matching the indexed shas changes nothing — the verdict lands.
#[test]
fn pre_spawn_gate_passes_when_nothing_reindexed() {
    let (verdict, report) = run_with_pins(|caller_sha, defs_sha| {
        [
            ("caller.rs".to_string(), caller_sha.to_string()),
            ("defs.rs".to_string(), defs_sha.to_string()),
        ]
        .into()
    });
    assert!(verdict.is_some(), "matching pre-spawn snapshot must not block the verdict");
    assert_eq!(report.skipped_drifted, 0);
    assert_eq!(report.oracle_only_calls, 0, "the covered call is no recall gap");
}

/// CALL-SITE document edited during the subprocess: index/disk/production all carry the NEW
/// content (every #82 gate passes), but the pre-spawn snapshot still has the OLD sha — the
/// `.scip` was built from bytes nobody can verify, so the candidate is skipped, never verdicted.
#[test]
fn pre_spawn_gate_skips_call_site_reindexed_mid_subprocess() {
    let (verdict, report) = run_with_pins(|_caller_sha, defs_sha| {
        [
            ("caller.rs".to_string(), "pre-spawn-old-sha".to_string()),
            ("defs.rs".to_string(), defs_sha.to_string()),
        ]
        .into()
    });
    assert!(verdict.is_none(), "mid-subprocess call-site reindex must skip the verdict");
    assert_eq!(report.skipped_drifted, 1);
    assert_eq!(report.rows_written, 0);
    // A skipped-as-drifted candidate is an ABSTENTION, not a heuristic miss: its occurrence must
    // not count as a recall gap (#88 review).
    assert_eq!(report.oracle_only_calls, 0, "drifted call-site doc is excluded from recall");
}

/// DEFINITION document edited during the subprocess: the call-site gate passes, but the resolved
/// symbol came from converting the def occurrence against bytes the pre-spawn snapshot can't
/// confirm — the verdict (and the moniker, asserted in the helper) is skipped.
#[test]
fn pre_spawn_gate_skips_definition_reindexed_mid_subprocess() {
    let (verdict, report) = run_with_pins(|caller_sha, _defs_sha| {
        [
            ("caller.rs".to_string(), caller_sha.to_string()),
            ("defs.rs".to_string(), "pre-spawn-old-sha".to_string()),
        ]
        .into()
    });
    assert!(verdict.is_none(), "mid-subprocess def reindex must skip the verdict");
    assert_eq!(report.skipped_drifted, 1);
    assert_eq!(report.rows_written, 0);
    // The call site is clean but its DEF document drifted: the occurrence resolving into it must
    // not count as a recall gap either (#88 review).
    assert_eq!(report.oracle_only_calls, 0, "drifted def doc is excluded from recall");
}
