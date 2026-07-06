use super::join::{self, names_external_package, package_of};
use super::store::SymbolSpan;

// ---------------------------------------------------------------------------
// join.rs — package extraction, def-range → symbol overlap mapping.
// ---------------------------------------------------------------------------

/// `package_of` / `names_external_package` extract the crate/package name from a package-bearing
/// SCIP symbol and return `None`/false for a local symbol.
#[test]
fn package_extraction_from_scip_symbol() {
    let external = "scip-rust cargo tokio 1.0 `spawn`().";
    assert_eq!(package_of(external).as_deref(), Some("tokio"));
    assert!(names_external_package(external));

    // A `local …` symbol has no package component.
    assert_eq!(package_of("local 0"), None);
    assert!(!names_external_package("local 0"));
}

/// `map_definition_to_symbol` picks the tightest span that REALLY contains the whole definition
/// range (a method beats its enclosing impl) and returns `None` when no span contains it.
#[test]
fn map_definition_to_symbol_prefers_tightest_span() {
    let spans = vec![
        SymbolSpan { symbol_id: 1, start_byte: 0, end_byte: 100 }, // enclosing impl
        SymbolSpan { symbol_id: 2, start_byte: 10, end_byte: 20 }, // tight method
    ];
    // Def 12..16 is contained by both → the tighter (id 2) wins.
    assert_eq!(join::map_definition_to_symbol(&spans, 12, 16), Some(2));
    // Def 50..50 is past the tight method's end (20) → only the enclosing impl contains it.
    assert_eq!(join::map_definition_to_symbol(&spans, 50, 50), Some(1));
    // Def 200..200 is past every span → no containment.
    assert_eq!(join::map_definition_to_symbol(&spans, 200, 200), None);
}

/// REAL containment: a symbol span whose `end_byte` falls BEFORE the definition must NOT match,
/// even though its `start_byte <= def_start`. This is the corrupting case the old
/// `end_byte.max(def_end)` predicate got wrong — a short helper preceding the real target would win
/// via `min_by_key` and record the verdict against the WRONG symbol. The fix requires `def_end <=
/// span.end_byte`.
#[test]
fn map_definition_to_symbol_requires_real_containment_not_just_start() {
    let spans = vec![
        // A short helper that ENDS before the definition (10..20), but starts before def_start.
        SymbolSpan { symbol_id: 1, start_byte: 10, end_byte: 20 },
        // The real enclosing target that actually contains the def (0..100).
        SymbolSpan { symbol_id: 2, start_byte: 0, end_byte: 100 },
    ];
    // Def 50..60 is contained ONLY by id 2; the preceding helper (ends at 20) must be rejected even
    // though 10 <= 50. Under the old `max(def_end)` predicate the tighter helper (10..20) wrongly
    // won. Now only id 2 contains it.
    assert_eq!(join::map_definition_to_symbol(&spans, 50, 60), Some(2));

    // A def whose END spills past the only candidate span (30..40, def 35..50) is NOT contained —
    // partial overlap is not containment, so it returns None rather than a wrong match.
    let one = vec![SymbolSpan { symbol_id: 5, start_byte: 30, end_byte: 40 }];
    assert_eq!(join::map_definition_to_symbol(&one, 35, 50), None);
    // The same span DOES contain a def that fits entirely inside it.
    assert_eq!(join::map_definition_to_symbol(&one, 32, 38), Some(5));
}
