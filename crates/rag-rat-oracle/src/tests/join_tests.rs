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

/// A reference occurrence, used where only the range and the symbol matter.
fn occurrence(start_byte: usize, end_byte: usize, symbol: &str) -> super::scip::ScipOccurrence {
    super::scip::ScipOccurrence {
        start_byte,
        end_byte,
        symbol: symbol.to_string(),
        is_definition: false,
        is_import: false,
    }
}

/// The same, carrying the `Definition` role.
fn definition(start_byte: usize, end_byte: usize, symbol: &str) -> super::scip::ScipOccurrence {
    super::scip::ScipOccurrence { is_definition: true, ..occurrence(start_byte, end_byte, symbol) }
}

/// The tightest containing occurrence wins, so a segment's own occurrence is not lost to the item
/// that encloses it.
///
/// The extractor writes the token it saw — for `Type::method` that is the whole path — while SCIP
/// records each segment separately. Taking the first containing occurrence handed such an edge to
/// the enclosing definition, which then read as the compiler naming a different target.
#[test]
fn containing_occurrence_prefers_the_tightest_span() {
    let occurrences = vec![
        occurrence(0, 100, "rust-analyzer cargo demo 0.1 `Enclosing`#"),
        occurrence(10, 20, "rust-analyzer cargo demo 0.1 `Tight`#"),
    ];
    let picked =
        join::find_containing_occurrence(&occurrences, 12, 16).expect("a containing occurrence");
    assert_eq!(picked.symbol, "rust-analyzer cargo demo 0.1 `Tight`#", "the tighter span wins");

    // Past the tight span's end, only the enclosing one contains the token.
    let picked =
        join::find_containing_occurrence(&occurrences, 50, 60).expect("a containing occurrence");
    assert_eq!(picked.symbol, "rust-analyzer cargo demo 0.1 `Enclosing`#");

    assert!(
        join::find_containing_occurrence(&occurrences, 200, 210).is_none(),
        "nothing contains it"
    );
}

/// A namespace WIDER than the token is the module the token sits inside, not the referent, so a
/// span SCIP has no type or callable occurrence for reports no evidence rather than a disagreement.
///
/// Letting such a match be judged turned every span-width mismatch into a contradiction against a
/// symbol no heuristic arm could produce (#1223). The exactly-bounding case is the referent and is
/// covered separately.
#[test]
fn a_namespace_occurrence_is_never_the_compilers_answer() {
    let module_only = vec![occurrence(0, 500, "rust-analyzer cargo demo 0.1 commands/clones/")];
    assert!(
        join::find_containing_occurrence(&module_only, 100, 122).is_none(),
        "a module match is the absence of evidence, not the compiler naming a target",
    );

    // It must not shadow a real occurrence either, even when the namespace is the tighter span.
    let with_type = vec![
        occurrence(0, 500, "rust-analyzer cargo demo 0.1 `Wanted`#"),
        occurrence(90, 130, "rust-analyzer cargo demo 0.1 commands/clones/"),
    ];
    let picked =
        join::find_containing_occurrence(&with_type, 100, 122).expect("the type occurrence");
    assert_eq!(picked.symbol, "rust-analyzer cargo demo 0.1 `Wanted`#");
}

/// A namespace that EXACTLY bounds the token is the referent, not the module the token sits in.
///
/// TypeScript emits a `references_type` edge for `ns.f()` whose callee range is the receiver node
/// itself, so for `import * as ns` the occurrence at that token is the namespace symbol — and this
/// crate indexes those. Width is the discriminator: a module spanning far past the token is
/// context, a module bounding it exactly is the answer.
#[test]
fn a_namespace_bounding_the_token_exactly_is_the_referent() {
    let receiver = vec![occurrence(100, 102, "scip-typescript npm demo 1.0 `src/m.ts`/ns/")];
    let picked = join::find_containing_occurrence(&receiver, 100, 102)
        .expect("a namespace naming the token itself still answers");
    assert_eq!(picked.symbol, "scip-typescript npm demo 1.0 `src/m.ts`/ns/");
}

/// A containing REFERENCE outranks a containing definition of any width, so a call site sitting
/// inside a definition's range is not judged against that definition. Tightest decides only within
/// a pass.
#[test]
fn a_reference_outranks_a_tighter_definition() {
    let occurrences = vec![
        occurrence(0, 100, "rust-analyzer cargo demo 0.1 `WideRef`#"),
        definition(10, 20, "rust-analyzer cargo demo 0.1 `TightDef`#"),
    ];
    let picked =
        join::find_containing_occurrence(&occurrences, 12, 16).expect("a containing occurrence");
    assert_eq!(
        picked.symbol, "rust-analyzer cargo demo 0.1 `WideRef`#",
        "a reference wins on role"
    );

    // With no reference containing the token, the definition is still selected — the
    // self-referential edge.
    let only_definition = vec![definition(10, 20, "rust-analyzer cargo demo 0.1 `TightDef`#")];
    let picked = join::find_containing_occurrence(&only_definition, 12, 16)
        .expect("a definition is the fallback, not excluded");
    assert_eq!(picked.symbol, "rust-analyzer cargo demo 0.1 `TightDef`#");
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
