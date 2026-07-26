//! End-to-end integration tests for the phase-2 (#69) read-side surfacing through the public
//! `IndexDatabase` API: build a real temp Rust checkout with an intra-file call, rebuild, run
//! the oracle over a programmatically-built `.scip` (the deterministic `--scip` consumption
//! path — no rust-analyzer), and assert the `Compiler` tier / `resolved-external` /
//! `compare_graph_to_scip` / gc behaviours. Models `eval::tests::eval_suite_runs_oracle_*`.

use std::path::PathBuf;

use ::protobuf::{EnumOrUnknown, Message};
use ::scip::types::{Document, Index, Occurrence, PositionEncoding, SymbolRole};
use rag_rat_base::config::ResolvedTarget;
use rag_rat_oracle::OracleTool;

use super::*;

fn temp_root() -> rag_rat_base::test_scratch::ScratchDir {
    let root = rag_rat_base::test_scratch::ScratchDir::new("q-oracle");
    fs::create_dir_all(root.join("src")).unwrap();
    root
}

fn rust_config(root: PathBuf) -> Config {
    Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.to_path_buf(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["src/".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    }
}

/// The callee identifier byte range of the (single) `calls_name` edge, read back from the DB so
/// the `.scip` occurrence aligns exactly with what the indexer recorded.
fn call_edge(db: &IndexDatabase) -> (i64, i64, i64, String) {
    db.storage
        .connection()
        .query_row(
            "SELECT edges.id, edges.callee_start_byte, edges.callee_end_byte, files.path
                 FROM edges JOIN files ON files.id = edges.source_file_id
                 WHERE edges.edge_kind = 'calls_name' AND edges.callee_start_byte IS NOT NULL
                 LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap()
}

/// Build a `.scip` over a single-line source file: a reference occurrence at the callee byte
/// range (single-line → char == byte for ASCII) plus a definition occurrence for the same
/// symbol at `def_range`. `def_path` is where the definition lives (in-corpus → Upgrade;
/// elsewhere/external symbol → resolved-external).
fn scip_with(
    path: &str,
    callee_start: i64,
    callee_end: i64,
    symbol: &str,
    def_path: Option<&str>,
    def_range: Option<(i64, i64)>,
) -> Vec<u8> {
    let occ = |start: i64, end: i64, role: SymbolRole| Occurrence {
        range: vec![0, start as i32, end as i32],
        symbol: symbol.to_string(),
        symbol_roles: role as i32,
        ..Default::default()
    };
    let mut documents = vec![Document {
        relative_path: path.to_string(),
        occurrences: vec![occ(callee_start, callee_end, SymbolRole::UnspecifiedSymbolRole)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    }];
    if let (Some(def_path), Some((ds, de))) = (def_path, def_range) {
        // A def in the SAME file must be appended to that document's occurrence list, not
        // pushed as a SECOND document with the same `relative_path` —
        // `ScipIndex::from_index` keys `occurrences_by_path` by path, so a
        // duplicate-path document overwrites the ref.
        if def_path == path {
            documents[0].occurrences.push(occ(ds, de, SymbolRole::Definition));
        } else {
            documents.push(Document {
                relative_path: def_path.to_string(),
                occurrences: vec![occ(ds, de, SymbolRole::Definition)],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            });
        }
    }
    Index { documents, ..Default::default() }.write_to_bytes().unwrap()
}

/// The full path: rebuild a checkout where `caller` calls `target`, run the oracle from a
/// pre-built `.scip` that resolves the call in-corpus, and assert `find_callers` /
/// `trace_callees` surface the `compiler` tier with the `scip:<tool>@<version>` reason — while
/// the heuristic `edges` row is untouched. Also asserts `compare_graph_to_scip` reports no
/// contradiction (an Upgrade is agreement-shaped, not a disagreement).
#[test]
fn oracle_run_from_scip_surfaces_compiler_tier() {
    let root = temp_root();
    // Single line so byte offsets == char offsets (ASCII): `target` is the callee.
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (edge_id, callee_start, callee_end, path) = call_edge(&db);
    // `target` definition: `fn target() {}` → identifier `target` at bytes 29..35.
    let symbol = "scip-rust crate held-mini `target`().";
    let scip = scip_with(&path, callee_start, callee_end, symbol, Some(&path), Some((29, 35)));

    let report = db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();
    // `target` is defined in-corpus, so the heuristic resolves it; the oracle CONFIRMS it. Both
    // Confirm and Upgrade surface the `compiler` tier.
    assert!(
        report.confirmed >= 1 || report.upgraded >= 1,
        "expected in-corpus confirm/upgrade, got {report:?}"
    );

    // find_callers (reverse) surfaces the compiler tier on the matching edge.
    let callers = db
        .find_callers_with_options("target", 50, &rag_rat_query::graph::GraphTraversalOptions {
            include_unresolved: true,
            ..Default::default()
        })
        .unwrap();
    let hop = callers.iter().find(|h| h.edge_id == edge_id).expect("call edge present");
    assert_eq!(hop.confidence, "compiler", "expected Compiler tier surfaced");
    assert_eq!(hop.resolution_reason.as_deref(), Some("scip:rust-analyzer@v-test"));
    // The heuristic edge confidence is preserved (not overwritten).
    assert_ne!(hop.edge_confidence, "compiler");

    // The heuristic `edges` row was never mutated by the oracle pass.
    let edge_confidence: String = db
        .storage
        .connection()
        .query_row("SELECT confidence FROM edges WHERE id = ?1", params![edge_id], |r| r.get(0))
        .unwrap();
    assert_ne!(edge_confidence, "compiler");

    // An Upgrade is agreement-shaped → compare_graph_to_scip reports no contradiction.
    let compare = db.compare_graph_to_scip().unwrap();
    assert!(!compare.summary.no_oracle_data);
    assert_eq!(compare.summary.contradictions, 0);
}

/// `oracle_pre_spawn_snapshot` returns the active checkout's indexed `(path -> sha256)` map;
/// a run pinned to a MATCHING snapshot verdicts normally, while a snapshot disagreeing with
/// the join-time indexed sha (the mid-subprocess reindex, #83) skips the candidate.
#[test]
fn pre_spawn_snapshot_round_trips_through_run_oracle() {
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (_edge_id, cs, ce, path) = call_edge(&db);
    let snapshot = db.oracle_pre_spawn_snapshot().unwrap();
    let indexed_sha: String = db
        .storage
        .connection()
        .query_row("SELECT sha256 FROM files WHERE path = ?1", params![path], |r| r.get(0))
        .unwrap();
    assert_eq!(
        snapshot.get(&path).map(String::as_str),
        Some(indexed_sha.as_str()),
        "snapshot must carry the indexed sha for every active-checkout file"
    );

    let symbol = "scip-rust crate held-mini `target`().";
    let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((29, 35)));
    let report = db
        .run_oracle(OracleTool::RustAnalyzer, "v-test", &scip, OracleShaSnapshots {
            production: None,
            pre_spawn: Some(&snapshot),
        })
        .unwrap();
    assert!(report.confirmed >= 1 || report.upgraded >= 1, "matching pin verdicts normally");
    assert_eq!(report.skipped_drifted, 0);

    let mut stale = snapshot.clone();
    stale.insert(path.clone(), "pre-spawn-old".to_string());
    let report = db
        .run_oracle(OracleTool::RustAnalyzer, "v-test2", &scip, OracleShaSnapshots {
            production: None,
            pre_spawn: Some(&stale),
        })
        .unwrap();
    assert_eq!(report.rows_written, 0, "a mid-subprocess reindex must skip the candidate");
    assert!(report.skipped_drifted >= 1);
}

/// Staleness revert: after a current run surfaces `compiler`, drifting the source file (so its
/// `files.sha256` no longer matches the verdict's `file_sha`) reverts the edge to heuristic
/// display — never `compiler`.
#[test]
fn drifted_file_reverts_to_heuristic_display() {
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (edge_id, cs, ce, path) = call_edge(&db);
    let symbol = "scip-rust crate held-mini `target`().";
    let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((29, 35)));
    db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();

    // Drift the recorded sha so the verdict's file_sha no longer matches (file changed). The
    // active connection exposes `files` as a scoped TEMP VIEW (read-only), so the UPDATE must
    // target the underlying `main.files` table.
    db.storage
        .connection()
        .execute("UPDATE main.files SET sha256 = 'drifted' WHERE path = ?1", params![path])
        .unwrap();

    let callers = db
        .find_callers_with_options("target", 50, &rag_rat_query::graph::GraphTraversalOptions {
            include_unresolved: true,
            ..Default::default()
        })
        .unwrap();
    let hop = callers.iter().find(|h| h.edge_id == edge_id).expect("call edge present");
    assert_ne!(hop.confidence, "compiler", "drifted file must revert to heuristic display");
    assert!(hop.resolution_reason.is_none());
}

/// `resolved-external`: a `.scip` resolving the callee to a packaged dependency symbol with no
/// in-corpus definition surfaces `resolved_external = resolved-external(<package>)` on the hop,
/// and feeds the quantitative completeness clause in the graph report.
#[test]
fn external_resolution_surfaces_resolved_external_label() {
    let root = temp_root();
    // `external_fn` is NOT defined in-corpus → the heuristic can't resolve it (NameOnly /
    // unresolved), so SCIP's external resolution is a clean `resolved-external`, not a
    // contradiction of an in-corpus claim.
    fs::write(root.join("src/lib.rs"), "fn caller() { external_fn(); }\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (edge_id, cs, ce, path) = call_edge(&db);
    // A packaged SCIP symbol with NO in-corpus definition occurrence →
    // resolved-external(tokio).
    let symbol = "scip-rust cargo tokio 1.0 `external_fn`().";
    let scip = scip_with(&path, cs, ce, symbol, None, None);
    let report = db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();
    assert!(report.resolved_external >= 1, "expected resolved-external, got {report:?}");

    let callees = db
        .trace_callees_with_options("caller", 50, &rag_rat_query::graph::GraphTraversalOptions {
            include_unresolved: true,
            ..Default::default()
        })
        .unwrap();
    let hop = callees.iter().find(|h| h.edge_id == edge_id).expect("call edge present");
    assert_eq!(hop.resolved_external.as_deref(), Some("resolved-external(tokio)"));
    // External placement is not an in-corpus upgrade → confidence stays heuristic.
    assert_ne!(hop.confidence, "compiler");
}

/// `compare_graph_to_scip` reports a contradiction when the heuristic resolved an edge
/// in-corpus but the compiler resolves the callee to a DIFFERENT (external) target.
#[test]
fn compare_graph_to_scip_reports_contradiction() {
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (edge_id, cs, ce, path) = call_edge(&db);
    // Force the heuristic edge to look in-corpus-resolved (Exact + to_symbol_id), so the
    // compiler's external resolution is a contradiction, not a plain resolved-external.
    let target_sym: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM symbols WHERE name = 'target' LIMIT 1", [], |r| r.get(0))
        .unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE edges SET confidence = 'Exact', resolution = 'exact', to_symbol_id = ?2 WHERE \
             id = ?1",
            params![edge_id, target_sym],
        )
        .unwrap();

    // The compiler says the callee is actually `other::target` in a dependency → contradiction.
    let symbol = "scip-rust cargo other 1.0 `target`().";
    let scip = scip_with(&path, cs, ce, symbol, None, None);
    db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();

    let compare = db.compare_graph_to_scip().unwrap();
    assert_eq!(compare.summary.contradictions, 1, "{compare:?}");
    let c = &compare.contradictions[0];
    assert_eq!(c.edge_id, edge_id);
    assert_eq!(c.heuristic_confidence, "exact");
    assert_eq!(c.resolved_external.as_deref(), Some("resolved-external(other)"));
}

/// #176: surfacing must NOT be hardcoded to rust-analyzer. A verdict written under another backend
/// (here scip-clang) must still be reported by `compare_graph_to_scip`, and the report's `tool`
/// must name the contributing backend — proving the multi-tool `latest_runs_in_scope` seam, not the
/// old single-`RustAnalyzer` query.
#[test]
fn compare_graph_to_scip_surfaces_non_rust_analyzer_tools() {
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (edge_id, cs, ce, path) = call_edge(&db);
    let target_sym: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM symbols WHERE name = 'target' LIMIT 1", [], |r| r.get(0))
        .unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE edges SET confidence = 'Exact', resolution = 'exact', to_symbol_id = ?2 WHERE \
             id = ?1",
            params![edge_id, target_sym],
        )
        .unwrap();

    let symbol = "scip-rust cargo other 1.0 `target`().";
    let scip = scip_with(&path, cs, ce, symbol, None, None);
    // Write the verdict under scip-clang, NOT rust-analyzer.
    db.run_oracle_from_scip(OracleTool::ScipClang, "clang-vtest", &scip).unwrap();

    let compare = db.compare_graph_to_scip().unwrap();
    assert!(!compare.summary.no_oracle_data, "a scip-clang run exists: {compare:?}");
    assert_eq!(compare.summary.contradictions, 1, "scip-clang verdict must surface: {compare:?}");
    assert_eq!(compare.contradictions[0].edge_id, edge_id);
    assert!(
        compare.query.tool.contains("scip-clang"),
        "the report must name the contributing backend, got `{}`",
        compare.query.tool
    );
}

