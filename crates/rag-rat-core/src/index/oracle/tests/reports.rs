use super::*;

/// C2 live before/after report: the heuristic "before" counts come from the `edges` index, the
/// after-side verdict counts + run-only fields come from the run's `OracleReport`, the precision/
/// recall come from diffing `edge_oracle`, and the moniker tally from `logical_symbol_monikers` —
/// all stamped onto the C0 schema with the caller's profile + provenance.
#[test]
fn resolution_report_assembles_before_after_from_index() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() {}\n");

    // Heuristic "before": four Exact edges resolved to an in-corpus symbol (resolved_before = 4),
    // three NameOnly edges (unresolved_before = 3); total_edges = 7 (all carry a callee range).
    let target = h.add_symbol(f, "target", 3, 9);
    h.add_edge(f, "target", 14, 20, "Exact", Some(target));
    h.add_edge(f, "other", 21, 26, "Exact", Some(target));
    let up = h.add_edge(f, "up", 30, 32, "NameOnly", None);
    let ext = h.add_edge(f, "ext", 33, 36, "NameOnly", None);
    let _plain = h.add_edge(f, "plain", 37, 42, "NameOnly", None);
    let conf_sym = h.add_symbol(f, "conf", 43, 47);
    let conf = h.add_edge(f, "conf", 48, 52, "Exact", Some(conf_sym));
    let contra_sym = h.add_symbol(f, "contra", 53, 59);
    let contra = h.add_edge(f, "contra", 60, 66, "Exact", Some(contra_sym));

    // Oracle verdicts: Upgrade + ResolvedExternal on the NameOnly edges; Confirm + Contradict on
    // two Exact edges → precision = 1 / (1 + 1) = 0.5.
    let file_sha: String = h
        .conn
        .query_row("SELECT sha256 FROM files WHERE id = ?1", params![f], |r| r.get(0))
        .unwrap();
    let write = |edge: i64, kind: OracleResolutionKind, resolved: Option<i64>| {
        h.write_verdict(edge, &file_sha, resolved, "scip x `t`().", kind);
    };
    write(up, OracleResolutionKind::Upgrade, Some(target));
    write(ext, OracleResolutionKind::ResolvedExternal, None);
    write(conf, OracleResolutionKind::Confirm, Some(conf_sym));
    write(contra, OracleResolutionKind::Contradict, Some(contra_sym));

    // Two logical symbols enriched with a moniker for this tool. The `logical_symbols` rows must
    // exist for the enriched-symbol tally: `count_symbols_with_moniker` joins them and filters by
    // the active `repo_id` (A3, round-6 P2 #3), so a moniker with no live logical symbol is not an
    // "enriched symbol" — mirroring production, where oracle only writes a moniker for a resolved
    // logical symbol. The default `repo_id` (`__unassigned__`) matches this raw-conn harness's
    // sole/active repo.
    for id in [1_i64, 2] {
        h.conn
            .execute(
                "INSERT INTO logical_symbols(id, language, path, logical_name, kind, \
                 variant_count, group_reason) VALUES (?1, 'rust', 'a.rs', ?2, 'function', 1, \
                 'test')",
                params![id, format!("sym{id}")],
            )
            .unwrap();
        h.conn
            .execute(
                "INSERT INTO logical_symbol_monikers(logical_symbol_id, tool, tool_version, \
                 moniker, computed_at) VALUES (?1, ?2, ?3, ?4, 0)",
                params![id, TOOL.as_db_str(), VERSION, format!("m{id}")],
            )
            .unwrap();
    }

    let mut bindings = std::collections::BTreeMap::new();
    bindings.insert("rust".to_string(), vec!["src".to_string()]);
    let profile = super::CorpusProfile {
        corpus_id: "rust-test".to_string(),
        tier: "small".to_string(),
        repo: "r".to_string(),
        rev: "1".to_string(),
        tool: TOOL.as_db_str().to_string(),
        prepare: Vec::new(),
        bindings,
        health: super::CorpusHealth {
            expected_min_heuristic_edges: 1,
            expected_min_oracle_examined: 1,
            expected_max_skipped_drifted: 0,
            expected_min_symbols_with_moniker: 1,
            expected_min_resolved_external: None,
            timeout_minutes: 8,
        },
    };
    // `tool_version` MUST match the VERSION the verdicts were written under — it's the metric scope
    // key as well as the report envelope's provenance.
    let provenance = super::RunProvenance {
        tool_version: VERSION.to_string(),
        rag_rat_commit: "commit".to_string(),
        worktree_id: WORKTREE.to_string(),
        production_sha: "prod".to_string(),
    };
    let run = super::OracleReport {
        upgraded: 1,
        resolved_external: 1,
        confirmed: 1,
        contradicted: 1,
        covered_calls: 8,
        oracle_only_calls: 2,
        skipped_drifted: 2,
        monikers_written: 2,
        ..Default::default()
    };

    let report =
        super::resolution_report(&h.conn, &profile, &provenance, TOOL, COMMIT, WORKTREE, &run)
            .unwrap();

    // Before/after resolution.
    assert_eq!(report.resolution.total_edges, 7);
    assert_eq!(report.resolution.resolved_before, 4);
    assert_eq!(report.resolution.unresolved_before, 3);
    assert_eq!(report.resolution.resolved_after, 4 + 1 + 1);
    // Verdict transitions (from the run) + moniker tally (from the index) + run-only drift.
    assert_eq!(report.upgraded, 1);
    assert_eq!(report.resolved_external, 1);
    assert_eq!(report.confirmed, 1);
    assert_eq!(report.contradicted, 1);
    assert_eq!(report.symbols_with_moniker, 2);
    assert_eq!(report.skipped_drifted, 2);
    // Diffed metrics: precision 1/(1+1), recall covered/(covered+oracle_only) = 8/10.
    assert!((report.metrics.precision - 0.5).abs() < 1e-9, "precision: {report:?}");
    assert!((report.metrics.recall - 0.8).abs() < 1e-9, "recall: {report:?}");
    // Envelope.
    assert_eq!(report.corpus_profile_hash, profile.hash());
    assert_eq!(report.tool_version, VERSION);
    assert_eq!(report.tool, TOOL.as_db_str());
}
