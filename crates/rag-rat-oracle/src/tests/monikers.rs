use super::*;
// ---------------------------------------------------------------------------
// Moniker anchors (#70, phase 3): oracle-run moniker pass + memory relocation.
// ---------------------------------------------------------------------------
/// A definition that maps to an in-corpus symbol writes that logical symbol's moniker; a
/// definition in a document rag-rat never indexed writes nothing.
#[test]
fn oracle_run_writes_monikers_for_in_corpus_defs() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    // A dependency source the `.scip` covers but rag-rat never indexed (on disk, no `files` row).
    std::fs::write(h.root().join("dep.rs"), "fn external_fn() {}\n").unwrap();

    let bytes = scip_bytes_docs(vec![
        // `target` identifier at line 0, chars 3..9.
        ("defs.rs", vec![occurrence(0, 3, 9, TARGET_MONIKER, SymbolRole::Definition as i32)]),
        ("dep.rs", vec![occurrence(
            0,
            3,
            14,
            "rust-analyzer cargo dep 1.0.0 external_fn().",
            SymbolRole::Definition as i32,
        )]),
    ]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    assert_eq!(report.monikers_written, 1, "only the in-corpus def writes a moniker");
    let (moniker, tool, tool_version) = h.moniker(1001).expect("moniker row written");
    assert_eq!(moniker, TARGET_MONIKER);
    assert_eq!(tool, TOOL.as_db_str());
    assert_eq!(tool_version, VERSION);
    let total: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM logical_symbol_monikers", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 1, "the unindexed dep def must not write a row");
}

/// An oracle run persists `index.external_symbols` into the `external_symbols` table (#114): the
/// raw moniker (so it exact-joins `edge_oracle`), the kind/signature/docs, and the derived
/// deprecation flag. A re-run is authoritative — a moniker the new `.scip` drops is cleared.
#[test]
fn oracle_run_persists_external_symbols_and_reclears_them() {
    use ::protobuf::{EnumOrUnknown, Message, MessageField};
    use ::scip::types::symbol_information::Kind;
    use ::scip::types::{Document, Index, Signature, SymbolInformation};

    let h = Harness::new();
    // A trivial in-corpus file gives the run a checkout to scope to.
    h.add_file("caller.rs", "fn caller() {}\n");

    let deprecated = "rust-analyzer cargo dep 1.0.0 old_api().";
    let current = "rust-analyzer cargo dep 1.0.0 new_api().";
    let scip_with = |symbols: Vec<SymbolInformation>| {
        Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            external_symbols: symbols,
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()
    };
    let external = |moniker: &str, docs: Vec<String>, sig: &str| SymbolInformation {
        symbol: moniker.to_string(),
        display_name: "api".to_string(),
        kind: EnumOrUnknown::new(Kind::Function),
        documentation: docs,
        signature_documentation: MessageField::some(Signature {
            language: "rust".to_string(),
            text: sig.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };

    let bytes = scip_with(vec![
        external(deprecated, vec!["@deprecated use new_api".to_string()], "fn old_api()"),
        external(current, vec!["The supported entry point.".to_string()], "fn new_api()"),
    ]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.external_symbols_written, 2, "both external contracts persisted");

    // The deprecated contract landed with the raw moniker, kind, signature, and the derived flag.
    let (kind, sig, dep): (String, String, bool) = h
        .conn
        .query_row(
            "SELECT kind, signature_text, deprecated FROM external_symbols WHERE moniker = ?1",
            [deprecated],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(kind, "Function");
    assert_eq!(sig, "fn old_api()");
    assert!(dep, "the @deprecated doc marks the contract deprecated");
    // The clean contract is present and NOT flagged.
    let clean_dep: bool = h
        .conn
        .query_row("SELECT deprecated FROM external_symbols WHERE moniker = ?1", [current], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(!clean_dep);

    // Authoritative re-run: a `.scip` that drops `old_api` clears its stale contract.
    let bytes2 = scip_with(vec![external(current, vec![], "fn new_api()")]);
    let report2 =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes2, h.root(), None, None)
            .unwrap();
    assert_eq!(report2.external_symbols_written, 1);
    let remaining: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM external_symbols", [], |r| r.get(0)).unwrap();
    assert_eq!(remaining, 1, "the dropped moniker's stale contract was cleared");
    let has_old: bool = h
        .conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM external_symbols WHERE moniker = ?1)",
            [deprecated],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!has_old, "old_api no longer described → its contract is gone");
}

/// A re-run is authoritative for the tool: monikers the current `.scip` no longer defines are
/// cleared, not left stale.
#[test]
fn oracle_rerun_clears_prior_monikers_for_tool() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);

    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert!(h.moniker(1001).is_some());

    // Second run: a `.scip` with no definitions at all.
    let empty = scip_bytes_docs(vec![("defs.rs", vec![])]);
    run_oracle(&h.conn, TOOL, "v2", COMMIT, WORKTREE, &empty, h.root(), None, None).unwrap();
    assert!(h.moniker(1001).is_none(), "authoritative clear removed the stale moniker");
}