/// #82 P0: when a run EXISTS but examined 0 in-scope verdicts, `compare_graph_to_scip` must WARN
/// — that is "run-but-empty" (the silent symptom of the scope bug), not "compiler agrees". Here
/// a run writes a verdict, then the callsite file drifts so the current-content gate
/// filters every verdict out → `verdicts_examined == 0` despite the run row existing.
#[test]
fn compare_warns_when_run_exists_but_no_verdicts_in_scope() {
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (_edge_id, cs, ce, path) = call_edge(&db);
    let symbol = "scip-rust crate held-mini `target`().";
    let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((29, 35)));
    db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();

    // Drift the callsite file so every verdict's `file_sha` gate fails → 0 in-scope verdicts,
    // even though the `oracle_runs` row still exists.
    db.storage
        .connection()
        .execute("UPDATE main.files SET sha256 = 'drifted' WHERE path = ?1", params![path])
        .unwrap();

    let compare = db.compare_graph_to_scip().unwrap();
    assert!(!compare.summary.no_oracle_data, "a run DOES exist for this checkout");
    assert_eq!(compare.summary.verdicts_examined, 0, "all verdicts filtered by the drift gate");
    assert!(
        compare.summary.warnings.iter().any(|w| w.contains("0 in-scope verdicts")),
        "run-but-empty must warn, not silently read as compiler-agrees: {:?}",
        compare.summary.warnings
    );
}

