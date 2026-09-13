use super::*;

/// A synthetic per-file definition has no extent, so it cannot identify an in-corpus symbol or
/// prove that the receiver resolves outside the corpus.
#[test]
fn zero_width_definition_produces_no_receiver_verdict() {
    let h = Harness::new();
    let caller = h.add_file("caller.ts", "function caller() { ns.f(); }\n");
    let defs = h.add_file("defs.ts", "function other() {}\n");
    let unrelated = h.add_symbol(defs, "other", 0, 19);
    let edge = h.add_edge_with_kind(caller, "ns", 20, 22, "references_type", "NameOnly", None);

    let file_symbol = "scip-typescript npm demo 1.0 `defs.ts`/";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.ts".to_string(),
            occurrences: vec![occurrence(0, 20, 22, file_symbol, SymbolRole::Import as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.ts".to_string(),
        occurrences: vec![occurrence(0, 0, 0, file_symbol, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });

    let report = run_oracle(
        &h.conn,
        OracleTool::ScipTypescript,
        VERSION,
        CHECKOUT,
        &index.write_to_bytes().unwrap(),
        h.root(),
        None,
        None,
    )
    .unwrap();

    let verdict = h.verdict(edge);
    assert!(
        verdict.is_none(),
        "a 0..0 definition must produce no receiver verdict, not resolve to unrelated symbol \
         {unrelated}: {verdict:?}"
    );
    assert_eq!(report.upgraded, 0, "the unrelated symbol must not become an upgrade");
    assert_eq!(
        report.resolved_external, 0,
        "an in-corpus file's synthetic definition is not evidence of an external target"
    );
}
