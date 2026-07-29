use std::path::Path;

use super::*;

/// `scip::stabilize_moniker_version` pins a scip-typescript local moniker's version to `_` so a
/// `package.json` version bump doesn't churn it (the basis for moniker-anchored memory relocation),
/// while leaving every other tool's symbols — and unparsable / package-less / local ones —
/// untouched.
#[test]
fn stabilize_moniker_version_pins_typescript_package_version() {
    use super::scip::stabilize_moniker_version as norm;

    // The version (3rd package field) is rewritten to `_`; name + descriptors are preserved.
    let v1 = "scip-typescript npm tsmon 9.9.9 `src/a.ts`/greet().";
    let v2 = "scip-typescript npm tsmon 9.9.10 `src/a.ts`/greet().";
    let normed = norm(OracleTool::ScipTypescript, v1);
    assert_eq!(normed, "scip-typescript npm tsmon _ `src/a.ts`/greet().");
    // Two different package versions normalize to the SAME moniker — the relocation invariant.
    assert_eq!(norm(OracleTool::ScipTypescript, v1), norm(OracleTool::ScipTypescript, v2));
    // Already `_` is borrowed unchanged (idempotent).
    let already = "scip-typescript npm tsmon _ `src/a.ts`/greet().";
    assert!(matches!(norm(OracleTool::ScipTypescript, already), std::borrow::Cow::Borrowed(_)));

    // Other tools pass through verbatim — scip-python already pins via `--project-version _`.
    assert_eq!(norm(OracleTool::ScipPython, v1), v1);
    assert_eq!(norm(OracleTool::RustAnalyzer, v1), v1);
    // A local symbol (no package) is left alone.
    let local = "local 42";
    assert_eq!(norm(OracleTool::ScipTypescript, local), local);
}

/// The per-tool default position encoding for SCIP documents that leave the field unset:
/// scip-typescript and scip-java emit UTF-16 columns (confirmed empirically), the rest stay
/// Unspecified.
#[test]
fn default_position_encoding_is_utf16_for_typescript_and_java() {
    use ::scip::types::PositionEncoding::{
        UTF16CodeUnitOffsetFromLineStart as U16, UnspecifiedPositionEncoding as UNSPEC,
    };
    assert_eq!(OracleTool::ScipTypescript.default_position_encoding(), U16);
    assert_eq!(OracleTool::ScipJava.default_position_encoding(), U16);
    assert_eq!(OracleTool::RustAnalyzer.default_position_encoding(), UNSPEC);
    assert_eq!(OracleTool::ScipClang.default_position_encoding(), UNSPEC);
    assert_eq!(OracleTool::ScipPython.default_position_encoding(), UNSPEC);
    assert_eq!(OracleTool::RaLsp.default_position_encoding(), UNSPEC);
    assert_eq!(OracleTool::TsLsp.default_position_encoding(), UNSPEC);
    assert_eq!(OracleTool::ClangdLsp.default_position_encoding(), UNSPEC);
}

/// #534/#536: a live tool is NEVER batch-capable — every batch driver (the auto-run loop, the init
/// wizard, `produce_scip_with_tool`) must gate it out via the discriminator, and
/// `batch_moniker_source` names the batch tool whose monikers the live writer copies.
#[test]
fn live_tools_are_gated_out_of_the_batch_paths() {
    let live = [OracleTool::RaLsp, OracleTool::TsLsp, OracleTool::ClangdLsp];
    for &tool in OracleTool::ALL {
        assert_eq!(tool.batch_capable(), !live.contains(&tool), "{tool:?}");
    }
    assert_eq!(OracleTool::RaLsp.batch_moniker_source(), Some(OracleTool::RustAnalyzer));
    assert_eq!(OracleTool::TsLsp.batch_moniker_source(), Some(OracleTool::ScipTypescript));
    assert_eq!(OracleTool::ClangdLsp.batch_moniker_source(), Some(OracleTool::ScipClang));
    for tool in OracleTool::ALL.iter().filter(|t| t.batch_capable()) {
        assert_eq!(tool.batch_moniker_source(), None, "{tool:?}");
    }

    // `produce_scip_with_tool` on a live tool returns the documented Blocked hint and never
    // builds a `scip_command` (which would be an unreachable! panic).
    for (tool, program) in
        [(OracleTool::RaLsp, "rust-analyzer"), (OracleTool::TsLsp, "typescript-language-server")]
    {
        let outcome =
            produce_scip_with_tool(tool, Path::new("/tmp"), Path::new("/tmp/x.scip")).unwrap();
        match outcome {
            ScipProduction::Blocked { tool: blocked, program: blocked_program, hint } => {
                assert_eq!(blocked, tool.as_db_str());
                assert_eq!(blocked_program, program);
                assert!(hint.contains("[oracle.live]"), "{hint}");
            },
            ScipProduction::Produced { .. } => panic!("a live tool must never produce a .scip"),
        }
    }
}

/// Sorting by declared `Authority` reproduces the batch-first order the read-side merge sites used
/// to get from `!batch_capable()`, and every canonical tool precedes every patch tool.
///
/// This is the behaviour-preservation pin for splitting that one predicate into three. It does NOT
/// assert that authority and `batch_capable` agree tool-for-tool — they are independent questions
/// that merely coincide today, and a whole-checkout LSP sweep would separate them. What must hold
/// is the ORDER the merge sites depend on.
#[test]
fn sorting_by_authority_reproduces_the_batch_first_merge_order() {
    let mut by_authority = OracleTool::ALL.to_vec();
    by_authority.sort_by_key(|tool| tool.authority());

    let mut by_old_predicate = OracleTool::ALL.to_vec();
    by_old_predicate.sort_by_key(|tool| !tool.batch_capable());

    assert_eq!(
        by_authority, by_old_predicate,
        "the declared authority must order the merge sites exactly as the predicate did",
    );

    let last_canonical = by_authority
        .iter()
        .rposition(|tool| tool.authority() == Authority::Canonical)
        .expect("some tool is canonical");
    assert!(
        by_authority[..=last_canonical].iter().all(|tool| tool.authority() == Authority::Canonical),
        "no patch tool may sort before a canonical one — that is the whole guarantee",
    );
}

/// Coverage and authority are DECLARED, so every tool has an answer and a new variant cannot
/// inherit one. The exhaustive matches make that a compile error; this pins that they are also
/// reachable and that today every whole-checkout tool is canonical.
///
/// The pairing is asserted as a fact about TODAY, not a rule: a whole-checkout LSP sweep would be
/// `WholeCheckout` + `Patch`, and this test should then be updated rather than treated as a veto.
#[test]
fn every_tool_declares_its_coverage_and_authority() {
    for &tool in OracleTool::ALL {
        let (coverage, authority) = (tool.coverage(), tool.authority());
        assert_eq!(
            coverage == Coverage::WholeCheckout,
            authority == Authority::Canonical,
            "{} pairs coverage {coverage:?} with authority {authority:?}; if that is intentional, \
             update this test — it records today's coincidence, not a constraint",
            tool.as_db_str(),
        );
    }
}