/// #82 finding 1: an IN-CORPUS contradiction (the compiler resolved the callee to a DIFFERENT
/// in-corpus symbol than the heuristic) must NOT be labeled `resolved-external`. A Rust SCIP
/// symbol carries a crate/package component even for the LOCAL crate, so deriving the label
/// from `scip_symbol` alone would mislabel it as `resolved-external(held-mini)`.
#[test]
fn in_corpus_contradiction_is_not_labeled_resolved_external() {
    let root = temp_root();
    // Two in-corpus defs. The heuristic resolves `target()` to `target`; the compiler resolves
    // the same callsite to the OTHER in-corpus def `other` → in-corpus Contradict.
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {} fn other() {}\n")
        .unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (edge_id, cs, ce, path) = call_edge(&db);
    // Force the heuristic edge to look in-corpus-resolved to `target` (Exact + to_symbol_id).
    let target_sym: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM symbols WHERE name = 'target' LIMIT 1", [], |r| r.get(0))
        .unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE edges SET confidence = 'Exact', resolution = 'exact', to_symbol_id = ?2 WHERE \
             id = ?1",
            params![edge_id, target_sym],
        )
        .unwrap();
    // The compiler resolves the callee to the in-corpus def `other` (a LOCAL-crate SCIP
    // symbol), whose definition occurrence sits at `other`'s recorded byte span.
    let (other_start, other_end): (i64, i64) = db
        .storage
        .connection()
        .query_row(
            "SELECT start_byte, end_byte FROM symbols WHERE name = 'other' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let symbol = "scip-rust crate held-mini `other`().";
    let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((other_start, other_end)));
    db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();

    let compare = db.compare_graph_to_scip().unwrap();
    assert_eq!(compare.summary.contradictions, 1, "{compare:?}");
    let c = &compare.contradictions[0];
    assert_eq!(c.edge_id, edge_id);
    // The disagreement is in-corpus → no external label, even though the SCIP symbol names the
    // local crate.
    assert_eq!(
        c.resolved_external, None,
        "an in-corpus contradiction must not be labeled resolved-external"
    );
}

