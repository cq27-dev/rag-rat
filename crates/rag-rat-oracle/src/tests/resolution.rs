use super::*;

/// A def inside the corpus upgrades an unresolved edge; the `edge_oracle` row is written and the
/// heuristic `edges` row is untouched.
#[test]
fn def_inside_corpus_upgrades_unresolved_edge() {
    let h = Harness::new();
    // `caller.rs` calls `target` defined in `defs.rs`.
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    // `target` definition spans bytes 3..9 ("target") in defs.rs.
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    // The callee identifier `target` in caller.rs is at bytes 14..20.
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    let same = "scip-rust crate v1 `target`().";
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        // Reference occurrence at the call site: chars 14..20 on line 0 (ASCII → bytes).
        occurrence(0, 14, 20, same, SymbolRole::UnspecifiedSymbolRole as i32),
    ]);
    // Definition occurrence lives in defs.rs.
    let mut full = Index::parse_from_bytes(&bytes).unwrap();
    full.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![occurrence(0, 3, 9, same, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = full.write_to_bytes().unwrap();

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let (kind, resolved, _scip) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::Upgrade.as_db_str());
    assert_eq!(resolved, Some(target_sym));
    // Heuristic row untouched.
    assert_eq!(h.heuristic_resolution(edge), ("unresolved".to_string(), None));
}

/// A def outside the corpus resolves to `resolved-external(<package>)`.
#[test]
fn def_outside_corpus_resolves_external() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { spawn(); }\n");
    let edge = h.add_edge(caller, "spawn", 14, 19, "NameOnly", None);

    // A SCIP symbol with a package component, no definition occurrence in the corpus → external.
    let external = "scip-rust cargo tokio 1.0 `spawn`().";
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 14, 19, external, SymbolRole::UnspecifiedSymbolRole as i32),
    ]);

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let (kind, resolved, scip) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::ResolvedExternal.as_db_str());
    assert_eq!(resolved, None);
    assert_eq!(scip, external);
}

/// The position-encoding correctness test: a non-ASCII identifier resolves correctly under BOTH
/// UTF-8 and UTF-16 `position_encoding`. The multibyte prefix shifts the byte offset away from the
/// char offset; only correct per-encoding conversion lands on the identifier.
#[test]
fn non_ascii_identifier_resolves_under_both_encodings() {
    for encoding in [
        PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
        PositionEncoding::UTF16CodeUnitOffsetFromLineStart,
    ] {
        let h = Harness::new();
        // `café` is the receiver prefix: 'é' is 2 UTF-8 bytes (1 UTF-16 unit). The call is
        // `café.naïve()`. We want the callee identifier `naïve`.
        //   bytes:  c a f é(2) . n a ï(2) v e ( ) ;
        //   "café." = c(1)a(1)f(1)é(2).(1) = 6 bytes → `naïve` starts at byte 6.
        //   `naïve` = n a ï(2) v e = 6 bytes → ends at byte 12.
        let src = "café.naïve();\n";
        let caller = h.add_file("caller.rs", src);
        let defs = h.add_file("defs.rs", "fn naïve() {}\n");
        // `naïve` definition in defs.rs: "fn " = 3 bytes, then `naïve` (6 bytes) → 3..9.
        let target_sym = h.add_symbol(defs, "naïve", 3, 9);
        let edge = h.add_edge(caller, "naïve", 6, 12, "NameOnly", None);

        // Char offsets of `naïve` on line 0 differ by encoding:
        //   UTF-8 : "café." = c,a,f,é(2),. = 6 code units → start 6; `naïve` = 6 units → end 12.
        //   UTF-16: "café." = c,a,f,é(1),. = 5 code units → start 5; `naïve` (ï=1) = 5 units → 10.
        let (start_char, end_char) = match encoding {
            PositionEncoding::UTF8CodeUnitOffsetFromLineStart => (6, 12),
            PositionEncoding::UTF16CodeUnitOffsetFromLineStart => (5, 10),
            _ => unreachable!(),
        };
        let (def_start, def_end) = match encoding {
            PositionEncoding::UTF8CodeUnitOffsetFromLineStart => (3, 9),
            PositionEncoding::UTF16CodeUnitOffsetFromLineStart => (3, 8),
            _ => unreachable!(),
        };

        let symbol = "scip-rust crate v1 `naïve`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                occurrences: vec![occurrence(
                    0,
                    start_char,
                    end_char,
                    symbol,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(encoding),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "defs.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                def_start,
                def_end,
                symbol,
                SymbolRole::Definition as i32,
            )],
            position_encoding: EnumOrUnknown::new(encoding),
            ..Default::default()
        });
        let bytes = index.write_to_bytes().unwrap();

        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

        let (kind, resolved, _) =
            h.verdict(edge).unwrap_or_else(|| panic!("verdict written for encoding {encoding:?}"));
        assert_eq!(
            kind,
            OracleResolutionKind::Upgrade.as_db_str(),
            "encoding {encoding:?}: expected in-corpus upgrade"
        );
        assert_eq!(resolved, Some(target_sym), "encoding {encoding:?}: wrong symbol");
    }
}

