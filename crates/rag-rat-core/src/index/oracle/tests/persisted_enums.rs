use super::*;

// ---------------------------------------------------------------------------
// mod.rs — persisted-enum round trips.
// ---------------------------------------------------------------------------

/// Both persisted enums round-trip through `as_db_str` / `from_db_str` for every variant, and
/// reject an unknown string — the `rust-modern-style` closed-enum contract.
#[test]
fn persisted_enums_round_trip_through_db_strings() {
    for &tool in OracleTool::ALL {
        assert_eq!(OracleTool::from_db_str(tool.as_db_str()), Some(tool));
    }
    assert_eq!(OracleTool::from_db_str("no-such-tool"), None);

    for kind in [
        OracleResolutionKind::Upgrade,
        OracleResolutionKind::ResolvedExternal,
        OracleResolutionKind::Confirm,
        OracleResolutionKind::Contradict,
    ] {
        assert_eq!(OracleResolutionKind::from_db_str(kind.as_db_str()), Some(kind));
    }
    assert_eq!(OracleResolutionKind::from_db_str("nonsense"), None);
}
