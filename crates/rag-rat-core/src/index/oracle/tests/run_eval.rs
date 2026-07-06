use super::*;

// ---------------------------------------------------------------------------
// run.rs — report aggregation, empty/no-scip path, eval metric ratios.
// ---------------------------------------------------------------------------

/// A run with no `.scip` documents (empty index) examines its candidates but writes no verdicts and
/// returns cleanly with `status = "Completed"` — the no-data path is not an error.
#[test]
fn run_with_empty_scip_completes_with_no_verdicts() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    let empty = Index::default().write_to_bytes().unwrap();
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &empty, h.root(), None, None).unwrap();

    assert_eq!(report.edges_examined, 1);
    assert_eq!(report.no_occurrence, 1, "no document → no occurrence bucket");
    assert_eq!(report.rows_written, 0);
    assert_eq!(report.oracle_only_calls, 0);
    assert_eq!(report.status, "Completed");
    assert!(h.verdict(edge).is_none());
    // The run is still recorded.
    let runs: i64 = h.conn.query_row("SELECT COUNT(*) FROM oracle_runs", [], |r| r.get(0)).unwrap();
    assert_eq!(runs, 1);
}

/// A candidate whose callee byte range falls outside every occurrence lands in the `no_occurrence`
/// bucket with no verdict written, even though the document has occurrences.
#[test]
fn candidate_outside_any_occurrence_counts_no_occurrence() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    // Callee range 14..20 in source, but the only occurrence covers bytes 0..5 (the `fn ca`
    // prefix).
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(
            0,
            0,
            5,
            "scip-rust crate v1 `other`().",
            SymbolRole::UnspecifiedSymbolRole as i32,
        ),
    ]);

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.edges_examined, 1);
    assert_eq!(report.no_occurrence, 1);
    assert_eq!(report.rows_written, 0);
    assert!(h.verdict(edge).is_none());
}

/// A run aggregates per-kind counts across multiple edges and computes the recall gap: an in-corpus
/// reference occurrence no edge covered increments `oracle_only_calls`.
#[test]
fn run_aggregates_counts_and_recall_gap() {
    let h = Harness::new();
    // Two call sites of `target` in caller.rs; only one is emitted as an edge → recall gap of 1.
    let caller = h.add_file("caller.rs", "fn caller() { target(); target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    // Edge covers the FIRST call site (bytes 14..20). Second call site (24..30) has no edge.
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    let sym = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![
                occurrence(0, 14, 20, sym, SymbolRole::UnspecifiedSymbolRole as i32),
                occurrence(0, 24, 30, sym, SymbolRole::UnspecifiedSymbolRole as i32),
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
        occurrences: vec![occurrence(0, 3, 9, sym, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.edges_examined, 1);
    assert_eq!(report.upgraded, 1);
    assert_eq!(report.rows_written, 1);
    assert_eq!(report.oracle_only_calls, 1, "the uncovered second call site is the recall gap");
    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::Upgrade.as_db_str());
    assert_eq!(resolved, Some(target_sym));
}

/// A rerun is AUTHORITATIVE for its `(tool, tool_version)`: when the new `.scip` no longer yields a
/// verdict for an edge the prior run covered, that edge's stale verdict must be GONE after the
/// rerun (not left behind by the per-edge upsert). The run clears the scope before writing.
#[test]
fn rerun_clears_stale_verdict_for_dropped_edge() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    // First run: a `.scip` that covers the edge → an Upgrade verdict is written.
    let sym = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![occurrence(0, 14, 20, sym, SymbolRole::UnspecifiedSymbolRole as i32)],
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
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(
        h.verdict(edge).map(|(k, _, _)| k).as_deref(),
        Some("upgrade"),
        "first run wrote it"
    );
    assert_eq!(target_sym, target_sym); // (binds target_sym so the def mapping is exercised)

    // Rerun with a `.scip` that has NO occurrence for that edge (the document lost the call site).
    let empty_doc =
        scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
            occurrence(
                0,
                0,
                5,
                "scip-rust crate v1 `other`().",
                SymbolRole::UnspecifiedSymbolRole as i32,
            ),
        ]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &empty_doc, h.root(), None, None).unwrap();

    assert!(h.verdict(edge).is_none(), "the dropped edge's stale verdict was cleared on rerun");
    let total: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 0, "rerun left no stale rows for this (tool, version)");
}

