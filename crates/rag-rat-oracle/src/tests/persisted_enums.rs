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
        (OracleTool::ClangdLsp, "clangd-lsp"),
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

/// The `edge_oracle.kind` SQL lists spliced into the metric and seed queries are rebuilt from
/// `as_db_str`, so a renamed variant fails here instead of silently counting zero rows.
#[test]
fn resolution_kind_sql_lists_spell_the_persisted_tokens() {
    use OracleResolutionKind::{Confirm, ResolvedExternal, Upgrade};
    let quoted = |kind: OracleResolutionKind| format!("'{}'", kind.as_db_str());
    let list = |kinds: &[OracleResolutionKind]| {
        format!("({})", kinds.iter().map(|&kind| quoted(kind)).collect::<Vec<_>>().join(", "))
    };
    assert_eq!(OracleResolutionKind::UPGRADE_SQL, quoted(Upgrade));
    assert_eq!(OracleResolutionKind::UPGRADEABLE_SQL, list(&[Upgrade, ResolvedExternal]));
    assert_eq!(OracleResolutionKind::IN_CORPUS_SQL, list(&[Upgrade, Confirm]));
}

/// `RunStatus` is persisted as `oracle_runs.status` and rides `stats_json`, so its tokens are
/// schema too — and `rag-rat-core`'s eval asserts the stored `Completed` literally. Pinned as
/// literals, through both the DB string and the serde form, for the same reason as above.
#[test]
fn run_status_tokens_are_pinned_through_db_strings_and_serde() {
    let aborted = RunStatus::Aborted("the server exited".to_string());
    for (status, token) in [
        (RunStatus::Completed, "Completed"),
        (RunStatus::Warming, "Warming"),
        (RunStatus::VersionMigrated, "VersionMigrated"),
        (RunStatus::VersionMigrationBlocked, "VersionMigrationBlocked"),
        (RunStatus::NoVerdicts, "NoVerdicts"),
        (RunStatus::BudgetExhausted, "BudgetExhausted"),
        (aborted, "Aborted: the server exited"),
    ] {
        assert_eq!(status.as_db_str(), token);
        assert_eq!(status.to_string(), token);
        assert_eq!(serde_json::to_value(&status).unwrap(), serde_json::json!(token));
        assert_eq!(RunStatus::from_db_str(token), Some(status));
    }
    // A bare `Aborted` is not a token the crate writes, and an unknown one stays unknown: the
    // status read-back keeps historical rows as raw strings rather than failing on them.
    assert_eq!(RunStatus::from_db_str("Aborted"), None);
    assert_eq!(RunStatus::from_db_str("Blocked"), None);
}