/// End-to-end #108 SCIP-aware ranking: an in-corpus Contradict RETARGETS importance to the
/// compiler's true callee. The heuristic resolves `caller -> target`; the compiler contradicts
/// it, resolving the callee to the in-corpus `other`. Before the run, `target` carries the
/// call's rank and `other` none; after, the rank flows to `other` (the compiler's answer),
/// not the heuristic guess. Proves the wrapper gates on a run existing, maps in-corpus
/// Contradict to a retarget, and applies it through to the ranker.
#[test]
fn important_symbols_retargets_an_in_corpus_contradiction() {
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {} fn other() {}\n")
        .unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    let (edge_id, cs, ce, path) = call_edge(&db);
    let name_of = |id: i64| -> String {
        conn.query_row(
            "SELECT qn.value FROM symbols
             LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
             WHERE symbols.id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    };
    let target_sym: i64 = conn
        .query_row("SELECT id FROM symbols WHERE name = 'target' LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let target_qn = name_of(target_sym);
    // Force the heuristic edge to look exactly-resolved to `target`.
    conn.execute(
        "UPDATE edges SET confidence = 'Exact', resolution = 'exact', to_symbol_id = ?2 WHERE id \
         = ?1",
        params![edge_id, target_sym],
    )
    .unwrap();

    let score_of = |out: &[rag_rat_query::pagerank::SymbolImportance], qn: &str| {
        out.iter().find(|s| s.qualified_name == qn).map_or(0.0, |s| s.score)
    };

    // The compiler contradicts the heuristic, resolving the callee to the other in-corpus def.
    let (other_id, other_start, other_end): (i64, i64, i64) = conn
        .query_row(
            "SELECT id, start_byte, end_byte FROM symbols WHERE name = 'other' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let other_qn = name_of(other_id);

    // Heuristic ranking (no oracle run yet): `target` carries the call's rank, `other` none.
    let before = global_ranking(&db);
    assert!(score_of(&before, &target_qn) > 0.0, "heuristically `target` carries rank: {before:?}");
    assert_eq!(score_of(&before, &other_qn), 0.0, "`other` is uncalled heuristically: {before:?}");

    let symbol = "scip-rust crate held-mini `other`().";
    let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((other_start, other_end)));
    db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();

    // SCIP-aware ranking: the edge is retargeted to the compiler's `other` — rank flows there,
    // and the contradicted heuristic target `target` loses it.
    let after = global_ranking(&db);
    assert!(
        score_of(&after, &other_qn) > score_of(&after, &target_qn),
        "rank flows to the compiler's resolved callee, not the heuristic guess: {after:?}"
    );
    assert!(
        score_of(&after, &other_qn) > score_of(&before, &other_qn),
        "the retargeted callee gains rank it did not have heuristically: {after:?}"
    );
}

/// #82 finding 2: an `Upgrade` on a heuristic-unresolved edge must surface the SCIP-RESOLVED
/// symbol as the hop's target, not the heuristic's missing/heuristic one. We strip the
/// heuristic resolution (NameOnly, no `to_symbol_id`) and let the compiler resolve in-corpus,
/// then read FORWARD from `caller` (robust for an unresolved edge) and assert the hop's target
/// moved to the compiler-resolved symbol.
#[test]
fn upgrade_hydrates_target_from_compiler_resolution() {
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (edge_id, cs, ce, path) = call_edge(&db);
    let target_qualified: String = db
        .storage
        .connection()
        .query_row(
            "SELECT qn.value FROM symbols
             LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
             WHERE symbols.name = 'target' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let (target_start, target_end): (i64, i64) = db
        .storage
        .connection()
        .query_row(
            "SELECT start_byte, end_byte FROM symbols WHERE name = 'target' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    // Demote the heuristic edge to a genuine miss: NameOnly, no resolved target / qualified
    // name. A promotion that didn't MOVE the target would surface this absent one.
    db.storage
        .connection()
        .execute(
            "UPDATE edges SET confidence = 'NameOnly', resolution = 'name_only', to_symbol_id = \
             NULL, target_qualified_name = NULL WHERE id = ?1",
            params![edge_id],
        )
        .unwrap();
    let symbol = "scip-rust crate held-mini `target`().";
    let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((target_start, target_end)));
    let report = db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();
    assert!(report.upgraded >= 1, "expected an Upgrade, got {report:?}");

    let callees = db
        .trace_callees_with_options("caller", 50, &rag_rat_query::graph::GraphTraversalOptions {
            include_unresolved: true,
            ..Default::default()
        })
        .unwrap();
    let hop = callees.iter().find(|h| h.edge_id == edge_id).expect("call edge present");
    assert_eq!(hop.confidence, "compiler", "an Upgrade surfaces the compiler tier");
    // The target moved to the SCIP-resolved symbol, not the heuristic's absent target.
    assert_eq!(hop.to_symbol.as_deref(), Some(target_qualified.as_str()));
    assert_eq!(hop.target_qualified_name.as_deref(), Some(target_qualified.as_str()));
    assert!(hop.verified_target_symbol, "the compiler-resolved target is verified");
}

/// #82 finding 4: a compiler-`Upgrade`d low-confidence neighbor must appear within `limit` even
/// when more `Exact` neighbors than `limit` outrank it heuristically. Oracle enrichment runs
/// AFTER the heuristic-ordered limit, so without overfetch+re-rank the upgraded edge is
/// dropped.
#[test]
fn compiler_upgrade_survives_heuristic_limit() {
    let root = temp_root();
    // Single line so byte offsets == char offsets (ASCII) for the `.scip` occurrence. `pull` is
    // the upgrade target; two of its three callers are Exact (high heuristic rank), the third
    // is a name-only miss the compiler upgrades.
    fs::write(
        root.join("src/lib.rs"),
        "fn pull() {} fn a() { pull(); } fn b() { pull(); } fn c() { pull(); }\n",
    )
    .unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Find the three call edges to `pull`. Make two of them Exact (high heuristic rank) and the
    // third a NameOnly miss the compiler will upgrade.
    let edges: Vec<(i64, i64, i64, String)> = {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT edges.id, edges.callee_start_byte, edges.callee_end_byte, files.path FROM \
                 edges JOIN files ON files.id = edges.source_file_id WHERE edges.edge_kind = \
                 'calls_name' AND edges.callee_start_byte IS NOT NULL ORDER BY edges.id",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?)))
            .unwrap();
        rows.map(Result::unwrap).collect()
    };
    assert_eq!(edges.len(), 3, "three calls to pull");
    let pull_sym: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM symbols WHERE name = 'pull' LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let (pull_start, pull_end): (i64, i64) = db
        .storage
        .connection()
        .query_row(
            "SELECT start_byte, end_byte FROM symbols WHERE name = 'pull' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    // Edges 0,1: Exact resolved to `pull`. Edge 2: NameOnly miss (the upgrade candidate).
    let conn = db.storage.connection();
    for (edge_id, ..) in &edges[..2] {
        conn.execute(
            "UPDATE edges SET confidence = 'Exact', resolution = 'exact', to_symbol_id = ?2 WHERE \
             id = ?1",
            params![edge_id, pull_sym],
        )
        .unwrap();
    }
    let (upgrade_edge, ucs, uce, path) = edges[2].clone();
    // NameOnly (heuristically low rank, below the Exact callers) but still target-resolved to
    // `pull` so the reverse traversal includes it as a candidate. The oracle still classifies
    // it an Upgrade (the heuristic didn't resolve it Exact/Syntactic-in-corpus), and
    // the re-rank must lift it above the Exact callers within the limit.
    conn.execute(
        "UPDATE edges SET confidence = 'NameOnly', resolution = 'name_only', to_symbol_id = ?2 \
         WHERE id = ?1",
        params![upgrade_edge, pull_sym],
    )
    .unwrap();

    // The compiler upgrades ONLY the name-only edge to the in-corpus `pull` def.
    let symbol = "scip-rust crate held-mini `pull`().";
    let scip = scip_with(&path, ucs, uce, symbol, Some(&path), Some((pull_start, pull_end)));
    let report = db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();
    assert!(report.upgraded >= 1, "expected an Upgrade, got {report:?}");

    // limit = 1: heuristically the two Exact callers outrank the name-only one, so without the
    // overfetch+re-rank the compiler upgrade is dropped. With it, the compiler tier wins.
    let callers = db
        .find_callers_with_options("pull", 1, &rag_rat_query::graph::GraphTraversalOptions {
            include_unresolved: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(callers.len(), 1, "limit honored");
    assert_eq!(
        callers[0].edge_id, upgrade_edge,
        "the compiler-upgraded neighbor must rank into the limit ahead of Exact neighbors"
    );
    assert_eq!(callers[0].confidence, "compiler");
}

/// gc prunes `oracle_runs` for dead checkout contexts: an oracle run recorded under a sibling
/// `(commit, worktree)` is dropped by `prune_to_live` when that context is not live.
#[test]
fn gc_prunes_oracle_runs_for_dead_contexts() {
    // Asserts whole-DB `oracle_runs` counts; opt out of the poison harness whose sibling seeds a
    // run under its own repo_id.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Record a run for THIS (active) checkout + a run for a dead sibling context.
    rag_rat_oracle::run_oracle(
        db.storage.connection(),
        OracleTool::RustAnalyzer,
        "v-test",
        &db.active_commit_sha,
        &db.active_worktree_id,
        &Index::default().write_to_bytes().unwrap(),
        &root,
        None,
        None,
    )
    .unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO oracle_runs(tool, tool_version, commit_sha, worktree_id, started_at, \
             status, stats_json) VALUES ('rust-analyzer', 'v-test', 'dead-commit', \
             'dead-worktree', 0, 'Completed', '{}')",
            [],
        )
        .unwrap();
    let before: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM oracle_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, 2);

    // gc keeps the active context and prunes the dead one.
    db.prune_to_live(
        std::slice::from_ref(&db.active_commit_sha),
        std::slice::from_ref(&db.active_worktree_id),
    )
    .unwrap();

    let remaining: Vec<String> = {
        let conn = db.storage.connection();
        let mut stmt = conn.prepare("SELECT commit_sha FROM oracle_runs").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert!(
        !remaining.iter().any(|c| c == "dead-commit"),
        "dead-context oracle_runs row must be pruned: {remaining:?}"
    );
}

#[test]
fn resolved_external_label_extracts_package() {
    assert_eq!(
        resolved_external_label("scip-rust cargo tokio 1.0 `spawn`()."),
        Some("resolved-external(tokio)".to_string())
    );
    // A symbol with no package component yields no label.
    assert_eq!(resolved_external_label("local 0"), None);
}

#[test]
fn completeness_annotation_counts_externals_and_lists_packages() {
    let mut summary = rag_rat_query::graph::GraphTraversalSummary {
        unresolved: 5,
        completeness_risk: "medium".to_string(),
        ..Default::default()
    };
    let hop = |external: Option<&str>| rag_rat_query::graph::GraphHop {
        edge_id: 0,
        from_symbol: None,
        to_symbol: None,
        edge_kind: "calls_name".to_string(),
        confidence: "name_only".to_string(),
        edge_confidence: "name_only".to_string(),
        target: None,
        target_qualified_name: None,
        evidence: None,
        receiver_hint: None,
        resolution: "unresolved".to_string(),
        resolution_reason: None,
        resolved_external: external.map(str::to_string),
        verified_target_symbol: false,
        shown_by_default: true,
        callsite: None,
        importance: None,
    };
    let hops = vec![
        hop(Some("resolved-external(tokio)")),
        hop(Some("resolved-external(libc)")),
        hop(None),
    ];
    annotate_completeness_with_externals(&mut summary, &hops);
    // The count is over the SHOWN window (2 of the passed hops), not divided by the
    // population-wide unresolved gap (#82 P3) — the clause speaks of "shown neighbors".
    assert_eq!(
        summary.completeness_risk,
        "medium (2 shown neighbors are resolved-external: libc, tokio)"
    );

    // No externals → the qualitative string is left untouched.
    let mut bare = rag_rat_query::graph::GraphTraversalSummary {
        unresolved: 3,
        completeness_risk: "high".to_string(),
        ..Default::default()
    };
    annotate_completeness_with_externals(&mut bare, &[hop(None)]);
    assert_eq!(bare.completeness_risk, "high");
}

/// `run_oracle_with_tool` degrades to `Blocked` (never an error) when the indexer isn't
/// installed — the missing-embedding-model UX. Skipped when rust-analyzer happens to be on PATH
/// (then the subprocess path runs, which this test doesn't assert).
#[test]
fn oracle_run_without_tool_is_blocked_not_error() {
    if std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return; // rust-analyzer present; the Blocked path isn't exercised here.
    }
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();
    let outcome = db.run_oracle_with_tool(OracleTool::RustAnalyzer, &root.join("o.scip")).unwrap();
    assert!(matches!(outcome, rag_rat_oracle::OracleRunOutcome::Blocked { .. }));
}

/// Run a git command in `root`, panicking on failure — used to make a real committed checkout
/// so `resolve_git_context` returns a non-empty `commit_sha` AND `worktree_id` (the active
/// context every other e2e test misses by running in a non-git temp dir).
fn git(root: &std::path::Path, args: &[&str]) {
    rag_rat_base::test_git::run(root, args);
}

/// Global (un-seeded) ranking — the common assertion shape: no explicit seed, no auto-diff.
fn global_ranking(db: &IndexDatabase) -> Vec<rag_rat_query::pagerank::SymbolImportance> {
    db.important_symbols(ImportantSymbolsRequest {
        limit: 20,
        personalize: Vec::new(),
        auto_seed_from_diff: false,
    })
    .unwrap()
    .symbols
}

/// A real committed git checkout where `caller` calls `target`, plus an UNCOMMITTED edit that
/// adds `fn touched()` to a second file — so `git_changed_paths` reports a non-empty diff with
/// an indexed symbol. `touched` CALLS `placeholder` so it is an endpoint of a resolved edge,
/// i.e. an actual node in the PageRank graph — a seed must reach the graph to personalize the
/// ranking (an isolated changed symbol now correctly falls back to global; see
/// `seed_resolving_only_to_isolated_symbols_is_labeled_global`). Returns the db; `touched.rs`
/// is the changed file.
fn checkout_with_dirty_indexed_symbol() -> (rag_rat_base::test_scratch::ScratchDir, IndexDatabase) {
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    fs::write(root.join("src/touched.rs"), "pub fn placeholder() {}\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    // Edit a tracked file AFTER commit → an uncommitted working-tree change with a real symbol
    // that participates in the graph (touched → placeholder).
    fs::write(
        root.join("src/touched.rs"),
        "pub fn placeholder() {} pub fn touched() { placeholder(); }\n",
    )
    .unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();
    (root, db)
}

/// Name/path/id seed resolution: a valid name resolves to a personalized ranking; a missing
/// name is skipped (counted, not fatal); an all-missing seed falls through to global WITH a
/// reason.
#[test]
fn explicit_seed_resolves_names_and_skips_misses() {
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A real name + a bogus one: the bogus is skipped (no_symbols += 1), the real one seeds.
    let result = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: vec!["target".to_string(), "does_not_exist_anywhere".to_string()],
            auto_seed_from_diff: false,
        })
        .unwrap();
    assert_eq!(result.mode.label(), "importance relative to your current changes");
    let seed = result.seed_source.expect("explicit seed reports provenance");
    assert_eq!(seed.kind, rag_rat_query::pagerank::SeedKind::Explicit);
    assert_eq!(seed.symbol_seed_count, 1, "only the real name seeded");
    assert_eq!(seed.skipped.no_symbols, 1, "the bogus name is skipped, not fatal");
    assert!(result.reason.is_none());

    // A `sym_<hex>` handle — the id every symbol-returning tool now emits (#149) — resolves to
    // its logical symbol's members and seeds them. A raw numeric rowid is deliberately NOT
    // accepted: the wire dropped `symbol_id`, so a bare number is treated as a name/path.
    let target_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM symbols WHERE name = 'target' LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let handle = rag_rat_query::symbol::logical_for_symbol_id(db.storage.connection(), target_id)
        .unwrap()
        .map(|logical| rag_rat_base::serde_big_id::format_sym_handle(logical.logical_symbol_id))
        .expect("the `target` symbol has a logical handle");
    let by_handle = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: vec![handle],
            auto_seed_from_diff: false,
        })
        .unwrap();
    assert_eq!(by_handle.mode, rag_rat_query::pagerank::ImportanceMode::PersonalizedToChanges);

    // All-missing seed → global fall-through WITH a reason (never silent, never an error).
    let all_missing = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: vec!["nope_a".to_string(), "nope_b".to_string()],
            auto_seed_from_diff: false,
        })
        .unwrap();
    assert_eq!(all_missing.mode, rag_rat_query::pagerank::ImportanceMode::Global);
    assert!(all_missing.reason.is_some(), "all-missing explicit seed reports why it's global");
    assert_eq!(all_missing.seed_source.unwrap().skipped.no_symbols, 2);
}