/// The recall gap counts only CALL-LIKE occurrences: an `Import`-role reference and a non-callable
/// (`Type`-descriptor) reference are excluded even though they have in-corpus definitions, while an
/// uncovered callable reference IS counted. This keeps imports / type refs from falsely lowering
/// recall (`oracle_only_calls`).
#[test]
fn recall_gap_counts_only_call_like_occurrences() {
    let h = Harness::new();
    // One source file with three uncovered references (no edges emitted for any of them):
    //   - a callable (`Method` suffix) → counts toward the recall gap,
    //   - an import of that same callable (Import role) → excluded,
    //   - a type reference (`Type` suffix) → excluded.
    let src = h.add_file("src.rs", "fn caller() {}\nstruct Thing;\n");
    let _ = h.add_edge(src, "noop", 0, 1, "NameOnly", None); // unrelated edge, distinct occurrence
    // The callable's SCIP definition (line 1, bytes 15..21) must map to one of OUR indexed symbols,
    // otherwise it is (correctly) excluded as not-in-rag-rat's-set. Seed a symbol spanning it.
    h.add_symbol(src, "target", 15, 21);

    let callable = "scip-rust crate v1 `target`().";
    let ty = "scip-rust crate v1 `Thing`#"; // `#` is the Type descriptor suffix in SCIP symbol text
    let index = Index {
        documents: vec![Document {
            relative_path: "src.rs".to_string(),
            occurrences: vec![
                // Uncovered callable reference + its definition (in-corpus) → 1 recall-gap call.
                occurrence(0, 5, 11, callable, SymbolRole::UnspecifiedSymbolRole as i32),
                occurrence(1, 0, 6, callable, SymbolRole::Definition as i32),
                // An IMPORT of the callable → excluded (Import role).
                occurrence(0, 12, 13, callable, SymbolRole::Import as i32),
                // A type reference + its definition → excluded (not callable).
                occurrence(1, 7, 12, ty, SymbolRole::UnspecifiedSymbolRole as i32),
                occurrence(1, 7, 12, ty, SymbolRole::Definition as i32),
            ],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    let bytes = index.write_to_bytes().unwrap();
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    // Only the one uncovered callable reference is the recall gap; the import + type ref are not.
    assert_eq!(report.oracle_only_calls, 1, "only the call-like uncovered reference counts");
}

/// `oracle_eval_metrics` derives precision / recall / recovery rates from the persisted verdicts.
/// One confirm + one contradict → precision 0.5; one upgrade among the low-confidence edges →
/// recovery 1.0.
#[test]
fn eval_metrics_derive_rates_from_persisted_verdicts() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() {}\n");
    let sha = h.file_sha("a.rs");
    // Exact edges judged: one confirmed, one contradicted. (to_symbol_id NULL keeps the FK happy;
    // precision is derived from the persisted edge_oracle rows, not the edges row.)
    let e_conf = h.add_edge(f, "c", 0, 1, "Exact", None);
    let e_contra = h.add_edge(f, "d", 1, 2, "Exact", None);
    // A NameOnly edge the oracle upgraded.
    let e_up = h.add_edge(f, "u", 2, 3, "NameOnly", None);

    h.write_verdict(e_conf, &sha, Some(1), "s", OracleResolutionKind::Confirm);
    h.write_verdict(e_contra, &sha, Some(3), "s", OracleResolutionKind::Contradict);
    h.write_verdict(e_up, &sha, Some(4), "s", OracleResolutionKind::Upgrade);

    // Recall sides come from the run, occurrence-counted over the call population: 3 covered call
    // occurrences + 1 oracle-only gap. (Recall no longer derives from the per-kind verdict sum.)
    let m = super::oracle_eval_metrics(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, RecallCalls {
        covered: 3,
        oracle_only: 1,
    })
    .unwrap();
    assert_eq!(m.confirmed, 1);
    assert_eq!(m.contradicted, 1);
    assert_eq!(m.upgraded, 1);
    assert_eq!(m.oracle_only_calls, 1);
    assert_eq!(m.covered_calls, 3);
    // precision = confirm / (confirm + contradict) = 1/2.
    assert!((m.precision - 0.5).abs() < 1e-9);
    // recovery = upgrades / low-confidence-edges-with-oracle = 1/1.
    assert!((m.name_only_recovery_rate - 1.0).abs() < 1e-9);
    // recall = covered_calls / (covered_calls + oracle_only) = 3/4.
    assert!((m.recall - 0.75).abs() < 1e-9, "recall was {}", m.recall);
    // oracle_upgradeable_fraction = (upgrade + external) / unresolved candidates.
    // Only `e_up` is a NameOnly edge carrying a callee range → denominator 1, numerator 1.
    assert!((m.oracle_upgradeable_fraction - 1.0).abs() < 1e-9);
}

/// `oracle_upgradeable_fraction` stays bounded by 1.0 even when the oracle resolved an
/// already-Exact edge to an external dependency. Numerator and denominator must both range over the
/// low-confidence (`NameOnly`/`Ambiguous`) population: a `resolved-external` verdict on an Exact
/// edge is NOT in the denominator, so counting it in the numerator (the old bug) let the fraction
/// exceed 1.0.
#[test]
fn oracle_upgradeable_fraction_is_bounded_by_one() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() {}\n");
    let sha = h.file_sha("a.rs");
    // The only low-confidence candidate (denominator = 1): a NameOnly edge the oracle upgraded.
    let e_low = h.add_edge(f, "u", 0, 1, "NameOnly", None);
    // An already-Exact edge the oracle placed to an EXTERNAL dep — NOT in the low-conf denominator.
    let e_exact = h.add_edge(f, "x", 1, 2, "Exact", None);

    h.write_verdict(e_low, &sha, None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(e_exact, &sha, None, "s", OracleResolutionKind::ResolvedExternal);

    let m = super::oracle_eval_metrics(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        RecallCalls::default(),
    )
    .unwrap();
    // Old numerator (upgraded 1 + resolved_external 1 = 2) over denominator 1 would be 2.0.
    // Scoped numerator counts only the low-conf upgrade → 1/1 = 1.0.
    assert!(
        m.oracle_upgradeable_fraction <= 1.0,
        "fraction {} exceeds 1.0",
        m.oracle_upgradeable_fraction
    );
    assert!((m.oracle_upgradeable_fraction - 1.0).abs() < 1e-9);
    // The raw counts still report both verdicts for transparency.
    assert_eq!(m.upgraded, 1);
    assert_eq!(m.resolved_external, 1);
}

/// Vacuous denominators yield 1.0 across the board (nothing to get wrong) — the documented
/// hit-rate convention.
#[test]
fn eval_metrics_are_vacuously_perfect_with_no_verdicts() {
    let h = Harness::new();
    let m = super::oracle_eval_metrics(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        RecallCalls::default(),
    )
    .unwrap();
    assert_eq!(m.precision, 1.0);
    assert_eq!(m.recall, 1.0);
    assert_eq!(m.name_only_recovery_rate, 1.0);
    assert_eq!(m.oracle_upgradeable_fraction, 1.0);
}