/// `local N` symbols are skipped entirely — they never produce a verdict.
#[test]
fn local_symbols_are_skipped() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { helper(); }\n");
    let edge = h.add_edge(caller, "helper", 14, 20, "NameOnly", None);

    // A `local 0` occurrence covers the call site — must be ignored.
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 14, 20, "local 0", SymbolRole::UnspecifiedSymbolRole as i32),
    ]);

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    assert!(h.verdict(edge).is_none(), "local symbol must not produce a verdict");
}

/// An Exact/Syntactic edge the oracle contradicts is recorded as a disagreement; the heuristic
/// `edges` row is unchanged.
#[test]
fn exact_edge_contradiction_recorded_not_applied() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\nfn other() {}\n");
    // Two candidate symbols: heuristic resolved to `wrong`, oracle says `right`.
    let wrong_sym = h.add_symbol(defs, "other", 18, 23);
    let right_sym = h.add_symbol(defs, "target", 3, 9);
    // Heuristic edge is Exact → resolved to the WRONG symbol.
    let edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(wrong_sym));

    let symbol = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                14,
                20,
                symbol,
                SymbolRole::UnspecifiedSymbolRole as i32,
            )],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        // Definition of `target` at bytes 3..9.
        occurrences: vec![occurrence(0, 3, 9, symbol, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::Contradict.as_db_str());
    assert_eq!(resolved, Some(right_sym), "oracle resolved to the correct symbol");
    // Heuristic row STILL points at the wrong symbol — never auto-applied.
    assert_eq!(h.heuristic_resolution(edge), ("exact".to_string(), Some(wrong_sym)));
}

/// An Exact edge the oracle agrees with is recorded as a confirmation (the precision signal).
#[test]
fn exact_edge_agreement_recorded_as_confirm() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));

    let symbol = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                14,
                20,
                symbol,
                SymbolRole::UnspecifiedSymbolRole as i32,
            )],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![occurrence(0, 3, 9, symbol, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::Confirm.as_db_str());
    assert_eq!(resolved, Some(target_sym));
    assert_eq!(report.confirmed, 1);
}

/// Decl-vs-def is a CONFIRM, not a contradiction. C/C++ index a function's prototype declaration
/// and its definition as separate concrete symbols (`parser.rs`: `function_definition` +
/// `declaration` with a `function_declarator`). The heuristic may bind a call to the declaration
/// row while the oracle maps `scip-clang`'s definition occurrence to the definition row — same
/// function, two concrete `symbol_id`s under one `logical_symbol_id`. Comparing concrete ids alone
/// scored this as a contradiction and ~halved measured precision (#61 follow-up); the join now
/// folds to the logical symbol, so this is a confirm.
#[test]
fn decl_and_def_of_same_logical_symbol_is_confirm_not_contradiction() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    // The declaration (a prototype, e.g. in a header) and the definition are SEPARATE concrete
    // symbols, both named `target`, grouped under one logical symbol.
    let decl_file = h.add_file("target.h", "fn target();\n");
    let def_file = h.add_file("target.c", "fn target() {}\n");
    let decl_sym = h.add_symbol(decl_file, "target", 3, 9);
    let def_sym = h.add_symbol(def_file, "target", 3, 9);
    h.add_logical_symbol(1000, "target.c", "target", "target", def_sym);
    h.conn
        .execute(
            "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, cfg_expr, \
             signature_hash, start_line, end_line) VALUES (1000, ?1, NULL, NULL, 1, 1)",
            params![decl_sym],
        )
        .unwrap();
    // Heuristic is Exact but resolved to the DECLARATION row (the wrong concrete row, right fn).
    let edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(decl_sym));

    let symbol = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                14,
                20,
                symbol,
                SymbolRole::UnspecifiedSymbolRole as i32,
            )],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    // The compiler's DEFINITION occurrence lands in target.c → maps to the definition row.
    index.documents.push(Document {
        relative_path: "target.c".to_string(),
        occurrences: vec![occurrence(0, 3, 9, symbol, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(
        kind,
        OracleResolutionKind::Confirm.as_db_str(),
        "decl-row heuristic vs def-row oracle of the SAME logical function must confirm"
    );
    assert_eq!(resolved, Some(def_sym), "oracle resolves to the definition row");
    assert_eq!(report.confirmed, 1);
    assert_eq!(report.contradicted, 0, "same logical symbol is not a contradiction");
}
