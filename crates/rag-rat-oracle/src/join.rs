//! Occurrence → edge join. For each edge candidate carrying a callee identifier byte range, find
//! the SCIP occurrence whose byte range **contains** that token (not line equality — identifiers
//! share lines), resolve its symbol to a definition, and classify the result.
//!
//! Containment, not equality, is deliberate: tree-sitter's callee token range and SCIP's occurrence
//! range need only overlap on the identifier, not match byte-for-byte (SCIP may bound the range
//! slightly differently around generics/paths). We require the occurrence to *contain* the edge's
//! token start, which is robust to those boundary differences while still being per-identifier.
//!
//! Containment alone is too generous to decide by, though, because the ranges that contain a token
//! nest: a segment, the item declaring it, and the module around both. The join takes the TIGHTEST
//! containing occurrence, and admits a namespace one only where it bounds the token exactly — a
//! module merely spanning the token is what the token sits inside, not something a call or a type
//! reference resolves to, so matching one means SCIP had no answer here rather than that it named a
//! different target.

use super::scip::{ScipIndex, ScipOccurrence};
use super::store::SymbolSpan;
use super::{OracleReport, OracleResolutionKind};

/// The classification for one edge candidate after the join — what to write to `edge_oracle`, or
/// `None` when the oracle had nothing to say (no containing occurrence / unresolvable).
#[derive(Debug, Clone)]
pub(crate) struct EdgeVerdict {
    pub(crate) resolved_symbol_id: Option<i64>,
    pub(crate) scip_symbol: String,
    pub(crate) kind: OracleResolutionKind,
    /// The byte range of the occurrence that actually produced this verdict (the one
    /// `find_containing_occurrence` selected — reference-preferred, full containment). The caller
    /// marks EXACTLY this occurrence as covered, so the covered-marking and the verdict can never
    /// disagree on overlapping occurrences (the weaker start-only/first-match `mark_covered`
    /// could, #81 finding 4).
    pub(crate) matched_occurrence: (usize, usize),
}

/// Inputs for classifying one edge against the oracle.
pub(crate) struct JoinInput<'a> {
    /// The edge's callee identifier byte range (`callee_start_byte..callee_end_byte`).
    pub(crate) callee_start_byte: i64,
    pub(crate) callee_end_byte: i64,
    /// The heuristic confidence on the edge (`Exact` / `Syntactic` / `NameOnly` / `Ambiguous`).
    pub(crate) confidence: &'a str,
    /// The heuristic's resolved target symbol id, if any.
    pub(crate) heuristic_symbol_id: Option<i64>,
    /// Occurrences in the edge's source document (byte-keyed).
    pub(crate) occurrences: &'a [ScipOccurrence],
    /// The full parsed index, for definition lookup.
    pub(crate) index: &'a ScipIndex,
    /// Resolver from a definition `(path, byte-range)` to one of our symbol ids.
    pub(crate) resolve_symbol: &'a dyn Fn(&str, usize, usize) -> Option<i64>,
    /// The logical symbol a concrete symbol id belongs to, if grouped (`logical_symbol_members`).
    /// Used to fold a function's prototype declaration and its definition — separate CONCRETE
    /// symbols in C/C++ — into the same identity when comparing the heuristic's target against the
    /// compiler's, so binding a call to the decl vs the def is a `Confirm`, not a `Contradict`.
    pub(crate) logical_symbol_of: &'a dyn Fn(i64) -> Option<i64>,
}

/// Classify one edge candidate. Returns `None` when no occurrence contains the callee token (the
/// `no_occurrence` bucket) or the reference occurrence has no resolvable definition.
pub(crate) fn classify_edge(input: &JoinInput<'_>) -> Option<EdgeVerdict> {
    let token_start = usize::try_from(input.callee_start_byte).ok()?;
    let token_end = usize::try_from(input.callee_end_byte).ok()?;

    // Find the reference occurrence containing the callee token. Prefer non-definition occurrences
    // (a call site is a reference, not a definition); fall back to any containing occurrence.
    let occurrence = find_containing_occurrence(input.occurrences, token_start, token_end)?;
    let scip_symbol = occurrence.symbol.clone();
    let matched_occurrence = (occurrence.start_byte, occurrence.end_byte);

    // Where is this symbol defined?
    let definition = input.index.definitions.get(&scip_symbol);
    let resolved_symbol_id =
        definition.and_then(|def| (input.resolve_symbol)(&def.path, def.start_byte, def.end_byte));

    let kind = match (resolved_symbol_id, definition) {
        // Definition inside the corpus → mapped to one of our symbols.
        (Some(our_symbol), _) => classify_resolved(input, our_symbol),
        // SCIP resolved the callee OUTSIDE the corpus (a definition the corpus doesn't index).
        (None, Some(_)) => classify_external(input),
        // SCIP referenced a symbol it has no definition for: treat package-bearing symbols as
        // external (they name a dependency), drop the rest (no actionable oracle data).
        (None, None) =>
            if names_external_package(&scip_symbol) {
                classify_external(input)
            } else {
                return None;
            },
    };

    Some(EdgeVerdict { resolved_symbol_id, scip_symbol, kind, matched_occurrence })
}