/// #142 review: a seed that RESOLVES to real symbols which are not endpoints of any resolved
/// edge has no effect on the ranking, so the result must be labeled `Global` (with a reason and
/// `effective_seed_count = 0`), NOT `PersonalizedToChanges` — otherwise the caller is told the
/// ranking is "relative to your changes" when it is actually global.
#[test]
fn seed_resolving_only_to_isolated_symbols_is_labeled_global() {
    let root = temp_root();
    // `caller -> target` is the only edge; `island` is a real symbol with no edges, so it never
    // enters the PageRank graph.
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {} fn island() {}\n")
        .unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let result = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: vec!["island".to_string()],
            auto_seed_from_diff: false,
        })
        .unwrap();
    assert_eq!(
        result.mode,
        rag_rat_query::pagerank::ImportanceMode::Global,
        "an isolated seed yields a global ranking, not a personalized one"
    );
    let seed = result.seed_source.expect("seed provenance is still reported");
    assert_eq!(seed.symbol_seed_count, 1, "the name resolved to exactly one symbol");
    assert_eq!(seed.effective_seed_count, 0, "but that symbol is not a graph node");
    assert_eq!(result.reason.as_deref(), Some("named symbols are not connected in the graph"));

    // Contrast: a connected seed (`target`) genuinely personalizes.
    let connected = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: vec!["target".to_string()],
            auto_seed_from_diff: false,
        })
        .unwrap();
    assert_eq!(connected.mode, rag_rat_query::pagerank::ImportanceMode::PersonalizedToChanges);
    assert_eq!(connected.seed_source.unwrap().effective_seed_count, 1);
}

