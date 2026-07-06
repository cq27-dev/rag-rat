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
}
