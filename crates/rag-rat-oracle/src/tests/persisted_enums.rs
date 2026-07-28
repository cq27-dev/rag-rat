use super::*;

// ---------------------------------------------------------------------------
// mod.rs — persisted-enum round trips.
// ---------------------------------------------------------------------------

/// Both persisted enums round-trip through `as_db_str` / `from_db_str` for every variant, and
/// reject an unknown string — the `rust-modern-style` closed-enum contract.
#[test]
fn persisted_enums_round_trip_through_db_strings() {
    // These strings are SCHEMA: they key `edge_oracle.tool` / `oracle_runs.tool`, so changing one
    // makes existing rows unreadable. Spelled out literally rather than derived from `as_db_str`,
    // which would make the test agree with any change it was meant to catch.
    let tokens = [
        (OracleTool::RustAnalyzer, "rust-analyzer"),
        (OracleTool::ScipClang, "scip-clang"),
        (OracleTool::ScipPython, "scip-python"),
        (OracleTool::ScipTypescript, "scip-typescript"),
        (OracleTool::ScipJava, "scip-java"),
        (OracleTool::RaLsp, "ra-lsp"),
        (OracleTool::TsLsp, "ts-lsp"),
    ];
    for (tool, token) in tokens {
        assert_eq!(tool.as_db_str(), token);
        assert_eq!(OracleTool::from_db_str(tool.as_db_str()), Some(tool));
    }
    // A hand-written table silently falls behind the enum — a new variant would ship with an
    // untested persisted token. Pin the coverage, not just the entries that happen to be listed.
    assert_eq!(
        tokens.map(|(tool, _)| tool).to_vec(),
        OracleTool::ALL.to_vec(),
        "every OracleTool variant needs its exact persisted token pinned here, in ALL order",
    );
    assert_eq!(OracleTool::from_db_str("no-such-tool"), None);

    for (kind, token) in [
        (OracleResolutionKind::Upgrade, "upgrade"),
        (OracleResolutionKind::ResolvedExternal, "resolved-external"),
        (OracleResolutionKind::Confirm, "confirm"),
        (OracleResolutionKind::Contradict, "contradict"),
    ] {
        assert_eq!(kind.as_db_str(), token);
        assert_eq!(OracleResolutionKind::from_db_str(kind.as_db_str()), Some(kind));
    }
    assert_eq!(OracleResolutionKind::from_db_str("nonsense"), None);
}