/// #142 review: auto-seed-from-diff in a source root that is NOT a git worktree must not fail
/// the tool — it is best-effort, so it falls through to a global ranking (with a reason)
/// instead of propagating the git error. (`temp_root()` is deliberately not `git init`-ed.)
#[test]
fn auto_seed_outside_a_git_worktree_falls_back_to_global() {
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let result = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: Vec::new(),
            auto_seed_from_diff: true,
        })
        .unwrap();
    assert_eq!(result.mode, rag_rat_query::pagerank::ImportanceMode::Global);
    assert!(!result.symbols.is_empty(), "the global ranking is still computed");
}

/// A name that matches MULTIPLE in-scope symbols seeds ALL of them — personalization is a
/// teleport SET, so `--personalize Thing` (where `Thing` is a struct plus its impls) biases
/// toward the whole entity, NOT skip-on-ambiguity. This is the Phase 4 UX-bug fix: any type
/// with impls used to resolve to nothing and fall back to global.
#[test]
fn multi_match_name_seeds_all_in_scope_symbols() {
    let root = temp_root();
    // A struct `Thing` plus two `impl Thing` blocks → the bare name `Thing` matches the struct
    // row AND the impl rows (impl blocks carry the type's name), i.e. ≥ 2 symbols share
    // "Thing".
    fs::write(
        root.join("src/lib.rs"),
        "pub struct Thing;\nimpl Thing { pub fn a(&self) {} }\nimpl Thing { pub fn b(&self) {} }\n",
    )
    .unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Sanity: the bare name really does match more than one symbol.
    let match_count: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM symbols WHERE name = 'Thing'", [], |r| r.get(0))
        .unwrap();
    assert!(match_count >= 2, "the struct + its impls all carry the name: {match_count}");

    let result = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: vec!["Thing".to_string()],
            auto_seed_from_diff: false,
        })
        .unwrap();
    // PERSONALIZED, not global: the multi-match name resolved to multiple seeds.
    assert_eq!(result.mode, rag_rat_query::pagerank::ImportanceMode::PersonalizedToChanges);
    let seed = result.seed_source.expect("reports provenance");
    assert_eq!(seed.kind, rag_rat_query::pagerank::SeedKind::Explicit);
    assert!(seed.symbol_seed_count >= 2, "all of the name's in-scope symbols are seeded: {seed:?}");
    assert_eq!(seed.skipped.no_symbols, 0, "a matched name is never counted as a miss");
}

