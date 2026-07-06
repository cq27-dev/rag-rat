use super::*;

#[test]
fn total_occurrences_counts_data_and_distinguishes_an_empty_shell() {
    use super::scip::ScipIndex;
    // A real index with occurrences → the raw count; gates the diagnostic-exit tolerance in
    // produce_scip_with_tool (#198 review) so an empty shell from an early-bailing tool isn't
    // accepted on a non-zero exit.
    let with_data = scip_bytes("a.py", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 0, 3, "scip x `a`/foo().", SymbolRole::Definition as i32),
        occurrence(1, 4, 7, "scip x `a`/foo().", 0),
    ]);
    assert_eq!(ScipIndex::total_occurrences(&with_data).unwrap(), 2);
    // A parseable index with documents but ZERO occurrences — the empty shell the gate must reject.
    let empty_shell = scip_bytes("a.py", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![]);
    assert_eq!(ScipIndex::total_occurrences(&empty_shell).unwrap(), 0);
    // Non-SCIP bytes don't parse → Err (the caller treats that as 0 / unusable).
    assert!(ScipIndex::total_occurrences(b"not a scip index").is_err());
}

#[test]
fn accept_produced_index_tolerates_diagnostic_exit_only_with_usable_data() {
    use std::path::Path;

    use super::accept_produced_index;
    let p = Path::new("/tmp/out.scip");
    let data =
        scip_bytes("a.py", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occurrence(
            0,
            0,
            3,
            "scip x `a`/foo().",
            SymbolRole::Definition as i32,
        )]);
    let empty_shell = scip_bytes("a.py", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![]);

    // Clean exit: needs only non-empty bytes (join + health gate validate the rest); an empty doc
    // shell is fine here (a 0-occurrence run trips the health gate downstream, not this). The
    // `tolerate_diagnostic_exit` flag is irrelevant on a clean exit.
    assert!(accept_produced_index(true, true, &data, "scip-python", p).unwrap().is_none());
    assert!(
        accept_produced_index(true, false, &empty_shell, "rust-analyzer", p).unwrap().is_none()
    );
    assert!(
        accept_produced_index(true, true, b"", "scip-python", p).is_err(),
        "clean exit, no bytes → bail"
    );

    // Non-zero exit, DIAGNOSTIC-exit tool (scip-python): tolerated ONLY with usable occurrences.
    let note = accept_produced_index(false, true, &data, "scip-python", p).unwrap();
    assert!(note.expect("tolerated note").contains("1 occurrences"));
    assert!(
        accept_produced_index(false, true, &empty_shell, "scip-python", p).is_err(),
        "non-zero exit + empty doc shell (0 occurrences) → bail"
    );
    assert!(
        accept_produced_index(false, true, b"", "scip-python", p).is_err(),
        "non-zero exit + no index → bail"
    );

    // Non-zero exit, NON-diagnostic tool (rust-analyzer/scip-clang): a real failure — bail even
    // with a parseable, occurrence-bearing index (it could be a crashed run's partial output).
    assert!(
        accept_produced_index(false, false, &data, "rust-analyzer", p).is_err(),
        "non-diagnostic backend's non-zero exit is a real failure → bail regardless of index"
    );
}