/// For an in-corpus resolution, classify against the heuristic: confirm/contradict when the
/// heuristic already resolved (Exact/Syntactic), upgrade otherwise (unresolved / NameOnly /
/// Ambiguous).
fn classify_resolved(input: &JoinInput<'_>, oracle_symbol_id: i64) -> OracleResolutionKind {
    classify_in_corpus(
        input.confidence,
        input.heuristic_symbol_id,
        oracle_symbol_id,
        input.logical_symbol_of,
    )
}

/// The in-corpus classification over plain values — shared with the LIVE oracle's write path
/// (#534), which resolves a callee to an in-corpus symbol via the LSP definition rather than a
/// SCIP occurrence but must apply the IDENTICAL confirm/contradict/upgrade taxonomy so live and
/// batch verdicts are interchangeable downstream.
pub(crate) fn classify_in_corpus(
    confidence: &str,
    heuristic_symbol_id: Option<i64>,
    oracle_symbol_id: i64,
    logical_symbol_of: &dyn Fn(i64) -> Option<i64>,
) -> OracleResolutionKind {
    if heuristic_resolved_in_corpus(confidence, heuristic_symbol_id) {
        if heuristic_agrees_with_oracle(heuristic_symbol_id, oracle_symbol_id, logical_symbol_of) {
            OracleResolutionKind::Confirm
        } else {
            OracleResolutionKind::Contradict
        }
    } else {
        OracleResolutionKind::Upgrade
    }
}

/// Whether the heuristic's target and the oracle's target are the SAME symbol — by concrete id, or
/// by LOGICAL identity when the concrete ids differ. The logical fallback is what keeps a C/C++
/// function's prototype-declaration row and its definition row from reading as a disagreement: the
/// heuristic may bind a call to the declaration (e.g. via the resolver's `logical_variant` path)
/// while the oracle maps `scip-clang`'s definition occurrence to the definition row — same
/// function, two concrete `symbol_id`s grouped under one `logical_symbol_id`. Comparing concrete
/// ids alone counted those as `Contradict` and roughly halved the measured precision (the
/// confirm/contradict split was ~even because *which* concrete row the resolver returned first was
/// order-dependent, not correctness-dependent). Concrete-equality stays the fast path; the logical
/// lookup only runs when the ids differ.
fn heuristic_agrees_with_oracle(
    heuristic_symbol_id: Option<i64>,
    oracle_symbol_id: i64,
    logical_symbol_of: &dyn Fn(i64) -> Option<i64>,
) -> bool {
    let Some(heuristic_symbol_id) = heuristic_symbol_id else {
        return false;
    };
    if heuristic_symbol_id == oracle_symbol_id {
        return true;
    }
    match (logical_symbol_of(heuristic_symbol_id), logical_symbol_of(oracle_symbol_id)) {
        (Some(heuristic_logical), Some(oracle_logical)) => heuristic_logical == oracle_logical,
        _ => false,
    }
}

/// Classify when SCIP resolves the callee to an EXTERNAL definition (outside the indexed corpus).
///
/// CRITICAL: `resolved-external` is NOT unconditional here. If the heuristic already resolved this
/// edge to an IN-CORPUS symbol (`Exact`/`Syntactic` + `heuristic_symbol_id.is_some()`), SCIP's
/// external resolution *disagrees* with that in-corpus target — the heuristic picked the wrong
/// target (the real callee lives in a dependency) — so this is a **Contradict**, which counts in
/// `confirm + contradict` and lowers precision honestly. `resolved-external` applies only when the
/// heuristic did NOT resolve in-corpus (unresolved / `NameOnly` / `Ambiguous`): there is no
/// in-corpus claim to contradict, just an external placement the heuristic missed.
fn classify_external(input: &JoinInput<'_>) -> OracleResolutionKind {
    if heuristic_resolved_in_corpus(input.confidence, input.heuristic_symbol_id) {
        OracleResolutionKind::Contradict
    } else {
        OracleResolutionKind::ResolvedExternal
    }
}

/// Whether the heuristic already resolved this edge to an in-corpus symbol: an `Exact`/`Syntactic`
/// confidence carrying a concrete `to_symbol_id`. This is the precondition for a confirm/contradict
/// verdict (there is a heuristic claim to agree or disagree with).
fn heuristic_resolved_in_corpus(confidence: &str, heuristic_symbol_id: Option<i64>) -> bool {
    matches!(confidence, "Exact" | "Syntactic") && heuristic_symbol_id.is_some()
}