/// Auto-seed maps the git diff to symbols THROUGH the scoped `files` view: a dirty indexed file
/// yields a personalized result whose seed provenance is `git_diff`.
#[test]
fn auto_seed_from_diff_picks_changed_symbols() {
    if std::process::Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let (_root, db) = checkout_with_dirty_indexed_symbol();
    let result = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: Vec::new(),
            auto_seed_from_diff: true,
        })
        .unwrap();
    assert_eq!(result.mode, rag_rat_query::pagerank::ImportanceMode::PersonalizedToChanges);
    let seed = result.seed_source.expect("auto-seed reports provenance");
    assert_eq!(seed.kind, rag_rat_query::pagerank::SeedKind::GitDiff);
    assert!(seed.indexed_paths >= 1, "the dirty indexed file counted: {seed:?}");
    assert!(seed.symbol_seed_count >= 1, "the changed file's symbol seeded: {seed:?}");
}

/// Diff with NO indexed symbols (a changed markdown file only) → global mode, a reason, and the
/// diff counts, NOT a silent fall-through.
#[test]
fn diff_without_symbols_falls_back_to_global_with_reason() {
    if std::process::Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    // The only working-tree change is a markdown file — never indexed as a Rust symbol.
    fs::write(root.join("NOTES.md"), "# notes\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    let result = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: Vec::new(),
            auto_seed_from_diff: true,
        })
        .unwrap();
    assert_eq!(result.mode, rag_rat_query::pagerank::ImportanceMode::Global);
    assert_eq!(result.reason.as_deref(), Some("no symbols found in current diff"));
    assert_eq!(result.diff_paths_with_symbols, Some(0));
    let seed = result.seed_source.expect("a fall-through still reports the diff it tried");
    assert!(seed.changed_paths >= 1, "the markdown change was considered: {seed:?}");
    assert_eq!(seed.symbol_seed_count, 0);
    assert!(!result.symbols.is_empty(), "global ranking still returns the spine");
}

/// Deleted and generated changed paths are counted in `skipped`, not seeded.
#[test]
fn deleted_and_generated_paths_counted_in_skipped() {
    if std::process::Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let root = temp_root();
    fs::create_dir_all(root.join("gen")).unwrap();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    fs::write(root.join("src/doomed.rs"), "pub fn doomed() {}\n").unwrap();
    fs::write(root.join("gen/out.rs"), "pub fn generated_fn() {}\n").unwrap();
    // A config with a Generated target for `gen/` so `out.rs` indexes generated.
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.to_path_buf(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![
            ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
            ResolvedTarget {
                name: "gen".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("gen")],
                include: vec!["gen/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Generated,
            },
        ],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Working-tree changes: delete a tracked file, and edit the generated one.
    fs::remove_file(root.join("src/doomed.rs")).unwrap();
    fs::write(root.join("gen/out.rs"), "pub fn generated_fn() {} pub fn more() {}\n").unwrap();

    let result = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: Vec::new(),
            auto_seed_from_diff: true,
        })
        .unwrap();
    let seed = result.seed_source.expect("reports provenance");
    assert_eq!(seed.skipped.deleted, 1, "the removed file is counted deleted: {seed:?}");
    assert_eq!(seed.skipped.generated, 1, "the generated file is counted generated: {seed:?}");
}

/// Acceptance invariant #1: with a non-empty diff carrying an indexed symbol, MCP defaults
/// (auto-seed ON) ⇒ PERSONALIZED, while CLI defaults (auto-seed OFF) ⇒ GLOBAL. The intentional
/// divergence — easy to "clean up" into uniformity by accident, so it's pinned.
#[test]
fn mcp_auto_seeds_but_cli_stays_global_on_a_nonempty_diff() {
    if std::process::Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let (_root, db) = checkout_with_dirty_indexed_symbol();

    // MCP default: auto_seed_from_diff = true → personalized.
    let mcp = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: Vec::new(),
            auto_seed_from_diff: true,
        })
        .unwrap();
    assert_eq!(
        mcp.mode,
        rag_rat_query::pagerank::ImportanceMode::PersonalizedToChanges,
        "MCP no-personalize + non-empty diff ⇒ personalized"
    );

    // CLI default: auto_seed_from_diff = false → global even with the same non-empty diff.
    let cli = db
        .important_symbols(ImportantSymbolsRequest {
            limit: 20,
            personalize: Vec::new(),
            auto_seed_from_diff: false,
        })
        .unwrap();
    assert_eq!(
        cli.mode,
        rag_rat_query::pagerank::ImportanceMode::Global,
        "CLI no-personalize ⇒ global, even with a non-empty diff"
    );
    assert!(cli.seed_source.is_none(), "CLI global carries no seed provenance");
}

/// #82 P2 regression: `find_callers` with NO oracle data must return the IDENTICAL membership
/// and order as the plain heuristic traversal. The unconditional re-sort in
/// `traverse_with_oracle` demoted `match_tier` to a within-confidence tiebreak and changed
/// truncation membership on EVERY query — including repos with no oracle run, where
/// enrichment is a no-op. The fix only re-sorts when a hop was actually promoted, so with
/// no oracle data the overfetched heuristic order + the caller's `limit` are returned
/// untouched.
#[test]
fn find_callers_without_oracle_matches_heuristic_order() {
    let root = temp_root();
    // Several callers of `target` with differing heuristic confidence, more than `limit` so
    // truncation membership is observable.
    let mut src = String::new();
    for i in 0..8 {
        src.push_str(&format!("fn caller{i}() {{ target(); }} "));
    }
    src.push_str("fn target() {}\n");
    fs::write(root.join("src/lib.rs"), src).unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    // No oracle run at all: enrichment early-returns false, so no re-sort fires.
    let opts = rag_rat_query::graph::GraphTraversalOptions {
        include_unresolved: true,
        ..Default::default()
    };
    let limit = 3;
    let via_oracle_path = db.find_callers_with_options("target", limit, &opts).unwrap();

    // The pre-oracle path: plain heuristic traversal at the SAME limit (what the oracle-aware
    // entry point must collapse to when there's nothing to enrich).
    let grouped = db.graph_options_with_logical_group(&opts).unwrap();
    let heuristic = rag_rat_query::graph::traverse_with_options(
        db.storage.connection(),
        "target",
        true,
        limit,
        &grouped,
    )
    .unwrap();

    let ids = |hops: &[rag_rat_query::graph::GraphHop]| {
        hops.iter().map(|h| h.edge_id).collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&via_oracle_path),
        ids(&heuristic),
        "with no oracle data, find_callers must match the plain heuristic membership + order"
    );
}

/// #82 P0 regression: on a REAL committed git checkout the active context is
/// `(commit_sha = HEAD, worktree_id = root)` — BOTH non-empty — and the indexed files are
/// `FileScope::commit` rows `(HEAD, '')`. The old oracle scope predicate
/// `files.commit_sha = ?sha AND files.worktree_id = ?wt` matched ZERO such rows, so `oracle
/// run` silently wrote 0 verdicts and the `Compiler` tier never surfaced. This test commits
/// the checkout, runs the oracle, and asserts verdicts are written AND `trace_callees`
/// surfaces `compiler` — the exact case the unit harness (`commit=''`) and the non-git e2e
/// tests both degenerate past.
#[test]
fn oracle_surfaces_compiler_tier_on_a_real_git_checkout() {
    if std::process::Command::new("git").arg("--version").output().is_err() {
        return; // no git on PATH — skip rather than fail.
    }
    let root = temp_root();
    fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
    // A real committed checkout: clean tree → files index as `FileScope::commit` (HEAD, '').
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);

    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Sanity: the active context carries BOTH a real commit_sha and a worktree_id, and the file
    // rows are committed-scoped (commit set, worktree empty) — the shape that broke the AND.
    let (active_commit, active_worktree) = crate::index::resolve_git_context(&root);
    assert!(!active_commit.is_empty(), "real checkout has a HEAD commit");
    assert!(!active_worktree.is_empty(), "worktree id is the root path");
    let (file_commit, file_worktree): (String, String) = db
        .storage
        .connection()
        .query_row("SELECT commit_sha, worktree_id FROM files WHERE path = 'src/lib.rs'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert!(!file_commit.is_empty() && file_worktree.is_empty(), "committed-scoped file row");

    let (edge_id, callee_start, callee_end, path) = call_edge(&db);
    let symbol = "scip-rust crate held-mini `target`().";
    let scip = scip_with(&path, callee_start, callee_end, symbol, Some(&path), Some((29, 35)));

    let report = db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();
    assert!(
        report.rows_written >= 1,
        "verdicts must be written on a real git checkout (the #82 P0 wrote 0); got {report:?}"
    );

    let callees = db
        .trace_callees_with_options("caller", 50, &rag_rat_query::graph::GraphTraversalOptions {
            include_unresolved: true,
            ..Default::default()
        })
        .unwrap();
    let hop = callees.iter().find(|h| h.edge_id == edge_id).expect("call edge present");
    assert_eq!(hop.confidence, "compiler", "Compiler tier must surface on a real git checkout");
    assert_eq!(hop.resolution_reason.as_deref(), Some("scip:rust-analyzer@v-test"));
}