/// The occurrence whose byte range contains the callee token. Prefers a reference occurrence (the
/// common case for a call site); a definition occurrence only matches a self-referential edge.
///
/// TIGHTEST WINS WITHIN EACH PASS — a containing reference still outranks a containing definition
/// of any width, which is what keeps a call site inside a definition's range from being judged
/// against that definition. Within a pass the width matters: the extractor's callee span is the
/// token it wrote — for `Type::method` that is the whole path, while SCIP records `Type` and
/// `method` as separate occurrences. Neither segment CONTAINS the wider span, so the only
/// containing occurrence left is the enclosing item's or module's definition, which then reads as
/// the compiler's answer for this edge. Taking the first match in iteration order therefore turned
/// a span-width disagreement into a confident wrong verdict; [`map_definition_to_symbol`] below
/// picks the smallest span for the same reason.
///
/// A NAMESPACE OCCURRENCE IS A CANDIDATE ONLY WHEN IT EXACTLY BOUNDS THE TOKEN. Where it is wider,
/// it is the module the token merely sits inside, and a module is not something a call or a type
/// reference resolves to — matching it is the absence of evidence, not the compiler naming a
/// different target, so [`classify_edge`] returns `None` into the `no_occurrence` bucket rather
/// than manufacturing a contradiction against a symbol no heuristic arm could produce (#1223).
///
/// The exact-bounds carve-out is not a softening: a namespace CAN be the referent when the token IS
/// the namespace. TypeScript emits a `references_type` edge for `ns.f()` whose callee range is the
/// receiver node itself, and for `import * as ns` / `namespace Foo` scip-typescript's occurrence at
/// that token is the namespace symbol — which this crate does index. Excluding it there would
/// discard a real verdict, so width is what separates the two cases.
pub(crate) fn find_containing_occurrence(
    occurrences: &[ScipOccurrence],
    token_start: usize,
    token_end: usize,
) -> Option<&ScipOccurrence> {
    let candidate = |occ: &&ScipOccurrence| {
        let exactly_bounds_token = occ.start_byte == token_start && occ.end_byte == token_end;
        occ.start_byte <= token_start
            && token_end <= occ.end_byte
            && occ.start_byte < occ.end_byte
            && (exactly_bounds_token || !super::scip::symbol_is_module(&occ.symbol))
    };
    let tightest = |only_references: bool| {
        occurrences
            .iter()
            .filter(|occ| !only_references || !occ.is_definition)
            .filter(candidate)
            .min_by_key(|occ| occ.end_byte - occ.start_byte)
    };
    tightest(true).or_else(|| tightest(false))
}

/// A SCIP symbol string names an external package when it parses to a symbol carrying a non-empty
/// package component (manager/name/version). `local …` symbols are already filtered upstream.
pub(crate) fn names_external_package(scip_symbol: &str) -> bool {
    package_of(scip_symbol).is_some()
}

/// The package name component of a SCIP symbol (`scip::symbol::parse_symbol`), e.g. the crate name.
/// `None` for local symbols or symbols without a package component.
pub fn package_of(scip_symbol: &str) -> Option<String> {
    let parsed = ::scip::symbol::parse_symbol(scip_symbol).ok()?;
    let package = parsed.package.as_ref()?;
    let name = package.name.trim();
    if name.is_empty() { None } else { Some(name.to_string()) }
}

/// Map a SCIP definition byte-range to one of our symbols in `spans` by REAL containment: the
/// symbol whose `[start_byte, end_byte]` actually contains the *whole* definition range
/// (`span.start_byte <= def_start && def_end <= span.end_byte`), preferring the tightest (smallest)
/// such span (so a method beats its enclosing impl). Returns the symbol id or `None`.
///
/// Containment is over the full def range — NOT just `def_start` — on purpose: a symbol whose span
/// ends *before* the definition (a short helper preceding the real target) must NOT win. The
/// earlier `def_start < span.end_byte.max(def_end)` predicate let exactly that happen — a large
/// `def_end` pulled any span starting before `def_start` into the candidate set, so a preceding
/// helper could be picked by `min_by_key`, recording the verdict against the WRONG symbol and
/// corrupting precision/recall. Requiring `def_end <= span.end_byte` makes "contains" mean
/// contains.
pub(crate) fn map_definition_to_symbol(
    spans: &[SymbolSpan],
    def_start: usize,
    def_end: usize,
) -> Option<i64> {
    let def_start = i64::try_from(def_start).ok()?;
    let def_end = i64::try_from(def_end).ok()?;
    spans
        .iter()
        .filter(|span| span.start_byte <= def_start && def_end <= span.end_byte)
        .min_by_key(|span| span.end_byte - span.start_byte)
        .map(|span| span.symbol_id)
}

/// Tally one verdict into the report's counters. Returns whether a row should be written
/// (every verdict writes a row; the `None` no-op is handled by the caller).
pub(crate) fn tally(report: &mut OracleReport, kind: OracleResolutionKind) {
    match kind {
        OracleResolutionKind::Upgrade => report.upgraded += 1,
        OracleResolutionKind::ResolvedExternal => report.resolved_external += 1,
        OracleResolutionKind::Confirm => report.confirmed += 1,
        OracleResolutionKind::Contradict => report.contradicted += 1,
    }
}
